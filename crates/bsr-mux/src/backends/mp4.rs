// SPDX-License-Identifier: MIT
use crate::{MuxerBackend, MuxerError, MuxerResult, EncodedPacket};
use bsr_ipc::FileNamingStrategy;
use ffmpeg_next as ffmpeg;
use ffmpeg_next::ffi;

pub struct Mp4Muxer {
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
        }

        let stream_idx = stream.index();

        output_context.write_header()?;

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
        }

        Ok(())
    }

    async fn finalize(&mut self) -> MuxerResult<()> {
        if let Some(ref mut output_context) = self.output_context {
            output_context.write_trailer()?;
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
    use super::*;
    use tempfile::tempdir;

    #[tokio::test]
    async fn test_mp4_muxer_initialization() {
        let temp_dir = tempdir().unwrap();
        let config = bsr_ipc::MuxerConfig {
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