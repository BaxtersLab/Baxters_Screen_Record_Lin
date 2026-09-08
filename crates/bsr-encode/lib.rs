// SPDX-License-Identifier: MIT
// Baxter's Screen Record — FFmpeg Encoding Layer
// Transforms raw frames into encoded packets

mod backends;
pub use backends::h264::H264EncoderBackend;

use bsr_core::buffer::DropOldestBuffer;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use tokio::sync::{broadcast, mpsc, Notify};
use tracing::{error, info, warn};
use bsr_ipc::TelemetryEvent;
use bsr_capture::CaptureFrame;

/// Encoder error types
///
/// Seed-BSR-G2-01-11: structured variants covering FFmpeg DLL loading failure,
/// missing codec, allocation failure, and runtime encoding errors.
#[derive(Debug, thiserror::Error)]
pub enum EncoderError {
    #[error("Initialization failed: {0}")]
    Initialization(String),
    #[error("Encoding failed: {0}")]
    Encoding(String),
    #[error("Unsupported format: {0}")]
    UnsupportedFormat(String),
    #[error("Shutdown failed: {0}")]
    Shutdown(String),
    /// One or more FFmpeg shared libraries could not be loaded.
    #[error("FFmpeg libraries not found: {missing:?}")]
    FfmpegNotFound { missing: Vec<String> },
    /// The requested codec was not registered in the FFmpeg build.
    #[error("Codec not found in FFmpeg registry")]
    CodecNotFound,
    /// Codec context or frame allocation failed.
    #[error("Allocation failed in FFmpeg")]
    AllocFailed,
}



/// Encoded packet data
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct EncodedPacket {
    pub data: Vec<u8>,
    pub timestamp: u64, // nanoseconds since epoch
    pub pts: i64,       // presentation timestamp
    pub dts: i64,       // decode timestamp
    pub keyframe: bool,
    pub codec: String,
}

/// Encoder configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncoderConfig {
    pub codec: String,
    pub preset: String,
    pub bitrate_kbps: u32,
    pub width: u32,
    pub height: u32,
    pub fps: u32,
}

impl Default for EncoderConfig {
    fn default() -> Self {
        Self {
            codec: "h264".to_string(),
            preset: "fast".to_string(),
            bitrate_kbps: 8000,
            width: 1920,
            height: 1080,
            fps: 30,
        }
    }
}

/// Encoder telemetry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncoderTelemetry {
    pub packets_encoded: u64,
    pub frames_encoded: u64,
    pub avg_encode_time_ms: f64,
}

impl Default for EncoderTelemetry {
    fn default() -> Self {
        Self {
            packets_encoded: 0,
            frames_encoded: 0,
            avg_encode_time_ms: 0.0,
        }
    }
}

/// Encoder backend trait
#[async_trait::async_trait]
pub trait EncoderBackend: Send + Sync + 'static {
    async fn initialize(&mut self, config: &EncoderConfig) -> Result<(), EncoderError>;
    async fn encode_frame(&mut self, frame: &CaptureFrame) -> Result<Option<EncodedPacket>, EncoderError>;
    async fn shutdown(&mut self) -> Result<(), EncoderError>;
}

/// Encoder service
///
/// Seed-BSR-G1-02-11 / G2-04-11: Consumes frames from the shared
/// DropOldestBuffer (produced by CaptureService), encodes them, and sends
/// encoded packets to the muxer via `packet_tx`.
pub struct EncoderService {
    backend: backends::h264::H264EncoderBackend,
    config: EncoderConfig,
    telemetry: EncoderTelemetry,
    telemetry_tx: broadcast::Sender<TelemetryEvent>,
    frame_buf: Arc<Mutex<DropOldestBuffer<CaptureFrame>>>,
    frame_notify: Arc<Notify>,
    packet_tx: mpsc::Sender<EncodedPacket>,
    shutdown_rx: mpsc::Receiver<()>,
}

impl EncoderService {
    pub fn new(
        config: EncoderConfig,
        telemetry_tx: broadcast::Sender<TelemetryEvent>,
        frame_buf: Arc<Mutex<DropOldestBuffer<CaptureFrame>>>,
        frame_notify: Arc<Notify>,
        packet_tx: mpsc::Sender<EncodedPacket>,
        shutdown_rx: mpsc::Receiver<()>,
    ) -> Result<Self, EncoderError> {
        let backend = backends::h264::H264EncoderBackend::new(&config)?;
        Ok(Self {
            backend,
            config,
            telemetry: EncoderTelemetry::default(),
            telemetry_tx,
            frame_buf,
            frame_notify,
            packet_tx,
            shutdown_rx,
        })
    }

    pub async fn run(mut self) -> Result<(), EncoderError> {
        info!("Encoder service starting");
        self.backend.initialize(&self.config)?;

        loop {
            tokio::select! {
                // Seed-BSR-G1-03-11: wait for a frame notification, then pop
                // from the shared ring buffer and encode.
                _ = self.frame_notify.notified() => {
                    // `Notify` holds at most one permit, so every push that lands
                    // while this task is encoding collapses into a single wake.
                    // Draining only one frame per wake leaves the surplus in the
                    // ring buffer until DropOldest evicts it, losing frames with no
                    // error reported anywhere. Drain the buffer instead.
                    loop {
                        let frame = {
                            let mut buf = self.frame_buf.lock().unwrap();
                            buf.pop()
                        };
                        let Some(frame) = frame else { break };
                        let start_time = std::time::Instant::now();
                        match self.backend.encode_frame(&frame) {
                            Ok(Some(packet)) => {
                                let latency = start_time.elapsed().as_millis() as f64;
                                self.telemetry.frames_encoded += 1;
                                self.telemetry.packets_encoded += 1;
                                self.telemetry.avg_encode_time_ms = latency;
                                if let Err(_) = self.packet_tx.send(packet).await {
                                    warn!("Failed to send encoded packet to muxer");
                                }
                                info!("Encoded frame in {:.1}ms", latency);
                            }
                            Ok(None) => {
                                // Encoder is buffering; no output packet yet.
                                self.telemetry.frames_encoded += 1;
                            }
                            Err(e) => {
                                // Seed-BSR-G1-04-11: log the FFmpeg error, emit
                                // telemetry, skip the frame — do NOT crash.
                                error!("Frame encoding failed: {}", e);
                                let _ = self.telemetry_tx.send(
                                    TelemetryEvent::ErrorOccurred {
                                        message: format!("Encode error: {}", e),
                                    },
                                );
                            }
                        }
                    }
                }
                _ = self.shutdown_rx.recv() => {
                    info!("Encoder service shutting down");
                    self.backend.shutdown()?;
                    break;
                }
            }
        }

        Ok(())
    }
}

/// Mock encoder backend
pub struct MockEncoderBackend;

impl MockEncoderBackend {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait::async_trait]
impl EncoderBackend for MockEncoderBackend {
    async fn initialize(&mut self, _config: &EncoderConfig) -> Result<(), EncoderError> {
        info!("Mock encoder initialized");
        Ok(())
    }

    async fn encode_frame(&mut self, frame: &CaptureFrame) -> Result<Option<EncodedPacket>, EncoderError> {
        // Generate synthetic encoded packet
        let data = vec![0u8; 1024]; // Small synthetic payload
        Ok(Some(EncodedPacket {
            data,
            timestamp: frame.timestamp,
            pts: frame.timestamp as i64,
            dts: frame.timestamp as i64,
            keyframe: true, // Mock as keyframe
            codec: "mock".to_string(),
        }))
    }

    async fn shutdown(&mut self) -> Result<(), EncoderError> {
        info!("Mock encoder shutdown");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bsr_capture::FrameFormat;
    use tokio::sync::{broadcast, mpsc};

    #[test]
    fn test_encoded_packet_serialization() {
        let packet = EncodedPacket {
            data: vec![1, 2, 3, 4],
            timestamp: 123456789,
            pts: 123456789,
            dts: 123456789,
            keyframe: true,
            codec: "h264".to_string(),
        };
        let json = serde_json::to_string(&packet).unwrap();
        let deserialized: EncodedPacket = serde_json::from_str(&json).unwrap();
        assert_eq!(packet, deserialized);
    }

    #[tokio::test]
    async fn test_mock_encoder() {
        let mut backend = MockEncoderBackend::new();
        let config = EncoderConfig::default();
        backend.initialize(&config).await.unwrap();

        let frame = CaptureFrame {
            data: vec![0u8; 1920 * 1080 * 4],
            timestamp: 123456789,
            width: 1920,
            height: 1080,
            format: FrameFormat::Bgra8,
        };

        let packet = backend.encode_frame(&frame).await.unwrap().expect("mock should produce packet");
        assert_eq!(packet.codec, "mock");
        assert!(!packet.data.is_empty());

        backend.shutdown().await.unwrap();
    }

    #[tokio::test]
    // Windows-only skip. MFT (Media Foundation Transform) is a Windows API; on Linux
    // `ffmpeg::encoder::find(Id::H264)` resolves to FFmpeg's software H.264 encoder,
    // which is present, and this test passes in 0.04s. Leaving it ignored everywhere
    // was silent test debt on this platform (Article IX.11) — the only Linux test of
    // the real EncoderService end to end was never running.
    #[cfg_attr(windows, ignore = "requires hardware MFT H.264 encoder; run manually with --ignored")]
    async fn test_encoder_service() {
        use std::sync::{Arc, Mutex};
        use tokio::sync::Notify;
        use bsr_core::buffer::DropOldestBuffer;

        let config = EncoderConfig::default();
        let (telemetry_tx, _) = broadcast::channel(32);
        let (packet_tx, mut packet_rx) = mpsc::channel(32);
        let (shutdown_tx, shutdown_rx) = mpsc::channel(1);

        let frame_buf = Arc::new(Mutex::new(DropOldestBuffer::new(32)));
        let frame_notify = Arc::new(Notify::new());

        let service = EncoderService::new(
            config, telemetry_tx, frame_buf.clone(), frame_notify.clone(), packet_tx, shutdown_rx,
        ).unwrap();

        // Spawn service
        let handle = tokio::spawn(async move {
            service.run().await.unwrap();
        });

        // Push multiple frames into the ring buffer (encoder may buffer before producing output)
        for i in 0..30 {
            let frame = CaptureFrame {
                data: vec![0u8; 1920 * 1080 * 4],
                timestamp: 123456789 + i,
                width: 1920,
                height: 1080,
                format: FrameFormat::Bgra8,
            };
            frame_buf.lock().unwrap().push(frame);
            frame_notify.notify_one();
        }

        // Receive packet with timeout
        let packet = tokio::time::timeout(
            std::time::Duration::from_secs(10),
            packet_rx.recv()
        ).await.expect("timed out waiting for packet").unwrap();
        assert_eq!(packet.codec, "h264");

        // Shutdown
        shutdown_tx.send(()).await.unwrap();

        handle.await.unwrap();
    }

    #[test]
    fn test_h264_backend_initialization() {
        let config = EncoderConfig {
            codec: "h264".to_string(),
            preset: "ultrafast".to_string(),
            bitrate_kbps: 5000,
            width: 1280,
            height: 720,
            fps: 30,
        };
        let backend = backends::h264::H264EncoderBackend::new(&config);
        assert!(backend.is_ok());
    }

    #[test]
    fn test_h264_encode_frame() {
        let config = EncoderConfig {
            codec: "h264".to_string(),
            preset: "ultrafast".to_string(),
            bitrate_kbps: 5000,
            width: 1280,
            height: 720,
            fps: 30,
        };
        let mut backend = backends::h264::H264EncoderBackend::new(&config).unwrap();
        backend.initialize(&config).unwrap();

        let frame = CaptureFrame {
            data: vec![0u8; 1280 * 720 * 4],
            timestamp: 123456789,
            width: 1280,
            height: 720,
            format: FrameFormat::Bgra8,
        };

        // Encoder may buffer initial frames; send several and check we get at least one packet
        let mut got_packet = false;
        for _ in 0..30 {
            if let Ok(Some(packet)) = backend.encode_frame(&frame) {
                assert_eq!(packet.codec, "h264");
                assert!(!packet.data.is_empty());
                got_packet = true;
                break;
            }
        }
        assert!(got_packet, "Expected at least one packet after 30 frames");
    }

    // ---- stride regression, 2026-09-07 --------------------------------------
    //
    // The test box recorded a 1842x934 crop on 2026-09-07 and every frame came back
    // split into severe diagonal displaced bands, while the 1920x1080 baseline was
    // pristine and the file decoded with ZERO ffmpeg errors. Cause: encode_frame
    // copied the packed BGRA buffer into the FFmpeg plane as one flat memcpy,
    // ignoring the plane's ALIGNED linesize.
    //
    //     1920 wide -> packed row 7680 == linesize 7680 -> drift 0     (invisible)
    //     1842 wide -> packed row 7368 vs linesize 7424 -> drift 56/row
    //
    // Every encoder test in this file used 1280x720 or 1920x1080. Both are already
    // stride-aligned, which is exactly why this survived the project's whole life.
    // These tests use deliberately UNALIGNED dimensions.

    #[test]
    fn packed_rows_land_on_stride_boundaries_not_packed_ones() {
        // 3 rows of 5 bytes packed, into a plane with 8-byte rows.
        let src: Vec<u8> = vec![1, 1, 1, 1, 1,
                                2, 2, 2, 2, 2,
                                3, 3, 3, 3, 3];
        let mut dst = vec![0u8; 8 * 3];
        backends::h264::copy_packed_into_plane(&mut dst, 8, &src, 5, 3).unwrap();

        // Each row must start at a multiple of the DESTINATION stride.
        assert_eq!(&dst[0..5],   &[1, 1, 1, 1, 1], "row 0 misplaced");
        assert_eq!(&dst[8..13],  &[2, 2, 2, 2, 2], "row 1 misplaced -- this is the shear");
        assert_eq!(&dst[16..21], &[3, 3, 3, 3, 3], "row 2 misplaced -- shear accumulates");

        // The padding between rows must be left alone, not overwritten with pixels.
        assert_eq!(&dst[5..8],   &[0, 0, 0], "row 0 padding was written into");
        assert_eq!(&dst[13..16], &[0, 0, 0], "row 1 padding was written into");
    }

    #[test]
    fn a_flat_memcpy_would_shear_an_unaligned_frame() {
        // Demonstrates the OLD behaviour is genuinely wrong, so this suite documents
        // the defect rather than only asserting the fix.
        let w = 1842usize;                 // the real crop width from the test box
        let src_stride = w * 4;            // 7368, packed
        let dst_stride = 7424usize;        // 64-aligned linesize ffmpeg would allocate
        assert_ne!(src_stride, dst_stride, "pick a width that is actually unaligned");

        // Mark the FIRST byte of each row. A row filled uniformly cannot expose the
        // shear -- reading 56 bytes into the same row returns the same value -- so the
        // marker is what makes the drift observable at all.
        let rows = 4usize;
        let mut src = vec![0u8; src_stride * rows];
        for y in 0..rows {
            src[y * src_stride] = 0xFF;                       // row-start marker
            for x in 1..src_stride { src[y * src_stride + x] = y as u8 + 1; }
        }

        let mut correct = vec![0u8; dst_stride * rows];
        backends::h264::copy_packed_into_plane(
            &mut correct, dst_stride, &src, src_stride, rows,
        ).unwrap();

        let mut flat = vec![0u8; dst_stride * rows];
        flat[..src.len()].copy_from_slice(&src);   // the old, buggy copy

        // Correct: every row begins on a destination-stride boundary, marker intact.
        for y in 0..rows {
            assert_eq!(correct[y * dst_stride], 0xFF,
                "row {y} must begin at y*dst_stride");
        }
        // Buggy: rows 1+ drift by 56 bytes each, so the marker is NOT at the boundary.
        assert_eq!(flat[0], 0xFF, "row 0 is correct either way -- the drift accumulates");
        for y in 1..rows {
            assert_ne!(flat[y * dst_stride], 0xFF,
                "a flat memcpy must NOT land row {y}'s marker on the boundary");
        }
        assert_ne!(correct, flat, "the two copies must differ, or the test proves nothing");
    }

    #[test]
    fn aligned_width_is_the_case_that_hid_the_bug() {
        // 1920*4 = 7680 is already 64-aligned, so packed == linesize and a flat memcpy
        // happens to be correct. Kept so the asymmetry is explicit in the suite.
        let w = 1920usize;
        let stride = w * 4;
        assert_eq!(stride % 64, 0, "1920 BGRA rows are 64-aligned");

        let rows = 3usize;
        let src: Vec<u8> = (0..rows).flat_map(|y| vec![y as u8 + 1; stride]).collect();
        let mut dst = vec![0u8; stride * rows];
        backends::h264::copy_packed_into_plane(&mut dst, stride, &src, stride, rows).unwrap();
        assert_eq!(dst, src, "with equal strides the copy is a straight image");
    }

    #[test]
    fn refuses_a_destination_narrower_than_a_row() {
        let src = vec![0u8; 40];
        let mut dst = vec![0u8; 40];
        let err = backends::h264::copy_packed_into_plane(&mut dst, 4, &src, 8, 5)
            .expect_err("a linesize shorter than a packed row must be refused, not truncated");
        assert!(err.contains("shorter"), "error should name the problem: {err}");
    }

    #[test]
    fn test_h264_encode_frame_at_an_unaligned_width() {
        // The end-to-end shape of the test-box failure: a crop whose width is not
        // stride-aligned must still encode.
        let (w, h) = (1842u32, 934u32);
        assert_ne!((w as usize * 4) % 64, 0, "width must be unaligned for this to test anything");

        let config = EncoderConfig {
            codec: "h264".to_string(),
            preset: "ultrafast".to_string(),
            bitrate_kbps: 5000,
            width: w,
            height: h,
            fps: 30,
        };
        let mut backend = backends::h264::H264EncoderBackend::new(&config).unwrap();
        backend.initialize(&config).unwrap();

        let frame = CaptureFrame {
            data: vec![0u8; w as usize * h as usize * 4],
            timestamp: 123456789,
            width: w,
            height: h,
            format: FrameFormat::Bgra8,
        };

        let mut got_packet = false;
        for _ in 0..30 {
            if let Ok(Some(packet)) = backend.encode_frame(&frame) {
                assert_eq!(packet.codec, "h264");
                assert!(!packet.data.is_empty());
                got_packet = true;
                break;
            }
        }
        assert!(got_packet, "an unaligned-width frame must encode");
    }

    #[test]
    fn test_h264_shutdown() {
        let config = EncoderConfig::default();
        let mut backend = backends::h264::H264EncoderBackend::new(&config).unwrap();
        backend.initialize(&config).unwrap();

        let frame = CaptureFrame {
            data: vec![0u8; 1920 * 1080 * 4],
            timestamp: 123456789,
            width: 1920,
            height: 1080,
            format: FrameFormat::Bgra8,
        };

        let _ = backend.encode_frame(&frame).unwrap();
        backend.shutdown().unwrap();
    }

    // ---- Timeline correctness -------------------------------------------
    //
    // Real capture cannot always sustain the configured fps: a damage-driven
    // PipeWire stream on a quiet desktop, or a 1080p swscale+x264 pass that
    // takes longer than one frame interval, both deliver fewer frames than
    // `config.fps`. If pts is a frame counter, the file's duration becomes
    // `frames / fps` regardless of how long the recording actually ran, so a
    // 17 s recording is written as a 7 s file that plays ~2.3x too fast.
    // Observed on a packaged 1.0.0-4 install: 221 frames over 17 s of wall
    // clock, muxed as 7.33 s. pts must come from the capture timestamp.

    /// Frames arriving at a real 10 fps against a 30 fps config must produce
    /// pts spaced ~3 ticks apart (100 ms at time_base 1/30), not 1 tick.
    #[test]
    fn h264_pts_follows_capture_time_not_frame_count() {
        let config = EncoderConfig {
            codec: "h264".to_string(),
            preset: "ultrafast".to_string(),
            bitrate_kbps: 2000,
            width: 320,
            height: 240,
            fps: 30,
        };
        let mut backend = backends::h264::H264EncoderBackend::new(&config).unwrap();
        backend.initialize(&config).unwrap();

        const N: u64 = 12;
        const STEP_NS: u64 = 100_000_000; // 100 ms => 10 fps of real capture
        let base = 1_700_000_000_000_000_000u64;

        let mut pts_seen = Vec::new();
        for i in 0..N {
            let frame = CaptureFrame {
                // Vary the content so x264 cannot collapse the whole run.
                data: vec![(i as u8).wrapping_mul(37); 320 * 240 * 4],
                timestamp: base + i * STEP_NS,
                width: 320,
                height: 240,
                format: FrameFormat::Bgra8,
            };
            if let Some(p) = backend.encode_frame(&frame).unwrap() {
                pts_seen.push(p.pts);
            }
        }

        assert!(
            pts_seen.len() >= 2,
            "expected at least two packets, got {}",
            pts_seen.len()
        );

        // Each 100 ms step is 3 ticks at time_base 1/30. A frame counter gives 1.
        let gaps: Vec<i64> = pts_seen.windows(2).map(|w| w[1] - w[0]).collect();
        let typical = gaps[gaps.len() / 2];
        assert!(
            typical >= 2,
            "pts advance {typical} tick(s) per 100 ms frame; expected ~3. \
             The encoder is stamping frame indices instead of capture \
             timestamps, so the file's duration will not match wall clock. \
             gaps={gaps:?} pts={pts_seen:?}"
        );

        // Total span must reflect real elapsed time, not frame count.
        let span = pts_seen.last().unwrap() - pts_seen.first().unwrap();
        let elapsed_ticks = ((N - 1) * STEP_NS * 30 / 1_000_000_000) as i64; // 33
        assert!(
            span >= elapsed_ticks - 6,
            "pts span {span} ticks covers far less than the {elapsed_ticks} \
             ticks of real capture time; the recording would play back sped up. \
             pts={pts_seen:?}"
        );
    }

    /// Two frames inside the same time_base tick must still advance pts:
    /// x264 and the mov muxer both reject non-monotonic timestamps.
    #[test]
    fn h264_pts_is_strictly_increasing_for_close_frames() {
        let config = EncoderConfig {
            codec: "h264".to_string(),
            preset: "ultrafast".to_string(),
            bitrate_kbps: 2000,
            width: 320,
            height: 240,
            fps: 30,
        };
        let mut backend = backends::h264::H264EncoderBackend::new(&config).unwrap();
        backend.initialize(&config).unwrap();

        let base = 1_700_000_000_000_000_000u64;
        let mut pts_seen = Vec::new();
        for i in 0..8u64 {
            let frame = CaptureFrame {
                data: vec![(i as u8).wrapping_mul(29); 320 * 240 * 4],
                // 1 ms apart: all well inside one 1/30 s tick.
                timestamp: base + i * 1_000_000,
                width: 320,
                height: 240,
                format: FrameFormat::Bgra8,
            };
            if let Some(p) = backend.encode_frame(&frame).unwrap() {
                pts_seen.push(p.pts);
            }
        }

        for w in pts_seen.windows(2) {
            assert!(
                w[1] > w[0],
                "pts must strictly increase, got {:?}",
                pts_seen
            );
        }
    }

    /// A capture clock that steps backwards (SystemTime is not monotonic —
    /// NTP can slew it) must not produce a rejected or rewound stream.
    #[test]
    fn h264_pts_survives_a_backwards_clock() {
        let config = EncoderConfig {
            codec: "h264".to_string(),
            preset: "ultrafast".to_string(),
            bitrate_kbps: 2000,
            width: 320,
            height: 240,
            fps: 30,
        };
        let mut backend = backends::h264::H264EncoderBackend::new(&config).unwrap();
        backend.initialize(&config).unwrap();

        let base = 1_700_000_000_000_000_000u64;
        let stamps = [
            base,
            base + 100_000_000,
            base + 50_000_000, // clock jumped backwards
            base + 200_000_000,
        ];

        let mut pts_seen = Vec::new();
        for (i, ts) in stamps.iter().enumerate() {
            let frame = CaptureFrame {
                data: vec![(i as u8).wrapping_mul(53); 320 * 240 * 4],
                timestamp: *ts,
                width: 320,
                height: 240,
                format: FrameFormat::Bgra8,
            };
            if let Some(p) = backend.encode_frame(&frame).unwrap() {
                pts_seen.push(p.pts);
            }
        }

        for w in pts_seen.windows(2) {
            assert!(
                w[1] > w[0],
                "a backwards capture clock must not rewind pts, got {:?}",
                pts_seen
            );
        }
    }


    /// `Notify` stores at most one permit, so frames pushed while the encoder is
    /// busy collapse into a single wake. If the service pops only one frame per
    /// wake the rest sit in the ring buffer until some later push, and are
    /// eventually evicted by DropOldest — the recording loses frames with no
    /// error anywhere. The service must drain the buffer on each wake.
    #[tokio::test]
    #[cfg_attr(windows, ignore = "requires hardware MFT H.264 encoder")]
    async fn encoder_service_drains_every_buffered_frame_per_wake() {
        use std::sync::{Arc, Mutex};
        use tokio::sync::Notify;
        use bsr_core::buffer::DropOldestBuffer;

        let config = EncoderConfig {
            codec: "h264".to_string(),
            preset: "ultrafast".to_string(),
            bitrate_kbps: 2000,
            width: 320,
            height: 240,
            fps: 30,
        };
        let (telemetry_tx, _) = broadcast::channel(32);
        let (packet_tx, _packet_rx) = mpsc::channel(64);
        let (shutdown_tx, shutdown_rx) = mpsc::channel(1);

        let frame_buf = Arc::new(Mutex::new(DropOldestBuffer::new(32)));
        let frame_notify = Arc::new(Notify::new());

        let service = EncoderService::new(
            config, telemetry_tx, frame_buf.clone(), frame_notify.clone(),
            packet_tx, shutdown_rx,
        ).unwrap();
        let handle = tokio::spawn(async move { service.run().await.unwrap(); });

        // Fill the buffer, then wake exactly once: the collapse case.
        const N: usize = 8;
        for i in 0..N {
            frame_buf.lock().unwrap().push(CaptureFrame {
                data: vec![(i as u8).wrapping_mul(31); 320 * 240 * 4],
                timestamp: 1_700_000_000_000_000_000u64 + (i as u64) * 33_000_000,
                width: 320,
                height: 240,
                format: FrameFormat::Bgra8,
            });
        }
        frame_notify.notify_one();

        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(15);
        loop {
            let left = frame_buf.lock().unwrap().len();
            if left == 0 {
                break;
            }
            if std::time::Instant::now() > deadline {
                shutdown_tx.send(()).await.ok();
                panic!(
                    "{left} of {N} frames left in the ring buffer after a single \
                     wake — the encoder drains only one frame per notification, so \
                     frames pushed while it was busy are lost to DropOldest"
                );
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }

        shutdown_tx.send(()).await.unwrap();
        handle.await.unwrap();
    }

}
