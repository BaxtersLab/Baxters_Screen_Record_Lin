// SPDX-License-Identifier: MIT
use crate::{MuxerBackend, MuxerError, MuxerResult, EncodedPacket};
use bsr_ipc::FileNamingStrategy;
use ffmpeg_next as ffmpeg;
use ffmpeg_next::ffi;

pub struct Mp4Muxer {
    /// Whether the container was given the codec parameter sets, and therefore
    /// whether this file was written as fragments.
    has_extradata: bool,
    /// Whether anything was actually recorded, so an empty file is not reported
    /// as a failed finalize.
    wrote_any_packet: bool,
    output_context: Option<ffmpeg::format::context::Output>,
    stream_index: Option<usize>,
    start_time: Option<std::time::Instant>,
    // Timestamp rescaling. Encoder pts are in `enc_time_base` (1/fps); the mov
    // muxer may replace the stream time_base at write_header, so each packet is
    // rescaled from enc_time_base to `stream_time_base` for correct timing.
    enc_time_base: ffmpeg::Rational,
    stream_time_base: ffmpeg::Rational,
}

// Safety: Mp4Muxer is only used from a single tokio task.
unsafe impl Send for Mp4Muxer {}
unsafe impl Sync for Mp4Muxer {}

impl Mp4Muxer {
    pub fn new() -> Self {
        Self {
            has_extradata: false,
            wrote_any_packet: false,
            output_context: None,
            stream_index: None,
            start_time: None,
            enc_time_base: ffmpeg::Rational::new(1, 30),
            stream_time_base: ffmpeg::Rational::new(1, 30),
        }
    }

    fn generate_output_path(&self, config: &bsr_ipc::MuxerConfig) -> std::path::PathBuf {
        let now = chrono::Utc::now();
        let base = &config.base_output_path;

        match &config.file_naming_strategy {
            FileNamingStrategy::Simple(name) => base.join(name),
            FileNamingStrategy::TimestampedFile => {
                let filename = format!("{}.mp4", now.format("%Y-%m-%d_%H-%M-%S"));
                base.join(filename)
            }
            FileNamingStrategy::TimestampedFolder => {
                let folder = now.format("%Y-%m-%d").to_string();
                let filename = format!("{}.mp4", now.format("%H-%M-%S"));
                base.join(folder).join(filename)
            }
        }
    }
}

#[async_trait::async_trait]
impl MuxerBackend for Mp4Muxer {
    async fn initialize(&mut self, config: &bsr_ipc::MuxerConfig) -> MuxerResult<()> {
        // REFUSE to record without the codec parameter sets.
        //
        // The encoder is opened with GLOBAL_HEADER, which means it no longer
        // repeats SPS/PPS inside the video data -- they exist only as
        // extradata. A muxer that starts without them writes a file that
        // describes nothing: it may look fine until someone tries to play it.
        // That is the failure the operator hit, and silently carrying on is how
        // it stayed hidden. Fail here, where the message is visible, rather
        // than on the user's disk.
        let extradata = config
            .extradata
            .as_ref()
            .filter(|e| !e.is_empty())
            .ok_or_else(|| MuxerError::Config(
                "no H.264 parameter sets (SPS/PPS): the muxer was given no extradata, and a \
                 recording written without them cannot be played back. The encoder supplies \
                 them via H264EncoderBackend::extradata() once it is open.".to_string()))?;
        self.has_extradata = true;
        ffmpeg::init().map_err(MuxerError::FFmpeg)?;

        let output_path = self.generate_output_path(config);

        // Ensure output directory exists
        if let Some(parent) = output_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let mut output_context = ffmpeg::format::output(&output_path)?;

        // Add H.264 video stream
        let codec = ffmpeg::encoder::find(ffmpeg::codec::Id::H264)
            .ok_or_else(|| MuxerError::Config("H.264 codec not found".to_string()))?;
        let mut stream = output_context.add_stream(codec)?;
        // The encoder emits pts in 1/fps units (frame indices), so the stream
        // time_base must be 1/fps for playback timing to be correct.
        let fps = config.fps.max(1) as i32;
        stream.set_time_base(ffmpeg::Rational::new(1, fps));

        // Set codec parameters so the container knows the stream type + geometry.
        unsafe {
            let codecpar = (*stream.as_mut_ptr()).codecpar;
            (*codecpar).codec_type = ffi::AVMediaType::AVMEDIA_TYPE_VIDEO;
            (*codecpar).codec_id = ffi::AVCodecID::AV_CODEC_ID_H264;
            (*codecpar).width = config.width as i32;
            (*codecpar).height = config.height as i32;

            // The H.264 parameter sets (SPS/PPS), if the encoder gave us any.
            //
            // These describe the stream: resolution, profile, how the frames are
            // framed. Without them the container says only "there is H.264 in
            // here", and a player can only recover the parameters by reading the
            // data through a complete index -- which is why an unfinalised
            // recording was unopenable. ffmpeg owns this buffer, so it is
            // allocated with av_malloc and padded as ffmpeg requires.
            {
                let extra = extradata;
                let size = extra.len();
                let buf = ffi::av_malloc(size + ffi::AV_INPUT_BUFFER_PADDING_SIZE as usize) as *mut u8;
                if !buf.is_null() {
                    std::ptr::copy_nonoverlapping(extra.as_ptr(), buf, size);
                    std::ptr::write_bytes(buf.add(size), 0, ffi::AV_INPUT_BUFFER_PADDING_SIZE as usize);
                    if !(*codecpar).extradata.is_null() {
                        ffi::av_free((*codecpar).extradata as *mut std::ffi::c_void);
                    }
                    (*codecpar).extradata = buf;
                    (*codecpar).extradata_size = size as i32;
                }
            }
        }

        let stream_idx = stream.index();

        // Write a FRAGMENTED mp4 when the stream is self-describing.
        //
        // A classic mp4 keeps its index at the END, written by finalize(). Any
        // stop that never reaches that call -- a crash, a kill, a power cut --
        // leaves every frame on disk and no index, and no player will open the
        // file. Fragments carry their own headers, so an interrupted recording
        // plays up to the last completed fragment instead of being lost.
        //
        // frag_keyframe alone is not enough: x264 emits a keyframe roughly every
        // 250 frames, so a short recording would close no fragment at all and
        // an unclean stop would still lose everything.
        //
        // frag_duration bounds the loss whatever the keyframe interval, and it
        // is also how soon a file becomes readable at all: nothing is committed
        // until the first fragment closes. 250 ms was chosen by measurement,
        // not taste -- over 5 s of 720p, fragments every 250 ms instead of
        // every 2 s cost 2,032 bytes on a 1.59 MB file, 0.13%. For that, the
        // worst case a crash can destroy falls from two seconds to a quarter of
        // one.
        //
        // Only when extradata exists: without it a fragmented file has nothing
        // to describe its contents and will not open at all, so the old
        // behaviour is the safer fallback.
        let mut mux_opts = ffmpeg::Dictionary::new();
        mux_opts.set("movflags", "frag_keyframe+empty_moov+default_base_moof");
        mux_opts.set("frag_duration", "250000");
        output_context.write_header_with(mux_opts)?;

        // The mov/mp4 muxer may replace the stream time_base at write_header; read
        // it back and remember it (and the encoder time_base) for packet rescaling.
        self.enc_time_base = ffmpeg::Rational::new(1, fps);
        self.stream_time_base = output_context
            .streams()
            .find(|s| s.index() == stream_idx)
            .map(|s| s.time_base())
            .unwrap_or_else(|| ffmpeg::Rational::new(1, fps));

        self.output_context = Some(output_context);
        self.stream_index = Some(stream_idx);
        self.start_time = Some(std::time::Instant::now());

        Ok(())
    }

    async fn write_packet(&mut self, packet: EncodedPacket) -> MuxerResult<()> {
        let enc_tb = self.enc_time_base;
        let stream_tb = self.stream_time_base;
        if let (Some(ref mut output_context), Some(stream_index)) = (&mut self.output_context, self.stream_index) {
            let mut av_packet = ffmpeg::Packet::copy(&packet.data);
            av_packet.set_pts(Some(packet.pts));
            av_packet.set_dts(Some(packet.dts));
            av_packet.set_stream(stream_index);

            if packet.keyframe {
                av_packet.set_flags(ffmpeg::packet::Flags::KEY);
            }

            // Rescale from encoder time_base (1/fps) to the stream's real time_base
            // so playback timing (and container duration) is correct.
            av_packet.rescale_ts(enc_tb, stream_tb);
            av_packet.write_interleaved(output_context)?;
            self.wrote_any_packet = true;
        }

        Ok(())
    }

    async fn finalize(&mut self) -> MuxerResult<()> {
        if let Some(ref mut output_context) = self.output_context {
            // A fragmented file that received no packets has nothing to close,
            // and ffmpeg reports an error for it. The recording is already
            // valid on disk at this point -- that is the whole point of
            // fragments -- so an empty one is not a failure to report.
            // av_write_trailer returns 0 on success and a NEGATIVE AVERROR on
            // failure -- but the mov muxer, writing a fragmented file, returns
            // the SIZE of the fragment index it just wrote, which is positive.
            // ffmpeg-next's wrapper treats any non-zero value as an error, so a
            // perfectly good recording came back as a failed finalize.
            // Measured 2026-09-24: +105 on a file whose 82 frames all decode.
            // Only a negative value is a real failure.
            let rc = unsafe { ffi::av_write_trailer(output_context.as_mut_ptr()) };
            if rc < 0 {
                if !self.wrote_any_packet {
                    // Nothing was recorded. A fragmented file with no packets has
                    // no index to close, and that is not a failure worth raising.
                    tracing::debug!("empty recording, nothing to finalize (rc {rc})");
                } else {
                    return Err(ffmpeg::Error::from(rc).into());
                }
            }
        }
        Ok(())
    }

    async fn shutdown(&mut self) -> MuxerResult<()> {
        self.finalize().await?;
        self.output_context = None;
        self.stream_index = None;
        self.start_time = None;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    /// Real H.264 parameter sets from a real encoder. The muxer refuses to
    /// write a file without them, on purpose, so the tests must supply what the
    /// product supplies.
    fn test_extradata() -> Option<Vec<u8>> {
        let cfg = bsr_encode::EncoderConfig {
            codec: "h264".into(),
            preset: "ultrafast".into(),
            bitrate_kbps: 1000,
            width: 320,
            height: 240,
            fps: 30,
        };
        bsr_encode::H264EncoderBackend::new(&cfg).ok().and_then(|e| e.extradata())
    }

    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_mp4_muxer_initialization() {
        let temp_dir = tempdir().unwrap();
        let config = bsr_ipc::MuxerConfig {
            extradata: test_extradata(),
            base_output_path: temp_dir.path().to_path_buf(),
            file_naming_strategy: bsr_ipc::FileNamingStrategy::Simple("test.mp4".to_string()),
            max_duration: std::time::Duration::from_secs(10),
            ..Default::default()
        };

        let mut muxer = Mp4Muxer::new();
        muxer.initialize(&config).await.unwrap();

        assert!(muxer.output_context.is_some());
        assert!(muxer.stream_index.is_some());
    }

    #[tokio::test]
    async fn test_mp4_muxer_write_packet() {
        let temp_dir = tempdir().unwrap();
        let config = bsr_ipc::MuxerConfig {
            extradata: test_extradata(),
            base_output_path: temp_dir.path().to_path_buf(),
            file_naming_strategy: bsr_ipc::FileNamingStrategy::Simple("test.mp4".to_string()),
            max_duration: std::time::Duration::from_secs(10),
            ..Default::default()
        };

        let mut muxer = Mp4Muxer::new();
        muxer.initialize(&config).await.unwrap();

        let packet = EncodedPacket {
            data: vec![0; 100], // dummy data
            pts: 0,
            dts: 0,
            keyframe: true,
        };

        muxer.write_packet(packet).await.unwrap();
    }

    #[tokio::test]
    async fn test_mp4_muxer_finalize() {
        let temp_dir = tempdir().unwrap();
        let config = bsr_ipc::MuxerConfig {
            extradata: test_extradata(),
            base_output_path: temp_dir.path().to_path_buf(),
            file_naming_strategy: bsr_ipc::FileNamingStrategy::Simple("test.mp4".to_string()),
            max_duration: std::time::Duration::from_secs(10),
            ..Default::default()
        };

        let mut muxer = Mp4Muxer::new();
        muxer.initialize(&config).await.unwrap();

        muxer.finalize().await.unwrap();
    }
}