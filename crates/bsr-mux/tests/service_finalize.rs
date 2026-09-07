// SPDX-License-Identifier: MIT
//! Regression: `MuxerService` must write the MP4 trailer when the packet stream ends.
//!
//! The sibling `e2e_mp4.rs` drives the `Mp4Muxer` **backend** directly and calls
//! `finalize()` itself, so it passed while the real pipeline was broken. This test
//! drives `MuxerService`, the way `bsr-ui` and `bsr-real-smoke` do, and ends the
//! recording the way they actually end it: **by dropping the packet sender**, which is
//! what happens the instant the encoder service shuts down.
//!
//! Before the fix, the service's `None => break` arm exited the loop without
//! finalizing. The file was left with real H.264 in it and no `moov` atom, and ffprobe
//! reported "moov atom not found". Measured on a real run: 512 KB, completely
//! unopenable. `bsr-ui` shuts the encoder down 100 ms *before* it sends
//! `StopRecording`, so it lost the race every time.

use bsr_capture::{CaptureFrame, FrameFormat};
use bsr_encode::{EncoderConfig, H264EncoderBackend};
use bsr_mux::backends::mp4::Mp4Muxer;
use bsr_mux::{EncodedPacket, MuxerCommand, MuxerService};
use ffmpeg_next as ffmpeg;

const W: u32 = 320;
const H: u32 = 240;
const FPS: u32 = 30;
const N_FRAMES: usize = 45;

fn make_frame(i: usize) -> CaptureFrame {
    let mut data = vec![0u8; (W * H * 4) as usize];
    for y in 0..H as usize {
        for x in 0..W as usize {
            let idx = (y * W as usize + x) * 4;
            data[idx] = ((x + i * 3) & 0xFF) as u8;
            data[idx + 1] = ((y + i * 2) & 0xFF) as u8;
            data[idx + 2] = ((x + y + i) & 0xFF) as u8;
            data[idx + 3] = 0xFF;
        }
    }
    CaptureFrame { data, timestamp: i as u64, width: W, height: H, format: FrameFormat::Bgra8 }
}

fn encode_packets() -> Vec<EncodedPacket> {
    let cfg = EncoderConfig {
        codec: "h264".into(),
        preset: "ultrafast".into(),
        bitrate_kbps: 2000,
        width: W,
        height: H,
        fps: FPS,
    };
    let mut enc = H264EncoderBackend::new(&cfg).expect("encoder init");
    enc.initialize(&cfg).expect("encoder initialize");
    let mut packets = Vec::new();
    for i in 0..N_FRAMES {
        if let Some(p) = enc.encode_frame(&make_frame(i)).expect("encode_frame") {
            packets.push(EncodedPacket { data: p.data, pts: p.pts, dts: p.dts, keyframe: p.keyframe });
        }
    }
    assert!(!packets.is_empty(), "encoder produced no packets");
    packets
}

/// Independently demux + decode the file. This is the check that fails on a missing
/// trailer: without `moov`, `input()` cannot even open the container.
fn assert_decodable(path: &std::path::Path) {
    ffmpeg::init().expect("ffmpeg init");
    let mut ictx = ffmpeg::format::input(&path).unwrap_or_else(|e| {
        panic!(
            "produced file is not a readable MP4: {e}. A missing `moov` atom means the \
             muxer never wrote its trailer — the recording is unplayable even though \
             the file exists and contains real H.264. ({} bytes)",
            std::fs::metadata(path).map(|m| m.len()).unwrap_or(0)
        )
    });
    let stream = ictx
        .streams()
        .best(ffmpeg::media::Type::Video)
        .expect("no video stream in the produced file");
    let stream_index = stream.index();
    let ctx = ffmpeg::codec::Context::from_parameters(stream.parameters()).unwrap();
    let mut decoder = ctx.decoder().video().expect("video decoder");
    assert_eq!(decoder.width(), W, "decoded width");
    assert_eq!(decoder.height(), H, "decoded height");

    let mut decoded = 0usize;
    for (s, packet) in ictx.packets() {
        if s.index() != stream_index {
            continue;
        }
        if decoder.send_packet(&packet).is_ok() {
            let mut frame = ffmpeg::util::frame::Video::empty();
            while decoder.receive_frame(&mut frame).is_ok() {
                decoded += 1;
            }
        }
    }
    let _ = decoder.send_eof();
    let mut frame = ffmpeg::util::frame::Video::empty();
    while decoder.receive_frame(&mut frame).is_ok() {
        decoded += 1;
    }
    assert!(decoded > 0, "container opened but no frames decoded out of it");
}

/// Returns the `TempDir` guard alongside the path: the caller must hold the guard
/// until it has finished reading the file. An earlier version copied the output to
/// `temp_dir()/bsr-svc-finalize-<pid>.mp4`, but both tests in this binary run in
/// parallel threads of one process, so they raced on that single shared name — one
/// test's `remove_file` deleted the other's output mid-read ("No such file or
/// directory", 0 bytes) or its `copy` overwrote a file being decoded ("Invalid data
/// found"). Roughly one run in three failed, in whichever test lost.
async fn run_service_until_packets_end(
    finish: Option<MuxerCommand>,
) -> (tempfile::TempDir, std::path::PathBuf) {
    let tmp = tempfile::tempdir().expect("tempdir");
    let dir = tmp.path().to_path_buf();

    let mut config = bsr_ipc::MuxerConfig {
        base_output_path: dir.clone(),
        file_naming_strategy: bsr_ipc::FileNamingStrategy::Simple("svc.mp4".into()),
        ..Default::default()
    };
    config.width = W;
    config.height = H;
    config.fps = FPS;
    let out = config.preview_output_path();

    let (packet_tx, packet_rx) = tokio::sync::mpsc::channel(8);
    let (telemetry_tx, telemetry_rx) = tokio::sync::mpsc::channel(4);
    let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel(1);
    let (ipc_cmd_tx, _ipc_cmd_rx) = tokio::sync::mpsc::channel(32);
    let ipc_client = bsr_ipc::IpcClient::new(ipc_cmd_tx);

    // Deliberately never drained. Telemetry is best-effort and must NEVER be able to
    // wedge the muxer: with a blocking `send().await` on a capacity-4 channel, the
    // service stopped writing after 4 packets and then ignored its command channel
    // entirely, which is what made a stop request time out.
    let _telemetry_rx = telemetry_rx;

    let service = MuxerService::new(config, Mp4Muxer::new(), packet_rx, telemetry_tx, cmd_rx, ipc_client);
    let task = tokio::spawn(async move { service.run().await });

    for p in encode_packets() {
        packet_tx.send(p).await.expect("muxer stopped accepting packets");
    }

    // End the recording the way the real pipeline ends it: the encoder goes away.
    drop(packet_tx);
    if let Some(cmd) = finish {
        let _ = cmd_tx.send(cmd).await;
    }

    tokio::time::timeout(std::time::Duration::from_secs(20), task)
        .await
        .expect("muxer did not exit within 20s")
        .expect("muxer task panicked")
        .expect("muxer returned an error");

    assert!(
        out.exists(),
        "muxer produced no output at {}",
        out.display()
    );
    (tmp, out)
}

/// The real-world path: the encoder shuts down, its sender drops, and nothing sends an
/// explicit stop. The file must still be playable.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn packet_stream_ending_finalizes_the_mp4() {
    let (_tmp, path) = run_service_until_packets_end(None).await;
    assert_decodable(&path);
}

/// The explicit path, for completeness: an operator pressing Stop.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn explicit_stop_finalizes_the_mp4() {
    let (_tmp, path) = run_service_until_packets_end(Some(MuxerCommand::StopRecording)).await;
    assert_decodable(&path);
}
