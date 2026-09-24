// SPDX-License-Identifier: MIT
// A recording that is never finalised must still play.
//
// WHY THIS EXISTS. The operator reported that recordings would not open in any
// player. Measured on their own files, 2026-09-24: 6 of 21 were unplayable with
// "moov atom not found", one of them 519 MB. Cause: a classic mp4 keeps its
// index at the END of the file, written by finalize(). Any stop that never
// reaches that call -- a crash, a kill, a power cut -- leaves every frame on
// disk and no index, and the whole recording is lost.
//
// The suite passed 170 tests while that was true, because every test finalised
// tidily. This test does NOT finalise: it writes packets and walks away, the
// way a killed process does, and then demands the file still decodes.

use bsr_capture::{CaptureFrame, FrameFormat};
use bsr_encode::{EncoderConfig, H264EncoderBackend};
use bsr_mux::backends::mp4::Mp4Muxer;
use bsr_mux::{EncodedPacket, MuxerBackend};
use ffmpeg_next as ffmpeg;

const W: u32 = 320;
const H: u32 = 240;
const FPS: u32 = 30;
const N_FRAMES: usize = 90; // three seconds, so several fragments close

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

async fn record_without_finalising(out_name: &str, dir: &std::path::Path) -> usize {
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

    // The parameter sets the container needs. A recorder that does not pass
    // these writes files that only open once fully finalised.
    let extradata = enc.extradata();
    assert!(extradata.is_some(), "encoder produced no parameter sets (GLOBAL_HEADER not honoured)");

    let mux_cfg = bsr_ipc::MuxerConfig {
        extradata: extradata.clone(),
        base_output_path: dir.to_path_buf(),
        file_naming_strategy: bsr_ipc::FileNamingStrategy::Simple(out_name.into()),
        max_duration: std::time::Duration::from_secs(60),
        fps: FPS,
        width: W,
        height: H,
    };
    let mut muxer = Mp4Muxer::new();
    muxer.initialize(&mux_cfg).await.expect("muxer init");

    let mut written = 0usize;
    for i in 0..N_FRAMES {
        if let Some(p) = enc.encode_frame(&make_frame(i)).expect("encode_frame") {
            muxer
                .write_packet(EncodedPacket { data: p.data, pts: p.pts, dts: p.dts, keyframe: p.keyframe })
                .await
                .expect("write_packet");
            written += 1;
        }
    }

    // THE POINT OF THIS TEST: no finalize(), no shutdown(). The muxer is simply
    // dropped, exactly as it would be if the process were killed.
    drop(muxer);
    written
}

#[tokio::test]
async fn a_recording_that_was_never_finalised_still_decodes() {
    let tmp = tempfile::tempdir().unwrap();
    let written = record_without_finalising("killed.mp4", tmp.path()).await;
    assert!(written > 0, "encoder produced no packets");

    let out = tmp.path().join("killed.mp4");
    if let Ok(keep) = std::env::var("BSR_KEEP_OUTPUT") { std::fs::copy(&out, &keep).ok(); }
    let size = std::fs::metadata(&out).expect("output exists").len();
    assert!(size > 1024, "file suspiciously small: {size} bytes");

    // It must open at all. Before the fragmented-mp4 fix this failed here with
    // "moov atom not found", which is exactly what the operator saw.
    ffmpeg::init().unwrap();
    let mut ictx = ffmpeg::format::input(&out)
        .expect("an unfinalised recording must still open — no moov atom means the recording is lost");

    let (vindex, decoder_ctx) = {
        let vstream = ictx.streams().best(ffmpeg::media::Type::Video).expect("no video stream");
        let idx = vstream.index();
        let ctx = ffmpeg::codec::context::Context::from_parameters(vstream.parameters())
            .expect("codec params");
        (idx, ctx)
    };
    let mut decoder = decoder_ctx.decoder().video().expect("open decoder");

    let mut decoded = 0usize;
    let mut frame = ffmpeg::util::frame::Video::empty();
    for (stream, packet) in ictx.packets() {
        if stream.index() == vindex && decoder.send_packet(&packet).is_ok() {
            while decoder.receive_frame(&mut frame).is_ok() {
                assert_eq!(frame.width(), W);
                assert_eq!(frame.height(), H);
                decoded += 1;
            }
        }
    }
    decoder.send_eof().ok();
    while decoder.receive_frame(&mut frame).is_ok() {
        decoded += 1;
    }

    // Not every frame need survive -- the last fragment may be incomplete --
    // but the bulk of the recording must, not zero.
    assert!(
        decoded * 2 >= written,
        "only {decoded} of {written} frames survived an unclean stop; the recording is effectively lost"
    );
}

#[tokio::test]
async fn the_file_is_self_describing_from_the_start() {
    // empty_moov writes the header up front; frag_keyframe closes a fragment at
    // each keyframe. Both must be present, or an interrupted file has nothing to
    // describe its contents.
    let tmp = tempfile::tempdir().unwrap();
    record_without_finalising("fragments.mp4", tmp.path()).await;
    let bytes = std::fs::read(tmp.path().join("fragments.mp4")).expect("read output");
    let has = |tag: &[u8]| bytes.windows(4).any(|w| w == tag);
    assert!(has(b"ftyp"), "no ftyp box: not an mp4 at all");
    assert!(has(b"moov"), "no moov at the start: empty_moov is not in effect");
    assert!(has(b"moof"), "no moof: the file is not fragmented, so an unclean stop loses everything");
}
