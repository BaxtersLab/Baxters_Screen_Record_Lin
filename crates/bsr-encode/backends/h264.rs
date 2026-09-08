// SPDX-License-Identifier: MIT
use crate::{EncoderConfig, EncoderError, EncodedPacket};
use ffmpeg_next as ffmpeg;
use std::time::Instant;

pub struct H264EncoderBackend {
    encoder: ffmpeg::encoder::video::Encoder,
    frame: ffmpeg::util::frame::Video,
    scaler: ffmpeg::software::scaling::Context,
    /// Denominator of the encoder time_base (which is 1/fps), used to convert
    /// capture timestamps into pts ticks.
    fps: i64,
    /// Capture timestamp of the first encoded frame; pts is measured from it.
    first_ts: Option<u64>,
    /// Last pts emitted, so pts stays strictly increasing when two frames land
    /// in the same tick or the capture clock steps backwards.
    last_pts: Option<i64>,
}

// Safety: H264EncoderBackend is only used from a single tokio task.
// The raw FFmpeg pointers inside are not shared across threads.
unsafe impl Send for H264EncoderBackend {}
unsafe impl Sync for H264EncoderBackend {}

/// Copy a packed BGRA buffer into a plane whose rows are `dst_stride` bytes apart.
///
/// `src` is packed: row `y` starts at `y * src_stride` with no padding. `dst` is an
/// FFmpeg plane, whose linesize is ALIGNED and therefore usually LARGER than a packed
/// row. Copying `src` into `dst` as one flat memcpy is the bug this function exists to
/// prevent: it puts row `y` at `y * src_stride` instead of `y * dst_stride`, so every
/// row lands progressively earlier and the image shears into diagonal displaced bands.
///
/// Extracted as a free function specifically so it is testable without a codec: the
/// defect is pure arithmetic, and the encoder tests that missed it for the whole
/// project's life used 1280x720 and 1920x1080 -- both already stride-aligned, so
/// `dst_stride == src_stride` and the bug is invisible.
pub(crate) fn copy_packed_into_plane(
    dst: &mut [u8],
    dst_stride: usize,
    src: &[u8],
    src_stride: usize,
    rows: usize,
) -> Result<(), String> {
    if dst_stride < src_stride {
        return Err(format!(
            "destination linesize {dst_stride} is shorter than a {src_stride}-byte row"
        ));
    }
    if src.len() < src_stride * rows {
        return Err(format!(
            "source is {} bytes, need {} for {rows} rows of {src_stride}",
            src.len(),
            src_stride * rows
        ));
    }
    if dst.len() < dst_stride * (rows - 1) + src_stride {
        return Err(format!(
            "destination is {} bytes, too small for {rows} rows at stride {dst_stride}",
            dst.len()
        ));
    }
    for y in 0..rows {
        let d = y * dst_stride;
        let s = y * src_stride;
        dst[d..d + src_stride].copy_from_slice(&src[s..s + src_stride]);
    }
    Ok(())
}

impl H264EncoderBackend {
    pub fn new(config: &EncoderConfig) -> Result<Self, EncoderError> {
        ffmpeg::init().map_err(|e| EncoderError::Initialization(e.to_string()))?;

        let codec = ffmpeg::encoder::find(ffmpeg::codec::Id::H264)
            .ok_or_else(|| EncoderError::Initialization("H.264 codec not found".to_string()))?;

        let context = ffmpeg::codec::Context::new_with_codec(codec);

        let mut video_encoder = context.encoder().video()
            .map_err(|e| EncoderError::Initialization(e.to_string()))?;

        video_encoder.set_width(config.width);
        video_encoder.set_height(config.height);
        video_encoder.set_format(ffmpeg::format::Pixel::YUV420P);
        video_encoder.set_time_base(ffmpeg::Rational::new(1, config.fps as i32));
        video_encoder.set_frame_rate(Some(ffmpeg::Rational::new(config.fps as i32, 1)));
        video_encoder.set_bit_rate(config.bitrate_kbps as usize * 1000);

        let mut opts = ffmpeg::Dictionary::new();
        opts.set("preset", &config.preset);
        opts.set("tune", "zerolatency");

        let encoder = video_encoder.open_with(opts)
            .map_err(|e| EncoderError::Initialization(e.to_string()))?;

        let frame = ffmpeg::util::frame::Video::new(
            ffmpeg::format::Pixel::YUV420P,
            config.width,
            config.height,
        );

        let scaler = ffmpeg::software::scaling::Context::get(
            ffmpeg::format::Pixel::BGRA,
            config.width,
            config.height,
            ffmpeg::format::Pixel::YUV420P,
            config.width,
            config.height,
            ffmpeg::software::scaling::Flags::BILINEAR,
        ).map_err(|e| EncoderError::Initialization(e.to_string()))?;

        Ok(Self {
            encoder,
            frame,
            scaler,
            fps: config.fps.max(1) as i64,
            first_ts: None,
            last_pts: None,
        })
    }

    pub fn initialize(&mut self, _config: &EncoderConfig) -> Result<(), EncoderError> {
        Ok(())
    }

    pub fn encode_frame(&mut self, frame: &crate::CaptureFrame) -> Result<Option<EncodedPacket>, EncoderError> {
        let _start_time = Instant::now();

        // The scaler and the x264 encoder are both opened once, at the configured size,
        // and neither can change size mid-stream. A frame of any other size cannot be
        // encoded -- previously that surfaced as an opaque swscale failure (or, with a
        // smaller frame, a silent partial copy of stale pixels). Say what is wrong.
        //
        // This matters now that the capture region is croppable: the pipeline must be
        // built with the CROPPED dimensions, not the screen's.
        let (want_w, want_h) = (self.frame.width(), self.frame.height());
        if frame.width != want_w || frame.height != want_h {
            return Err(EncoderError::Encoding(format!(
                "frame is {}x{} but this encoder was opened for {}x{}; the capture \
                 region and the encoder configuration disagree",
                frame.width, frame.height, want_w, want_h
            )));
        }

        let mut src_frame = ffmpeg::util::frame::Video::new(
            ffmpeg::format::Pixel::BGRA,
            frame.width,
            frame.height,
        );
        // Copy ROW BY ROW, honouring the destination's linesize.
        //
        // This was a single flat `copy_from_slice(&frame.data)` and it silently sheared
        // every frame whose width was not stride-aligned. FFmpeg allocates each plane with
        // an ALIGNED linesize; `CaptureFrame.data` is packed at exactly `width * 4`. Those
        // are only the same number when the packed row already happens to be aligned:
        //
        //     1920 wide -> packed row 7680 == linesize 7680  -> drift 0     (clean)
        //     1842 wide -> packed row 7368 vs linesize 7424  -> drift 56/row
        //
        // A flat memcpy therefore starts each row a little earlier than the frame expects,
        // and the error accumulates down the image as progressive diagonal displaced bands.
        // Found on the test box 2026-09-07: a 1842x934 crop was corrupted in exactly that
        // pattern while the 1920x1080 baseline was pristine, and the file still decoded with
        // ZERO ffmpeg errors -- because the bitstream was valid, the input to it was not.
        //
        // Note `portal_backend.rs` already reads the real PipeWire stride via
        // `plane_stride()[0]`; this path just was not using the same discipline.
        let src_stride = frame.width as usize * 4;
        let dst_stride = src_frame.stride(0);
        let rows = frame.height as usize;
        copy_packed_into_plane(
            src_frame.data_mut(0), dst_stride, &frame.data, src_stride, rows,
        ).map_err(EncoderError::Encoding)?;

        self.scaler.run(&src_frame, &mut self.frame)
            .map_err(|e| EncoderError::Encoding(e.to_string()))?;

        // pts must track real capture time, not frame count. Capture routinely
        // delivers fewer frames than `fps` — a damage-driven PipeWire stream on
        // a quiet desktop repeats slowly, and a 1080p scale+encode can take
        // longer than one frame interval — and a counter would then write a
        // file shorter than the recording, played back sped up, with no error.
        let base = *self.first_ts.get_or_insert(frame.timestamp);
        let elapsed_ns = frame.timestamp.saturating_sub(base) as i128;
        let mut pts = (elapsed_ns * self.fps as i128 / 1_000_000_000) as i64;

        // Keep pts strictly increasing: two frames can share a tick, and the
        // capture clock is SystemTime, which NTP can step backwards. x264 and
        // the mov muxer both reject non-monotonic timestamps.
        if let Some(last) = self.last_pts {
            if pts <= last {
                pts = last + 1;
            }
        }
        self.last_pts = Some(pts);

        self.frame.set_pts(Some(pts));

        self.encoder.send_frame(&self.frame)
            .map_err(|e| EncoderError::Encoding(e.to_string()))?;

        let mut packet = ffmpeg::Packet::empty();
        match self.encoder.receive_packet(&mut packet) {
            Ok(()) => {
                let data = packet.data().unwrap_or(&[]).to_vec();
                Ok(Some(EncodedPacket {
                    data,
                    timestamp: frame.timestamp,
                    pts: packet.pts().unwrap_or(0),
                    dts: packet.dts().unwrap_or(0),
                    keyframe: packet.is_key(),
                    codec: "h264".to_string(),
                }))
            }
            Err(_) => Ok(None), // Encoder is buffering, no output yet
        }
    }

    pub fn shutdown(&mut self) -> Result<(), EncoderError> {
        self.encoder.send_eof()
            .map_err(|e| EncoderError::Shutdown(e.to_string()))?;

        let mut packet = ffmpeg::Packet::empty();
        while self.encoder.receive_packet(&mut packet).is_ok() {
            // drain
        }

        Ok(())
    }
}