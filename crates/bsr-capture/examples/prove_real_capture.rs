// SPDX-License-Identifier: MIT
//! Proves the Linux capture backend records **the real screen**.
//!
//! "A file exists" and "the smoke test passed" prove nothing here — the backend this
//! port replaced produced a file every single time, full of frames it invented. So this
//! tool does not assert that frames arrived. It puts a **known target** on the screen,
//! changes it, and asserts the captured pixels followed.
//!
//! Three independent checks, each of which the old mock backend fails:
//!
//! 1. **Non-uniform.** A solid-colour capture is indistinguishable from a mock and from
//!    the black frame an X11 grab returns under Wayland. The old mock painted every
//!    pixel `B=n, G=128, R=128`; a real desktop does not.
//! 2. **Known target present.** A window showing a known solid colour must appear in the
//!    capture as a large run of that colour.
//! 3. **Known target changed.** The window's colour is changed and the capture must
//!    follow it. This is the check a static screenshot, a stale buffer or a frozen
//!    PipeWire stream cannot pass.
//!
//! Run it:
//! ```text
//! cd ~/workspace/Baxters_Screen_Record_Linux && cargo run -p bsr-capture --example prove_real_capture
//! ```
//! On the first run GNOME shows its screen-share picker — pick a monitor and click
//! Share. Later runs reuse the cached restore token and are silent.
//!
//! The target is an ordinary, decorated, 640x480 `gst-launch-1.0` window. It is
//! deliberately not fullscreen, always-on-top or edge-reserving: Article XIV forbids
//! testing that class of surface against the live session.

use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::time::Duration;

use bsr_capture::{CaptureBackend, CaptureFrame};

/// Fraction of the screen the target window must occupy to count as "found".
/// The window is 640x480 on a 1920x1080 screen = 15.4% of pixels; a threshold of 2%
/// tolerates it being partly covered while staying far above any incidental colour.
const MIN_TARGET_FRACTION: f64 = 0.02;

/// Per-channel tolerance when matching the target colour. The compositor may apply
/// colour management, and videoconvert rounds; exact equality is too brittle.
const COLOUR_TOLERANCE: u8 = 24;

struct Phase {
    name: &'static str,
    /// videotestsrc pattern name.
    pattern: &'static str,
    /// Expected colour as (B, G, R) — the capture is BGRA.
    bgr: (u8, u8, u8),
}

const PHASES: [Phase; 2] = [
    Phase { name: "red", pattern: "red", bgr: (0, 0, 255) },
    Phase { name: "blue", pattern: "blue", bgr: (255, 0, 0) },
];

/// Kills the target window when dropped, however this program exits.
struct Target(Child);

impl Drop for Target {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

fn show_target(pattern: &str) -> Result<Target, String> {
    let child = Command::new("gst-launch-1.0")
        .args([
            "videotestsrc",
            &format!("pattern={pattern}"),
            "!",
            "video/x-raw,width=640,height=480,framerate=10/1",
            "!",
            "videoconvert",
            "!",
            "autovideosink",
        ])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| format!("spawn gst-launch-1.0 (is gstreamer1.0-tools installed?): {e}"))?;
    Ok(Target(child))
}

/// Count pixels within tolerance of the target colour, and measure how uniform the
/// frame is. `spread` is the number of distinct quantised colours seen — a mock or a
/// black frame collapses to 1.
fn analyse(frame: &CaptureFrame, want: (u8, u8, u8)) -> (f64, usize) {
    let near = |a: u8, b: u8| a.abs_diff(b) <= COLOUR_TOLERANCE;
    let mut hits = 0usize;
    let mut seen = std::collections::HashSet::new();
    for px in frame.data.chunks_exact(4) {
        let (b, g, r) = (px[0], px[1], px[2]);
        if near(b, want.0) && near(g, want.1) && near(r, want.2) {
            hits += 1;
        }
        // Quantise to 5 bits per channel so sub-perceptual noise does not inflate this.
        seen.insert((b >> 3, g >> 3, r >> 3));
    }
    let total = (frame.data.len() / 4).max(1);
    (hits as f64 / total as f64, seen.len())
}

fn write_png(frame: &CaptureFrame, path: &PathBuf) -> Result<(), String> {
    // BGRA -> RGBA for the encoder.
    let mut rgba = Vec::with_capacity(frame.data.len());
    for px in frame.data.chunks_exact(4) {
        rgba.extend_from_slice(&[px[2], px[1], px[0], 255]);
    }
    let buf = image::RgbaImage::from_raw(frame.width, frame.height, rgba)
        .ok_or("frame data does not match its stated dimensions")?;
    buf.save(path).map_err(|e| format!("write {}: {e}", path.display()))
}

#[tokio::main(flavor = "current_thread")]
async fn main() -> Result<(), String> {
    tracing_subscriber::fmt().with_max_level(tracing::Level::INFO).init();

    let out_dir = std::env::var("BSR_PROOF_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|_| std::env::temp_dir().join("bsr-capture-proof"));
    std::fs::create_dir_all(&out_dir).map_err(|e| format!("create {}: {e}", out_dir.display()))?;

    println!("Proof output: {}", out_dir.display());
    println!("If GNOME shows its screen-share picker, choose a monitor and click Share.\n");

    let mut backend = bsr_capture::platform_backend();
    backend.initialize().await.map_err(|e| format!("capture backend did not start: {e}"))?;

    let mut failures: Vec<String> = Vec::new();
    let mut results = Vec::new();

    for phase in &PHASES {
        let _target = show_target(phase.pattern)?;
        // Let the window map, get composited, and reach the PipeWire stream.
        tokio::time::sleep(Duration::from_millis(2500)).await;

        // Take several frames and keep the best; the window may still be animating in.
        let mut best: Option<(f64, usize, CaptureFrame)> = None;
        for _ in 0..12 {
            let frame = backend
                .capture_frame()
                .await
                .map_err(|e| format!("[{}] capture_frame: {e}", phase.name))?;
            let (fraction, spread) = analyse(&frame, phase.bgr);
            if best.as_ref().is_none_or(|(bf, _, _)| fraction > *bf) {
                best = Some((fraction, spread, frame));
            }
            tokio::time::sleep(Duration::from_millis(120)).await;
        }

        let (fraction, spread, frame) = best.expect("at least one frame was captured");
        let png = out_dir.join(format!("phase-{}.png", phase.name));
        write_png(&frame, &png)?;

        println!(
            "[{}] {}x{}  target colour BGR{:?}: {:.2}% of pixels  |  distinct colours: {}\n      -> {}",
            phase.name,
            frame.width,
            frame.height,
            phase.bgr,
            fraction * 100.0,
            spread,
            png.display()
        );

        // Check 1 — non-uniform. A mock or a black X11 grab collapses to a handful.
        if spread < 50 {
            failures.push(format!(
                "[{}] frame has only {spread} distinct colours — that is a uniform field, \
                 not a desktop. This is what a mock backend and an X11-under-Wayland grab \
                 both look like.",
                phase.name
            ));
        }
        // Check 2 — the known target is actually in the picture.
        if fraction < MIN_TARGET_FRACTION {
            failures.push(format!(
                "[{}] only {:.2}% of the captured pixels match the target colour BGR{:?} \
                 (need {:.0}%). The target window was on screen; the capture did not see it.",
                phase.name,
                fraction * 100.0,
                phase.bgr,
                MIN_TARGET_FRACTION * 100.0
            ));
        }
        results.push((phase.name, fraction, frame));
    }

    // Check 3 — the capture followed the change. Each phase must match its own colour
    // far better than the other phase's frame does.
    let (name_a, frac_a, frame_a) = &results[0];
    let (name_b, frac_b, frame_b) = &results[1];
    let (cross_a, _) = analyse(frame_a, PHASES[1].bgr);
    let (cross_b, _) = analyse(frame_b, PHASES[0].bgr);
    println!(
        "\nchange check: {name_a} frame is {:.2}% {name_a} vs {:.2}% {name_b}; \
         {name_b} frame is {:.2}% {name_b} vs {:.2}% {name_a}",
        frac_a * 100.0,
        cross_a * 100.0,
        frac_b * 100.0,
        cross_b * 100.0
    );
    if *frac_a <= cross_a || *frac_b <= cross_b {
        failures.push(
            "the capture did not follow the target's colour change — the two phases are \
             not distinguishable. A frozen PipeWire stream, a cached screenshot or a \
             synthetic source all look like this."
                .into(),
        );
    }

    backend.shutdown().await.map_err(|e| format!("shutdown: {e}"))?;

    if failures.is_empty() {
        println!("\nPASS — the capture backend recorded the real screen, and followed it when it changed.");
        Ok(())
    } else {
        println!("\nFAIL:");
        for f in &failures {
            println!("  - {f}");
        }
        Err(format!("{} check(s) failed", failures.len()))
    }
}
