// SPDX-License-Identifier: MIT
//! Cropping the record space.
//!
//! Size alone proves nothing here: a crop that returns a correctly-sized rectangle of
//! the WRONG pixels looks perfect in every dimension check and is completely broken.
//! These tests paint a frame where every pixel encodes its own coordinates, so the
//! copied block can be checked against where it actually came from.

use bsr_capture::{crop_frame, CaptureFrame, FrameFormat};
use bsr_core::config::CaptureRegion;

/// Each pixel stores its own (x, y) in B and G, so any misplaced row or column shows up.
fn coordinate_frame(w: u32, h: u32) -> CaptureFrame {
    let mut data = vec![0u8; (w * h * 4) as usize];
    for y in 0..h as usize {
        for x in 0..w as usize {
            let i = (y * w as usize + x) * 4;
            data[i] = (x % 251) as u8;     // B
            data[i + 1] = (y % 251) as u8; // G
            data[i + 2] = 0x40;            // R
            data[i + 3] = 0xFF;            // A
        }
    }
    CaptureFrame { data, timestamp: 7, width: w, height: h, format: FrameFormat::Bgra8 }
}

fn px(frame: &CaptureFrame, x: u32, y: u32) -> (u8, u8) {
    let i = ((y * frame.width + x) * 4) as usize;
    (frame.data[i], frame.data[i + 1])
}

fn region(x1: i32, y1: i32, x2: i32, y2: i32) -> CaptureRegion {
    CaptureRegion { x1, y1, x2, y2 }
}

#[test]
fn crop_takes_the_pixels_it_was_asked_for() {
    let frame = coordinate_frame(1920, 1080);
    let out = crop_frame(&frame, &region(100, 50, 900, 650));

    assert_eq!((out.width, out.height), (800, 600));
    assert_eq!(out.data.len(), 800 * 600 * 4, "buffer must match the new dimensions");

    // Top-left of the crop is (100, 50) of the original, and the far corner follows.
    assert_eq!(px(&out, 0, 0), ((100 % 251) as u8, (50 % 251) as u8));
    assert_eq!(px(&out, 799, 599), (((100 + 799) % 251) as u8, ((50 + 599) % 251) as u8));
    // A row in the middle must not be sheared: source stride is 1920, destination 800.
    assert_eq!(px(&out, 400, 300), (((100 + 400) % 251) as u8, ((50 + 300) % 251) as u8));
}

#[test]
fn cropping_all_four_corners_in_gives_the_middle() {
    // "Crop the corners" in its plainest form: pull every edge inward by 200px.
    let frame = coordinate_frame(1920, 1080);
    let out = crop_frame(&frame, &region(200, 200, 1720, 880));
    assert_eq!((out.width, out.height), (1520, 680));
    assert_eq!(px(&out, 0, 0), ((200 % 251) as u8, (200 % 251) as u8));
}

#[test]
fn frame_metadata_survives_the_crop() {
    let frame = coordinate_frame(640, 480);
    let out = crop_frame(&frame, &region(0, 0, 320, 240));
    assert_eq!(out.timestamp, frame.timestamp, "timestamp must be preserved");
    assert_eq!(out.format, frame.format, "pixel format must be preserved");
}

/// The safe direction. A region that resolves to nothing must record too much rather
/// than nothing at all -- and must never read past the buffer.
#[test]
fn unusable_regions_fall_back_to_the_whole_frame() {
    let frame = coordinate_frame(640, 480);
    for (name, r) in [
        ("entirely off-screen", region(5000, 5000, 6000, 6000)),
        ("zero area", region(10, 10, 10, 10)),
        ("sub-pixel", region(10, 10, 11, 11)),
    ] {
        let out = crop_frame(&frame, &r);
        assert_eq!((out.width, out.height), (640, 480), "{name} should be a no-op");
        assert_eq!(out.data.len(), frame.data.len(), "{name}");
    }
}

#[test]
fn oversized_region_is_clamped_rather_than_overrunning_the_buffer() {
    let frame = coordinate_frame(640, 480);
    let out = crop_frame(&frame, &region(-1000, -1000, 9000, 9000));
    assert_eq!((out.width, out.height), (640, 480));
    assert_eq!(px(&out, 0, 0), (0, 0));
}

/// A frame whose buffer is shorter than its declared size must not be cropped.
/// Cropping it would read off the end of the allocation.
#[test]
fn a_frame_smaller_than_it_claims_is_not_cropped() {
    let mut frame = coordinate_frame(640, 480);
    frame.data.truncate(1000);
    let out = crop_frame(&frame, &region(0, 0, 320, 240));
    assert_eq!((out.width, out.height), (640, 480), "must refuse rather than overrun");
}

/// Every crop must produce H.264-encodable dimensions. An odd width or height cannot be
/// represented in 4:2:0 and fails at Record time, long after the value was typed.
#[test]
fn every_crop_is_even_sized_so_h264_can_encode_it() {
    let frame = coordinate_frame(1920, 1080);
    for (x1, y1, x2, y2) in [(0, 0, 801, 601), (3, 7, 1001, 903), (1, 1, 1919, 1079)] {
        let out = crop_frame(&frame, &region(x1, y1, x2, y2));
        assert_eq!(out.width % 2, 0, "width {} is odd for {:?}", out.width, (x1, y1, x2, y2));
        assert_eq!(out.height % 2, 0, "height {} is odd", out.height);
        assert_eq!(out.data.len(), (out.width * out.height * 4) as usize);
    }
}
