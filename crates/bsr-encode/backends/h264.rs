use crate::{EncoderConfig, EncoderError, EncodedPacket};
use ffmpeg_next as ffmpeg;
use std::time::Instant;

pub struct H264EncoderBackend {
    encoder: ffmpeg::encoder::video::Encoder,
    frame: ffmpeg::util::frame::Video,
    scaler: ffmpeg::software::scaling::Context,
    pts: i64,
}

// Safety: H264EncoderBackend is only used from a single tokio task.
// The raw FFmpeg pointers inside are not shared across threads.
unsafe impl Send for H264EncoderBackend {}
unsafe impl Sync for H264EncoderBackend {}

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
            pts: 0,
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
        src_frame.data_mut(0)[..frame.data.len()].copy_from_slice(&frame.data);

        self.scaler.run(&src_frame, &mut self.frame)
            .map_err(|e| EncoderError::Encoding(e.to_string()))?;

        self.frame.set_pts(Some(self.pts));
        self.pts += 1;

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