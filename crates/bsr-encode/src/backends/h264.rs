use crate::{EncoderBackend, EncoderConfig, EncoderError, EncodedPacket};
use async_trait::async_trait;
use ffmpeg_next as ffmpeg;
use std::time::Instant;

pub struct H264EncoderBackend {
    codec: ffmpeg::codec::encoder::video::Video,
    context: ffmpeg::codec::Context,
    frame: ffmpeg::util::frame::Video,
    scaler: ffmpeg::software::scaling::Context,
    pts: i64,
}

#[async_trait]
impl EncoderBackend for H264EncoderBackend {
    async fn initialize(&mut self, config: &EncoderConfig) -> Result<(), EncoderError> {
        // Initialization is done in new(), so this is a no-op
        Ok(())
    }

    async fn encode_frame(&mut self, frame: crate::CaptureFrame) -> Result<EncodedPacket, EncoderError> {
        let start_time = Instant::now();

        // Create source frame from CaptureFrame data
        let mut src_frame = ffmpeg::util::frame::Video::new(
            ffmpeg::format::Pixel::BGRA,
            frame.width as u32,
            frame.height as u32,
        );
        src_frame.data_mut(0).copy_from_slice(&frame.data);

        // Scale to YUV420P
        self.scaler.run(&src_frame, &mut self.frame)?;

        // Set PTS
        self.frame.set_pts(Some(self.pts));
        self.pts += 1;

        // Send frame to encoder
        self.context.send_frame(&self.frame)?;

        // Receive packet
        let mut packet = ffmpeg::Packet::empty();
        if self.context.receive_packet(&mut packet).is_ok() {
            let data = packet.data().unwrap_or(&[]).to_vec();
            let latency = start_time.elapsed().as_millis() as f32;

            Ok(EncodedPacket {
                data,
                timestamp: frame.timestamp,
                pts: packet.pts().unwrap_or(0),
                dts: packet.dts().unwrap_or(0),
                keyframe: packet.is_key(),
                codec: "h264".to_string(),
            })
        } else {
            Err(EncoderError::Encoding("No packet received".to_string()))
        }
    }

    async fn shutdown(&mut self) -> Result<(), EncoderError> {
        // Send flush frame
        self.context.send_frame(&ffmpeg::util::frame::Video::empty())?;

        // Drain remaining packets
        let mut packet = ffmpeg::Packet::empty();
        while self.context.receive_packet(&mut packet).is_ok() {
            // Optionally handle remaining packets
        }

        Ok(())
    }
}

impl H264EncoderBackend {
    pub fn new(config: &EncoderConfig) -> Result<Self, EncoderError> {
        ffmpeg::init()?;

        let codec = ffmpeg::encoder::find(ffmpeg::codec::Id::H264)
            .ok_or(EncoderError::Initialization("H.264 codec not found".to_string()))?;

        let mut context = ffmpeg::codec::Context::new();
        context.set_codec(codec);
        context.set_width(config.width as u32);
        context.set_height(config.height as u32);
        context.set_format(ffmpeg::format::Pixel::YUV420P);
        context.set_time_base(ffmpeg::Rational::new(1, config.fps as i32));

        if let Some(mut encoder) = context.encoder().video() {
            encoder.set_preset("veryfast");
            encoder.set_tune("zerolatency");
            encoder.open()?;
        } else {
            return Err(EncoderError::Initialization("Failed to create video encoder".to_string()));
        }

        let frame = ffmpeg::util::frame::Video::new(
            ffmpeg::format::Pixel::YUV420P,
            config.width as u32,
            config.height as u32,
        );

        let scaler = ffmpeg::software::scaling::Context::get(
            ffmpeg::format::Pixel::BGRA,
            config.width as u32,
            config.height as u32,
            ffmpeg::format::Pixel::YUV420P,
            config.width as u32,
            config.height as u32,
            ffmpeg::software::scaling::Flags::BILINEAR,
        )?;

        Ok(Self {
            codec,
            context,
            frame,
            scaler,
            pts: 0,
        })
    }
}