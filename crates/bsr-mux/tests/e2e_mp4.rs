// SPDX-License-Identifier: MIT
// Live end-to-end: encode real frames with the H.264 encoder, mux them to an MP4
// with the real Mp4Muxer, then INDEPENDENTLY demux + decode the produced file and
// confirm it contains decodable H.264 video of the correct dimensions.
//
// Analogous to BSM's e2e_record.rs: it proves the recorder produces a valid,
// decodable output file — not just a structurally-present container.

use bsr_capture::{CaptureFrame, FrameFormat};
use bsr_encode::{EncoderConfig, H264EncoderBackend};
use bsr_mux::backends::mp4::Mp4Muxer;
use bsr_mux::{EncodedPacket, MuxerBackend};
use ffmpeg_next as ffmpeg;

const W: u32 = 320;
const H: u32 = 240;
const FPS: u32 = 30;
const N_FRAMES: usize = 60;

/// A moving BGRA gradient so successive frames genuinely differ (real encode,
/// not a trivially-compressible constant image).
fn make_frame(i: usize) -> CaptureFrame {
    let mut data = vec![0u8; (W * H * 4) as usize];
    for y in 0..H as usize {
        for x in 0..W as usize {
            let idx = (y * W as usize + x) * 4;
            data[idx] = ((x + i * 3) & 0xFF) as u8; // B
            data[idx + 1] = ((y + i * 2) & 0xFF) as u8; // G
            data[idx + 2] = ((x + y + i) & 0xFF) as u8; // R
            data[idx + 3] = 0xFF; // A
        }
    }
    CaptureFrame { data, timestamp: i as u64, width: W, height: H, format: FrameFormat::Bgra8 }
}

#[tokio::test]
async fn e2e_encode_mux_mp4_is_decodable() {
    let tmp = tempfile::tempdir().unwrap();
    let out = tmp.path().join("e2e.mp4");

    // 1) Encode real frames -> H.264 packets.
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

    let mut packets: Vec<EncodedPacket> = Vec::new();
    for i in 0..N_FRAMES {
        let frame = make_frame(i);
        if let Some(p) = enc.encode_frame(&frame).expect("encode_frame") {
            packets.push(EncodedPacket { data: p.data, pts: p.pts, dts: p.dts, keyframe: p.keyframe });
        }
    }
    assert!(!packets.is_empty(), "encoder produced no packets");
    let n_packets = packets.len();

    // 2) Mux to MP4 with the real muxer.
    let mux_cfg = bsr_ipc::MuxerConfig {
        base_output_path: tmp.path().to_path_buf(),
        file_naming_strategy: bsr_ipc::FileNamingStrategy::Simple("e2e.mp4".into()),
        max_duration: std::time::Duration::from_secs(60),
        fps: FPS,
        width: W,
        height: H,
    };
    let mut muxer = Mp4Muxer::new();
    muxer.initialize(&mux_cfg).await.expect("muxer init");
    for p in packets {
        muxer.write_packet(p).await.expect("write_packet");
    }
    muxer.finalize().await.expect("finalize");

    // 3) The file must be non-degenerate (a real ~60-frame encode is several KB,
    //    not the 48-261 byte header-only stubs).
    let size = std::fs::metadata(&out).expect("output exists").len();
    assert!(size > 1024, "MP4 suspiciously small: {size} bytes (broken mux?)");

    // 4) INDEPENDENT decode-verify: demux + decode the produced file back to raw
    //    frames. A container missing SPS/PPS extradata would decode 0 frames.
    ffmpeg::init().unwrap();
    let mut ictx = ffmpeg::format::input(&out).expect("open muxed mp4");

    // Playback timing must be correct: a 60-frame clip at 30 fps is ~2 s, not the
    // ~0.66 ms a 1/90000 time_base with frame-index pts would produce.
    let container_duration_s = ictx.duration() as f64 / 1_000_000.0;
    let expected_s = N_FRAMES as f64 / FPS as f64;

    let (vindex, decoder_ctx) = {
        let vstream = ictx
            .streams()
            .best(ffmpeg::media::Type::Video)
            .expect("no video stream in output");
        let idx = vstream.index();
        let ctx = ffmpeg::codec::context::Context::from_parameters(vstream.parameters())
            .expect("codec params");
        (idx, ctx)
    };
    let mut decoder = decoder_ctx.decoder().video().expect("open decoder");
    assert_eq!(decoder.format(), ffmpeg::format::Pixel::YUV420P, "unexpected decoded pixfmt");

    let mut decoded = 0usize;
    let mut frame = ffmpeg::util::frame::Video::empty();
    for (stream, packet) in ictx.packets() {
        if stream.index() == vindex && decoder.send_packet(&packet).is_ok() {
            while decoder.receive_frame(&mut frame).is_ok() {
                assert_eq!(frame.width(), W, "decoded width mismatch");
                assert_eq!(frame.height(), H, "decoded height mismatch");
                decoded += 1;
            }
        }
    }
    decoder.send_eof().ok();
    while decoder.receive_frame(&mut frame).is_ok() {
        assert_eq!(frame.width(), W);
        assert_eq!(frame.height(), H);
        decoded += 1;
    }

    eprintln!(
        "e2e: {n_packets} packets muxed, {decoded} frames decoded, {size} bytes, \
         container duration {container_duration_s:.3}s (expect ~{expected_s:.2}s)"
    );
    assert!(decoded > 0, "no frames decoded from the muxed MP4 — container is not decodable");
    // Most muxed frames should survive a decode round-trip.
    assert!(decoded >= n_packets / 2, "decoded {decoded} << muxed {n_packets}: lossy/broken mux");
    // Playback timing must be sane (catches the frame-index-pts / 1-90000-timebase bug).
    assert!(
        container_duration_s > expected_s * 0.5 && container_duration_s < expected_s * 2.0,
        "MP4 duration {container_duration_s:.3}s is wildly off expected {expected_s:.2}s \
         (timestamp/time_base bug)"
    );
}
