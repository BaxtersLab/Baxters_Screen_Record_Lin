// SPDX-License-Identifier: MIT
//! The live preview must not consume the recording's frames.
//!
//! bsr-ui ran its preview task on the **same** `DropOldestBuffer` and the same
//! `Notify` as the encoder service. `notify_one()` wakes exactly one waiter, so the
//! preview and the encoder were competing consumers of one queue, and every frame the
//! preview won was scaled for display and then discarded.
//!
//! Measured on a packaged install: a 17 s recording produced 221 frames instead of
//! ~510 (~13 fps), and once the encoder was changed to drain the buffer per wake the
//! preview could win outright — a 17 s recording wrote an empty 261-byte MP4 with a
//! zero-length `mdat` and no track, while the UI reported "Recording finalised and
//! ready to save".
//!
//! This drives the real `CaptureService`, the real `EncoderService` and the real
//! `MuxerService` with a preview attached, and checks that the recording is intact.

use bsr_capture::{CaptureBackend, CaptureError, CaptureFrame, CaptureService, FrameFormat};
use bsr_core::buffer::DropOldestBuffer;
use bsr_mux::backends::mp4::Mp4Muxer;
use bsr_mux::MuxerService;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, watch, Notify};

const W: u32 = 640;
const H: u32 = 480;
const FPS: u32 = 30;

/// A backend that always has a frame ready, so the service's own pacing is what
/// decides the rate — the same shape as a portal stream repeating its last frame.
struct SyntheticBackend {
    n: u64,
}

#[async_trait::async_trait]
impl CaptureBackend for SyntheticBackend {
    async fn initialize(&mut self) -> Result<(), CaptureError> {
        Ok(())
    }
    async fn capture_frame(&mut self) -> Result<CaptureFrame, CaptureError> {
        self.n += 1;
        let ts = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos() as u64;
        Ok(CaptureFrame {
            data: vec![(self.n as u8).wrapping_mul(37); (W * H * 4) as usize],
            timestamp: ts,
            width: W,
            height: H,
            format: FrameFormat::Bgra8,
        })
    }
    async fn shutdown(&mut self) -> Result<(), CaptureError> {
        Ok(())
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_live_preview_does_not_consume_the_recordings_frames() {
    let dir = tempfile::tempdir().unwrap();
    let mut mcfg = bsr_ipc::MuxerConfig {
        base_output_path: dir.path().to_path_buf(),
        file_naming_strategy: bsr_ipc::FileNamingStrategy::Simple("take.mp4".into()),
        ..Default::default()
    };
    mcfg.width = W;
    mcfg.height = H;
    mcfg.fps = FPS;
    let out = mcfg.preview_output_path();

    let ccfg = bsr_capture::CaptureConfig {
        width: W,
        height: H,
        fps: FPS,
        drop_policy: bsr_capture::DropPolicy::DropOldest,
        buffer_capacity: 8,
        region: None,
    };
    let ecfg = bsr_encode::EncoderConfig {
        codec: "h264".into(),
        preset: "superfast".into(),
        bitrate_kbps: 4000,
        width: W,
        height: H,
        fps: FPS,
    };

    let (telem_tx, _telem_rx) = broadcast::channel(256);
    let (enc_packet_tx, mut enc_packet_rx) = mpsc::channel(64);
    let (mux_packet_tx, mux_packet_rx) = mpsc::channel(64);
    let (cap_shutdown_tx, cap_shutdown_rx) = mpsc::channel(1);
    let (enc_shutdown_tx, enc_shutdown_rx) = mpsc::channel(1);
    let (mtelem_tx, _mtelem_rx) = mpsc::channel(64);
    let (_cmd_tx, cmd_rx) = mpsc::channel(1);
    let (ipc_tx, _ipc_rx) = mpsc::channel(32);

    let frame_buf = Arc::new(Mutex::new(DropOldestBuffer::new(8)));
    let notify = Arc::new(Notify::new());
    let (preview_tx, mut preview_rx) = watch::channel::<Option<CaptureFrame>>(None);

    let capture = CaptureService::new(
        SyntheticBackend { n: 0 },
        ccfg,
        telem_tx.clone(),
        cap_shutdown_rx,
        frame_buf.clone(),
        notify.clone(),
    )
    .with_preview(preview_tx, Duration::from_millis(200));
    let cap_task = tokio::spawn(async move { capture.run().await });

    let encoder = bsr_encode::EncoderService::new(
        ecfg,
        telem_tx,
        frame_buf.clone(),
        notify.clone(),
        enc_packet_tx,
        enc_shutdown_rx,
    )
    .expect("encoder service");
    let enc_task = tokio::spawn(async move { encoder.run().await });

    // The bridge bsr-ui runs between the two packet types.
    let encoded = Arc::new(AtomicU64::new(0));
    let encoded_c = encoded.clone();
    tokio::spawn(async move {
        while let Some(ep) = enc_packet_rx.recv().await {
            encoded_c.fetch_add(1, Ordering::Relaxed);
            let mp = bsr_mux::EncodedPacket {
                data: ep.data,
                pts: ep.pts,
                dts: ep.dts,
                keyframe: ep.keyframe,
            };
            if mux_packet_tx.send(mp).await.is_err() {
                break;
            }
        }
    });

    let mux = MuxerService::new(
        mcfg,
        Mp4Muxer::new(),
        mux_packet_rx,
        mtelem_tx,
        cmd_rx,
        bsr_ipc::IpcClient::new(ipc_tx),
    );
    let mux_task = tokio::spawn(async move { mux.run().await });

    // The preview consumer, doing display-shaped work on every frame it is handed.
    let previewed = Arc::new(AtomicU64::new(0));
    let previewed_c = previewed.clone();
    let preview_task = tokio::spawn(async move {
        while preview_rx.changed().await.is_ok() {
            let f = preview_rx.borrow_and_update().clone();
            if let Some(f) = f {
                previewed_c.fetch_add(1, Ordering::Relaxed);
                let _ = f.data.iter().fold(0u64, |a, b| a.wrapping_add(*b as u64));
            }
        }
    });

    tokio::time::sleep(Duration::from_secs(3)).await;

    cap_shutdown_tx.send(()).await.ok();
    tokio::time::timeout(Duration::from_secs(20), cap_task)
        .await
        .expect("capture did not exit")
        .expect("capture panicked")
        .expect("capture errored");
    enc_shutdown_tx.send(()).await.ok();
    tokio::time::timeout(Duration::from_secs(30), enc_task)
        .await
        .expect("encoder did not exit")
        .expect("encoder panicked")
        .expect("encoder errored");
    preview_task.abort();
    tokio::time::timeout(Duration::from_secs(30), mux_task)
        .await
        .expect("muxer did not exit")
        .expect("muxer panicked")
        .expect("muxer errored");

    let enc_n = encoded.load(Ordering::Relaxed);
    let prev_n = previewed.load(Ordering::Relaxed);
    let size = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
    eprintln!("encoded={enc_n} previewed={prev_n} file={size} bytes");

    // 3 s at 30 fps is ~90 frames. Allow generous slack for scheduling, but a
    // preview stealing frames shows up as a fraction of this, and the 261-byte
    // field failure shows up as ~0.
    assert!(
        enc_n >= 60,
        "only {enc_n} frames reached the encoder in 3 s at {FPS} fps; the preview \
         ({prev_n} frames) is consuming the recording's frames"
    );
    assert!(
        size > 20_000,
        "recording is only {size} bytes from {enc_n} encoded frames"
    );
    // The preview must still work, and must be throttled rather than full-rate.
    assert!(prev_n > 0, "the preview received no frames at all");
    assert!(
        prev_n < enc_n,
        "preview took {prev_n} frames vs {enc_n} encoded — it should be throttled"
    );
}
