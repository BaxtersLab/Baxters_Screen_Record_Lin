// SPDX-License-Identifier: MIT
//! The record space, across every crate that has an opinion about it.
//!
//! Cropping is agreed on by four crates: `bsr-core` owns the region type and its
//! normalisation, `bsr-ui` turns operator insets into a region and predicts the output
//! size, `bsr-capture` actually cuts the pixels, and `bsr-encode`/`bsr-mux` are built for
//! the size that comes out. Each crate tests its own half; nothing tested that they
//! agree. A disagreement here is not cosmetic — the encoder is opened at a fixed size and
//! refuses any frame that is not exactly it, so the whole recording produces no frames.

mod common;

use bsr_capture::{crop_frame, CaptureFrame, FrameFormat};
use bsr_core::config::CaptureRegion;
use bsr_ipc::IpcCommand;
use bsr_ui::UiSettings;
use common::TestHarness;

fn frame(w: u32, h: u32) -> CaptureFrame {
    CaptureFrame {
        data: vec![0u8; (w * h * 4) as usize],
        timestamp: 0,
        width: w,
        height: h,
        format: FrameFormat::Bgra8,
    }
}

/// The size the UI promises must be the size capture delivers.
#[test]
fn the_uis_predicted_size_matches_what_capture_actually_produces() {
    for (l, t, r, b) in [(0, 0, 0, 0), (100, 50, 300, 150), (7, 3, 9, 5), (1, 1, 1, 1)] {
        let settings = UiSettings {
            crop_left: l, crop_top: t, crop_right: r, crop_bottom: b,
            ..UiSettings::default()
        };
        let (pred_w, pred_h) = settings.recording_size();

        let full = frame(1920, 1080);
        let cropped = match settings.to_capture_region() {
            Some(region) => crop_frame(&full, &region),
            None => full.clone(),
        };

        assert_eq!(
            (cropped.width, cropped.height),
            (pred_w, pred_h),
            "insets {l},{t},{r},{b}: UI predicted {pred_w}x{pred_h} but capture produced {}x{}",
            cropped.width, cropped.height
        );
        assert_eq!(
            cropped.data.len(),
            (pred_w * pred_h * 4) as usize,
            "buffer must match the agreed dimensions, or the encoder reads garbage"
        );
    }
}

/// The muxer's container geometry must be the cropped size too, or the file declares
/// dimensions it does not contain.
#[test]
fn the_container_geometry_matches_the_cropped_frames() {
    let settings = UiSettings {
        crop_left: 200, crop_top: 100, crop_right: 200, crop_bottom: 100,
        ..UiSettings::default()
    };
    let cfg = settings.to_muxer_config();
    let cropped = crop_frame(&frame(1920, 1080), &settings.to_capture_region().unwrap());
    assert_eq!((cfg.width, cfg.height), (cropped.width, cropped.height));
}

/// A region an agent sends over IPC must arrive at the UI meaning the same rectangle.
#[tokio::test]
async fn a_region_set_over_ipc_arrives_unchanged() {
    let _guard = common::record_space_lock();
    // Drain anything a previous test left in the process-global slot.
    let _ = bsr_ipc::try_take_record_space();

    let h = TestHarness::new().await;
    let region = CaptureRegion { x1: 300, y1: 150, x2: 1500, y2: 900 };
    h.send(IpcCommand::SetRecordSpace { region: Some(region) }).await;

    let delivered = bsr_ipc::try_take_record_space().expect("region should reach the UI queue");
    assert_eq!(delivered, Some(region));

    let mut settings = UiSettings::default();
    settings.set_capture_region(delivered);
    assert_eq!(settings.recording_size(), (1200, 750));
    assert_eq!(settings.to_capture_region(), Some(region), "no drift through the UI");

    h.shutdown().await;
}

/// Clearing over IPC restores the full screen — the documented default.
#[tokio::test]
async fn clearing_the_region_over_ipc_restores_the_full_screen() {
    let _guard = common::record_space_lock();
    let _ = bsr_ipc::try_take_record_space();

    let h = TestHarness::new().await;
    h.send(IpcCommand::SetRecordSpace { region: None }).await;

    let delivered = bsr_ipc::try_take_record_space().expect("a clear is still a delivery");
    assert_eq!(delivered, None);

    let mut settings = UiSettings { crop_left: 100, ..UiSettings::default() };
    settings.set_capture_region(delivered);
    assert!(!settings.has_crop_insets());
    assert_eq!(settings.recording_size(), (1920, 1080));

    h.shutdown().await;
}
