use anyhow::Result;
use std::sync::{Arc, Mutex};
use tokio::sync::{mpsc, Notify};
use bsr_core::buffer::DropOldestBuffer;
use tracing::{info, error};
use std::time::Duration;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let args: Vec<String> = std::env::args().collect();
    let target_frames: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(50);
    let pipe_path = std::env::var("BSR_TELEMETRY_PIPE").unwrap_or_else(|_| r"\\.\pipe\bsr-telemetry".to_string());

    info!(target_frames, pipe = %pipe_path, "Starting real BSR smoke runner");

    // Shared frame buffer
    let frame_buf = Arc::new(Mutex::new(DropOldestBuffer::new(32usize)));
    let frame_notify = Arc::new(Notify::new());

    // Telemetry broadcast placeholder
    let (telem_tx, _telem_rx) = tokio::sync::broadcast::channel::<bsr_ipc::TelemetryEvent>(32);

    // Shutdown channels
    let (cap_shutdown_tx, cap_shutdown_rx) = mpsc::channel(1);
    let (enc_shutdown_tx, enc_shutdown_rx) = mpsc::channel(1);
    let (mux_cmd_tx, mux_cmd_rx) = mpsc::channel(1);

    // Encoder → muxer packet channel
    let (packet_tx, mut enc_packet_rx) = mpsc::channel::<bsr_encode::EncodedPacket>(32);
    let (mux_packet_tx, mux_packet_rx) = mpsc::channel::<bsr_mux::EncodedPacket>(32);
    let (mux_telem_tx, mut mux_telem_rx) = mpsc::channel(32);

    // Telemetry pipe channel
    let (frame_tx, frame_rx) = mpsc::channel::<bsr_ipc::telemetry_pipe::FrameInfo>(128);
    let pipe_clone = pipe_path.clone();
    let telemetry_handle = tokio::spawn(async move {
        bsr_ipc::telemetry_pipe::telemetry_client(frame_rx, pipe_clone).await;
    });

    // Capture backend — the platform's real one. A tool called `bsr-real-smoke`
    // selecting a mock on Linux was the sharpest form of this whole defect.
    let backend = bsr_capture::platform_backend();

    // BSR_CROP="x1,y1,x2,y2" records only that rectangle. Declared here rather than in
    // the library because this is a harness: the product reads the same region from its
    // config file (`capture.region`) and the UI's Settings panel.
    let mut capture_config = bsr_capture::CaptureConfig::default();
    let crop = std::env::var("BSR_CROP").ok().and_then(|v| {
        let n: Vec<i32> = v.split(',').filter_map(|p| p.trim().parse().ok()).collect();
        match n.as_slice() {
            [x1, y1, x2, y2] => Some(bsr_core::config::CaptureRegion {
                x1: *x1, y1: *y1, x2: *x2, y2: *y2,
            }),
            _ => {
                error!("BSR_CROP must be x1,y1,x2,y2 -- ignoring {v:?}");
                None
            }
        }
    });
    // Resolve against the capture size so the encoder and muxer are built for the
    // CROPPED dimensions. Configuring them for the screen and then feeding them a
    // smaller frame is exactly what the encoder now refuses.
    let (enc_w, enc_h) = match crop.as_ref().and_then(|r| {
        r.resolve(capture_config.width, capture_config.height)
    }) {
        Some((x, y, w, h)) => {
            info!(x, y, w, h, "recording a cropped region");
            capture_config.region = crop.clone();
            (w, h)
        }
        None => (capture_config.width, capture_config.height),
    };
    let capture_service = bsr_capture::CaptureService::new(
        backend,
        capture_config,
        telem_tx.clone(),
        cap_shutdown_rx,
        frame_buf.clone(),
        frame_notify.clone(),
    );

    tokio::spawn(async move {
        if let Err(e) = capture_service.run().await {
            error!(%e, "capture service failed");
        }
    });

    // Encoder service (real H.264 backend when available)
    let mut encoder_config = bsr_encode::EncoderConfig::default();
    encoder_config.width = enc_w;
    encoder_config.height = enc_h;
    let encoder_service = match bsr_encode::EncoderService::new(
        encoder_config.clone(),
        telem_tx.clone(),
        frame_buf.clone(),
        frame_notify.clone(),
        packet_tx,
        enc_shutdown_rx,
    ) {
        Ok(svc) => svc,
        Err(e) => {
            error!(%e, "encoder init failed");
            // Was `return Ok(())`: a run that never encoded a single frame exited 0
            // and read as a successful smoke test.
            return Err(anyhow::anyhow!("encoder init failed: {e}"));
        }
    };

    tokio::spawn(async move {
        if let Err(e) = encoder_service.run().await {
            error!(%e, "encoder failed");
        }
    });

    // Muxer service
    let (ipc_cmd_tx, _ipc_cmd_rx) = mpsc::channel(32);
    let ipc_client = bsr_ipc::IpcClient::new(ipc_cmd_tx);
    let mut muxer_config = bsr_ipc::MuxerConfig::default();
    muxer_config.width = enc_w;
    muxer_config.height = enc_h;
    let muxer_service = bsr_mux::MuxerService::new(
        muxer_config,
        bsr_mux::backends::mp4::Mp4Muxer::new(),
        mux_packet_rx,
        mux_telem_tx,
        mux_cmd_rx,
        ipc_client,
    );

    // Handle kept: the muxer writes the MP4 trailer (the `moov` atom) inside
    // `finalize()`, and `finalize()` only runs when it is told to stop. Detaching this
    // task and exiting produced a 500 KB file full of real H.264 that ffprobe could not
    // open at all -- "moov atom not found". A recording that cannot be played is not a
    // recording, so the run must both ASK for the trailer and WAIT for it.
    let muxer_task = tokio::spawn(async move {
        if let Err(e) = muxer_service.run().await {
            error!(%e, "muxer failed");
        }
    });

    // Bridge: forward encoded packets to muxer and telemetry, with monotonic seq.
    let cap_shutdown_clone = cap_shutdown_tx.clone();
    let (done_tx, done_rx) = tokio::sync::oneshot::channel::<()>();
    tokio::spawn(async move {
        let mut frames_sent: usize = 0;
        let mut seq_counter: u64 = 0;
        let encoder_config_for_bridge = encoder_config.clone();
        while let Some(ep) = enc_packet_rx.recv().await {
            let mp = bsr_mux::EncodedPacket {
                data: ep.data.clone(),
                pts: ep.pts,
                dts: ep.dts,
                keyframe: ep.keyframe,
            };
            if mux_packet_tx.send(mp).await.is_err() { break; }

            seq_counter = seq_counter.wrapping_add(1);
            let fi = bsr_ipc::telemetry_pipe::FrameInfo {
                sequence: seq_counter,
                width: encoder_config_for_bridge.width,
                height: encoder_config_for_bridge.height,
                size_bytes: ep.data.len(),
                timestamp: ep.timestamp,
            };
            let _ = frame_tx.send(fi).await;

            frames_sent += 1;
            info!(frames_sent, seq = seq_counter, "sent frame");

            if frames_sent >= target_frames {
                info!(frames_sent, "target reached, shutting down capture");
                let _ = cap_shutdown_clone.send(()).await;
                // Signal completion to main
                let _ = done_tx.send(());
                break;
            }
        }
    });

    // Wait for completion signalled by the bridge, with timeout.
    // Every failure here used to be logged and then discarded, with `Ok(())` returned
    // at the end of main: a run that captured nothing at all still exited 0.
    let outcome: Result<()> = match tokio::time::timeout(Duration::from_secs(60), done_rx).await {
        Ok(Ok(())) => {
            info!("real smoke run complete");
            Ok(())
        }
        Ok(Err(_)) => Err(anyhow::anyhow!(
            "completion sender dropped before signalling — the pipeline stopped early"
        )),
        Err(_) => Err(anyhow::anyhow!(
            "timeout waiting for {target_frames} frames — capture or encode never delivered"
        )),
    };

    // Finalize the MP4. Order matters and mirrors the UI's Stop path: capture has
    // already been told to stop above, so let the encoder's in-flight packets reach the
    // muxer before asking it to write the trailer, then wait for that write to land.
    tokio::time::sleep(Duration::from_millis(300)).await;
    if let Err(e) = mux_cmd_tx.send(bsr_mux::MuxerCommand::StopRecording).await {
        error!(%e, "could not ask the muxer to finalize -- the MP4 will be unplayable");
    }
    match tokio::time::timeout(Duration::from_secs(15), muxer_task).await {
        Ok(Ok(())) => info!("muxer finalized; MP4 trailer written"),
        Ok(Err(e)) => error!(%e, "muxer task panicked before writing the trailer"),
        Err(_) => error!("timed out waiting for the muxer to write the MP4 trailer"),
    }

    // Give telemetry client time to flush TelemetryDone and receive ack
    match tokio::time::timeout(Duration::from_secs(10), telemetry_handle).await {
        Ok(join_res) => match join_res {
            Ok(_) => info!("telemetry_client finished cleanly"),
            Err(e) => error!(%e, "telemetry_client task panicked"),
        },
        Err(_) => error!("timeout waiting for telemetry_client to finish"),
    }

    outcome
}
