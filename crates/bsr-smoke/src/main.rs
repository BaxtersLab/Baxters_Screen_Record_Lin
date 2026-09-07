use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, Notify};
use bsr_core::buffer::DropOldestBuffer;
use tracing::{info, error};
use bsr_encode::EncoderBackend;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    // Config
    let target_frames: usize = std::env::args().nth(1).and_then(|s| s.parse().ok()).unwrap_or(50);
    let pipe_path = std::env::var("BSR_TELEMETRY_PIPE").unwrap_or_else(|_| r"\\.\pipe\bsr-telemetry".to_string());

    info!(target_frames, pipe = %pipe_path, "Starting BSR smoke runner");

    // Shared frame buffer
    let frame_buf = Arc::new(Mutex::new(DropOldestBuffer::new(32usize)));
    let frame_notify = Arc::new(Notify::new());

    // Telemetry broadcast placeholder
    let (telem_tx, _telem_rx) = tokio::sync::broadcast::channel::<bsr_ipc::TelemetryEvent>(32);

    // Shutdown channel for capture
    let (cap_shutdown_tx, cap_shutdown_rx) = mpsc::channel(1);

    // Telemetry pipe channel
    let (frame_tx, frame_rx) = mpsc::channel::<bsr_ipc::telemetry_pipe::FrameInfo>(128);

    // Spawn telemetry client
    let pipe_clone = pipe_path.clone();
    tokio::spawn(async move {
        bsr_ipc::telemetry_pipe::telemetry_client(frame_rx, pipe_clone).await;
    });

    // The platform's real capture backend: DXGI on Windows, xdg-desktop-portal +
    // PipeWire on Linux. This used to pick `mock_backend::MockCaptureBackend` on
    // every non-Windows target, so a "successful" smoke run on Linux proved only
    // that the pipeline could carry frames the capture layer had invented.
    let backend = bsr_capture::platform_backend();

    // Create capture service and run it
    let capture_config = bsr_capture::CaptureConfig::default();
    let capture_service = bsr_capture::CaptureService::new(
        backend,
        capture_config,
        telem_tx.clone(),
        cap_shutdown_rx,
        frame_buf.clone(),
        frame_notify.clone(),
    );

    // The handle is kept, not detached. A detached capture task that fails to start
    // only logged the error, and the encoder loop below then waited on `frame_notify`
    // forever — a silent hang with an error line scrolled off the top. That mattered
    // little when the backend was a mock that could not fail; with a real capture
    // backend, "the portal grant was refused" is an ordinary outcome and must exit
    // non-zero rather than hang.
    let mut capture_task = tokio::spawn(async move { capture_service.run().await });

    // Create mock encoder backend (fast, deterministic)
    let mut encoder = bsr_encode::MockEncoderBackend::new();
    encoder.initialize(&bsr_encode::EncoderConfig::default()).await?;

    // Encoder loop: wait for frames, encode with mock backend, forward telemetry
    let mut frames_sent: usize = 0;
    // Monotonic sequence counter maintained by the sender (resets on new run)
    let mut seq_counter: u64 = 0;
    loop {
        // Wait for a frame, or for the capture service to stop. Whichever happens
        // first: a capture service that has exited will never notify again.
        tokio::select! {
            joined = &mut capture_task => {
                return match joined {
                    Ok(Ok(())) => {
                        info!(frames_sent, "capture service stopped before the frame target");
                        Err(anyhow::anyhow!(
                            "capture stopped after {frames_sent} of {target_frames} frames"
                        ))
                    }
                    Ok(Err(e)) => {
                        error!(%e, "capture service failed");
                        Err(anyhow::anyhow!("capture service failed: {e}"))
                    }
                    Err(e) => Err(anyhow::anyhow!("capture task panicked: {e}")),
                };
            }
            _ = frame_notify.notified() => {}
        }

        // Pop one frame
        let frame_opt = {
            let mut buf = frame_buf.lock().unwrap();
            buf.pop()
        };

        if let Some(frame) = frame_opt {
            // Encode (mock)
            match encoder.encode_frame(&frame).await {
                Ok(Some(packet)) => {
                    // Send FrameInfo to telemetry client
                    // increment monotonic sequence counter by exactly one per encoded frame
                    seq_counter = seq_counter.wrapping_add(1);
                    let fi = bsr_ipc::telemetry_pipe::FrameInfo {
                        sequence: seq_counter,
                        width: frame.width,
                        height: frame.height,
                        size_bytes: packet.data.len(),
                        timestamp: packet.timestamp,
                    };
                    let _ = frame_tx.send(fi.clone()).await;
                    frames_sent += 1;
                    info!(sent = frames_sent, seq = packet.pts, size = fi.size_bytes, "encoded packet forwarded");
                }
                Ok(None) => {
                    // encoder buffering
                }
                Err(e) => {
                    error!(%e, "encoding error");
                }
            }

            if frames_sent >= target_frames {
                info!(frames_sent, "target reached, shutting down capture");
                let _ = cap_shutdown_tx.send(()).await;
                break;
            }
        }
    }

    // Allow telemetry client to flush
    tokio::time::sleep(std::time::Duration::from_millis(200)).await;

    info!(frames_sent, "smoke run complete");
    Ok(())
}
