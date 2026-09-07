// SPDX-License-Identifier: MIT
//! Capture -> crop -> encode -> mux -> a file another tool can decode.
//!
//! This is the path that hid two real defects this session, both of which passed every
//! per-crate test: the muxer exited without writing its MP4 trailer when the packet
//! stream ended (producing megabytes of real H.264 in a container nothing could open),
//! and the encoder silently disagreed with the capture layer about frame size. Both
//! needed three crates in the same test to show up.

mod common;

use std::time::Duration;

use bsr_capture::{crop_frame, CaptureFrame, FrameFormat};
use bsr_encode::{EncoderConfig, H264EncoderBackend};
use bsr_mux::backends::mp4::Mp4Muxer;
use bsr_mux::{EncodedPacket, MuxerService};
use bsr_ui::UiSettings;
use common::TestHarness;
use ffmpeg_next as ffmpeg;

const SCREEN_W: u32 = 1280;
const SCREEN_H: u32 = 720;
const FPS: u32 = 30;
const FRAMES: usize = 40;

/// A frame that genuinely differs between calls, so the encoder has real work to do and
/// a frozen pipeline cannot masquerade as a working one.
fn screen_frame(i: usize) -> CaptureFrame {
    let (w, h) = (SCREEN_W as usize, SCREEN_H as usize);
    let mut data = vec![0u8; w * h * 4];
    for y in 0..h {
        for x in 0..w {
            let px = (y * w + x) * 4;
            data[px] = ((x + i * 5) & 0xFF) as u8;
            data[px + 1] = ((y + i * 3) & 0xFF) as u8;
            data[px + 2] = ((x ^ y).wrapping_add(i) & 0xFF) as u8;
            data[px + 3] = 0xFF;
        }
    }
    CaptureFrame { data, timestamp: i as u64, width: SCREEN_W, height: SCREEN_H, format: FrameFormat::Bgra8 }
}

/// Independently demux and decode. Opening the container at all is the check that fails
/// on a missing `moov` atom.
fn decode(path: &std::path::Path) -> (u32, u32, usize) {
    ffmpeg::init().expect("ffmpeg init");
    let mut ictx = ffmpeg::format::input(&path).unwrap_or_else(|e| {
        panic!(
            "produced file is not a readable MP4: {e} ({} bytes). A missing moov atom \
             means the recording is unplayable even though the file exists.",
            std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
        )
    });
    let stream = ictx.streams().best(ffmpeg::media::Type::Video).expect("no video stream");
    let idx = stream.index();
    let ctx = ffmpeg::codec::Context::from_parameters(stream.parameters()).unwrap();
    let mut dec = ctx.decoder().video().expect("video decoder");
    let (w, h) = (dec.width(), dec.height());

    let mut frames = 0usize;
    for (s, packet) in ictx.packets() {
        if s.index() == idx && dec.send_packet(&packet).is_ok() {
            let mut f = ffmpeg::util::frame::Video::empty();
            while dec.receive_frame(&mut f).is_ok() {
                frames += 1;
            }
        }
    }
    let _ = dec.send_eof();
    let mut f = ffmpeg::util::frame::Video::empty();
    while dec.receive_frame(&mut f).is_ok() {
        frames += 1;
    }
    (w, h, frames)
}

/// Run the whole pipeline for the given operator insets and return the output path.
async fn record_with_insets(h: &TestHarness, l: u32, t: u32, r: u32, b: u32) -> std::path::PathBuf {
    let settings = UiSettings {
        resolution: format!("{SCREEN_W}x{SCREEN_H}"),
        crop_left: l, crop_top: t, crop_right: r, crop_bottom: b,
        fps: FPS,
        output_folder: h.out_dir.path().to_string_lossy().into_owned(),
        file_naming_strategy: bsr_ui::FileNamingStrategyUi::Simple,
        ..UiSettings::default()
    };
    let (rec_w, rec_h) = settings.recording_size();
    let region = settings.to_capture_region();

    // Encoder built for the CROPPED size, exactly as bsr-ui does it.
    let enc_cfg = EncoderConfig {
        codec: "h264".into(),
        preset: "ultrafast".into(),
        bitrate_kbps: 2500,
        width: rec_w,
        height: rec_h,
        fps: FPS,
    };
    let mut enc = H264EncoderBackend::new(&enc_cfg).expect("encoder init");
    enc.initialize(&enc_cfg).expect("encoder initialize");

    let mut cfg = settings.to_muxer_config();
    cfg.file_naming_strategy = bsr_ipc::FileNamingStrategy::Simple("take.mp4".into());
    let out = cfg.preview_output_path();

    let (packet_tx, packet_rx) = tokio::sync::mpsc::channel(64);
    let (telemetry_tx, _telemetry_rx) = tokio::sync::mpsc::channel(4);
    let (_cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(1);
    let (ipc_cmd_tx, _ipc_cmd_rx) = tokio::sync::mpsc::channel(32);
    let service = MuxerService::new(
        cfg, Mp4Muxer::new(), packet_rx, telemetry_tx, cmd_rx, bsr_ipc::IpcClient::new(ipc_cmd_tx),
    );
    let muxer = tokio::spawn(async move { service.run().await });

    for i in 0..FRAMES {
        let captured = screen_frame(i);
        let frame = match &region {
            Some(reg) => crop_frame(&captured, reg),
            None => captured,
        };
        assert_eq!(
            (frame.width, frame.height),
            (rec_w, rec_h),
            "capture and encoder configuration disagree; the encoder refuses every frame"
        );
        if let Some(p) = enc.encode_frame(&frame).expect("encode_frame") {
            let sent = tokio::time::timeout(
                Duration::from_secs(5),
                packet_tx.send(EncodedPacket { data: p.data, pts: p.pts, dts: p.dts, keyframe: p.keyframe }),
            )
            .await;
            sent.expect("muxer stopped consuming — it is wedged").expect("muxer dropped its receiver");
        }
    }

    // End it the way the real pipeline ends it: the encoder goes away. No explicit stop.
    drop(packet_tx);
    tokio::time::timeout(Duration::from_secs(20), muxer)
        .await
        .expect("muxer did not finish")
        .expect("muxer panicked")
        .expect("muxer errored");
    out
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn full_screen_recording_is_decodable() {
    let h = TestHarness::new().await;
    let out = record_with_insets(&h, 0, 0, 0, 0).await;
    let (w, hgt, frames) = decode(&out);
    assert_eq!((w, hgt), (SCREEN_W, SCREEN_H));
    assert!(frames > 0, "container opened but decoded no frames");
    h.shutdown().await;
}

/// The cropped path all the way through: the file must be the cropped size and play.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn cropped_recording_is_decodable_at_the_cropped_size() {
    let h = TestHarness::new().await;
    let out = record_with_insets(&h, 100, 60, 180, 40).await;
    let (w, hgt, frames) = decode(&out);
    assert_eq!((w, hgt), (SCREEN_W - 280, SCREEN_H - 100), "file must be the cropped size");
    assert!(frames > 0, "container opened but decoded no frames");
    h.shutdown().await;
}

/// Odd insets must still yield an encodable, playable file: H.264 4:2:0 cannot represent
/// an odd dimension, so the rounding has to happen before the encoder is opened.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn an_odd_crop_still_produces_a_playable_file() {
    let h = TestHarness::new().await;
    let out = record_with_insets(&h, 33, 17, 45, 21).await;
    let (w, hgt, frames) = decode(&out);
    assert_eq!(w % 2, 0, "width {w} is odd");
    assert_eq!(hgt % 2, 0, "height {hgt} is odd");
    assert!(frames > 0);
    h.shutdown().await;
}

/// **The window the save-offer race lived in.**
///
/// While a recording is in progress the file on disk is real H.264 with no `moov` atom —
/// it exists, it has a plausible size, and nothing can play it. `bsr-ui` used to open its
/// save modal and send `OfferSaveCopy` the instant Stop was pressed, with the muxer
/// shutdown detached on another task, so a quick operator could copy the file during
/// exactly this window and keep the broken copy forever.
///
/// This asserts the window is real, and that it closes only when the muxer task
/// completes. Deterministic: the "before" check happens with no stop requested at all,
/// so no trailer can possibly have been written yet.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn a_recording_is_not_playable_until_the_muxer_finishes() {
    let dir = tempfile::tempdir().expect("temp dir");
    let cfg = bsr_ipc::MuxerConfig {
        base_output_path: dir.path().to_path_buf(),
        file_naming_strategy: bsr_ipc::FileNamingStrategy::Simple("mid.mp4".into()),
        max_duration: Duration::from_secs(3600),
        fps: FPS,
        width: SCREEN_W,
        height: SCREEN_H,
    };
    let out = cfg.preview_output_path();

    let enc_cfg = EncoderConfig {
        codec: "h264".into(), preset: "ultrafast".into(), bitrate_kbps: 2500,
        width: SCREEN_W, height: SCREEN_H, fps: FPS,
    };
    let mut enc = H264EncoderBackend::new(&enc_cfg).expect("encoder init");
    enc.initialize(&enc_cfg).expect("encoder initialize");

    let (packet_tx, packet_rx) = tokio::sync::mpsc::channel(64);
    let (telemetry_tx, _t) = tokio::sync::mpsc::channel(4);
    let (_cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(1);
    let (ipc_cmd_tx, _i) = tokio::sync::mpsc::channel(32);
    let service = MuxerService::new(
        cfg, Mp4Muxer::new(), packet_rx, telemetry_tx, cmd_rx, bsr_ipc::IpcClient::new(ipc_cmd_tx),
    );
    let muxer = tokio::spawn(async move { service.run().await });

    for i in 0..FRAMES {
        if let Some(p) = enc.encode_frame(&screen_frame(i)).expect("encode") {
            packet_tx
                .send(EncodedPacket { data: p.data, pts: p.pts, dts: p.dts, keyframe: p.keyframe })
                .await
                .expect("muxer accepts packets");
        }
    }

    // Mid-recording: bytes on disk, nothing requested a trailer, so it must NOT open.
    let mid_len = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
    assert!(mid_len > 0, "the muxer should have written packet data by now");
    ffmpeg::init().expect("ffmpeg init");
    assert!(
        ffmpeg::format::input(&out).is_err(),
        "a recording still in progress must not be playable ({mid_len} bytes) — if this \
         ever passes, the save-offer race stops being detectable here"
    );

    // Finishing the muxer is what makes it playable. This is the signal `bsr-ui` now
    // waits for before offering the file to anyone.
    drop(packet_tx);
    tokio::time::timeout(Duration::from_secs(20), muxer)
        .await
        .expect("muxer did not finish")
        .expect("muxer panicked")
        .expect("muxer errored");

    let (w, h, frames) = decode(&out);
    assert_eq!((w, h), (SCREEN_W, SCREEN_H));
    assert!(frames > 0, "playable only after the muxer completes");
}

/// Container duration in seconds, as any player would read it.
fn container_duration_secs(path: &std::path::Path) -> f64 {
    ffmpeg::init().expect("ffmpeg init");
    let ictx = ffmpeg::format::input(&path).expect("open produced file");
    // `duration()` is in AV_TIME_BASE units (microseconds).
    ictx.duration() as f64 / 1_000_000.0
}

/// **A recording must be as long as the recording actually was.**
///
/// Capture cannot always sustain the configured fps: a damage-driven PipeWire
/// stream on a quiet desktop repeats slowly, and at 1080p the BGRA->YUV420P
/// scale plus x264 pass can exceed one frame interval outright. Measured on the
/// reference box, x264 preset "fast" sustains 21.6 fps at 1080p against a 30 fps
/// target.
///
/// While pts was a frame counter, the file's duration was always
/// `frames / fps` no matter how long the recording ran, so an underrun wrote a
/// short file that played back sped up, with nothing reported as wrong.
/// Observed on a packaged 1.0.0-4 install: 17 s of recording, 221 frames, muxed
/// as a 7.33 s file — roughly 2.3x too fast.
///
/// This drives the real encoder and the real `MuxerService` with frames arriving
/// at a genuine 10 fps against a 30 fps configuration, and reads the duration
/// back out of the container.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn file_duration_matches_real_capture_time_when_capture_underruns() {
    const N: usize = 30;
    const STEP_NS: u64 = 100_000_000; // 100 ms apart => 10 fps of real capture
    let real_secs = (N as f64 - 1.0) * STEP_NS as f64 / 1e9; // 2.9 s

    let dir = tempfile::tempdir().expect("temp dir");
    let cfg = bsr_ipc::MuxerConfig {
        base_output_path: dir.path().to_path_buf(),
        file_naming_strategy: bsr_ipc::FileNamingStrategy::Simple("slow.mp4".into()),
        max_duration: Duration::from_secs(3600),
        fps: FPS,
        width: SCREEN_W,
        height: SCREEN_H,
        ..Default::default()
    };
    let out = cfg.preview_output_path();

    let enc_cfg = EncoderConfig {
        codec: "h264".into(),
        preset: "ultrafast".into(),
        bitrate_kbps: 2500,
        width: SCREEN_W,
        height: SCREEN_H,
        fps: FPS,
    };
    let mut enc = H264EncoderBackend::new(&enc_cfg).expect("encoder init");
    enc.initialize(&enc_cfg).expect("encoder initialize");

    let (packet_tx, packet_rx) = tokio::sync::mpsc::channel(64);
    let (telemetry_tx, _telemetry_rx) = tokio::sync::mpsc::channel(4);
    let (_cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(1);
    let (ipc_cmd_tx, _ipc_cmd_rx) = tokio::sync::mpsc::channel(32);
    let service = MuxerService::new(
        cfg, Mp4Muxer::new(), packet_rx, telemetry_tx, cmd_rx,
        bsr_ipc::IpcClient::new(ipc_cmd_tx),
    );
    let muxer = tokio::spawn(async move { service.run().await });

    let base = 1_700_000_000_000_000_000u64;
    for i in 0..N {
        let mut frame = screen_frame(i);
        frame.timestamp = base + i as u64 * STEP_NS;
        if let Some(p) = enc.encode_frame(&frame).expect("encode_frame") {
            packet_tx
                .send(EncodedPacket { data: p.data, pts: p.pts, dts: p.dts, keyframe: p.keyframe })
                .await
                .expect("muxer dropped its receiver");
        }
    }
    drop(packet_tx);
    tokio::time::timeout(Duration::from_secs(20), muxer)
        .await
        .expect("muxer did not finish")
        .expect("muxer panicked")
        .expect("muxer errored");

    let (w, h, frames) = decode(&out);
    assert_eq!((w, h), (SCREEN_W, SCREEN_H));
    assert!(frames > 0, "container opened but decoded no frames");

    let got = container_duration_secs(&out);
    let counter_secs = N as f64 / FPS as f64; // 1.0 s — what a frame counter writes
    assert!(
        (got - real_secs).abs() < 0.35,
        "file reports {got:.2}s for {real_secs:.2}s of real capture \
         ({N} frames, 100 ms apart, at a {FPS} fps configuration). A frame \
         counter would write ~{counter_secs:.2}s and play back \
         {:.1}x too fast.",
        real_secs / got.max(0.001)
    );
}
