// SPDX-License-Identifier: MIT
//! The field sequence: an idle live-view session runs for a few seconds, is shut
//! down, and a recording session is opened immediately afterwards.
//!
//! ```sh
//! env -u GDK_BACKEND cargo run -p bsr-capture --example two_sessions
//! ```

use bsr_capture::portal_backend::PortalCaptureBackend;
use bsr_capture::CaptureBackend;
use std::time::{Duration, Instant};

async fn frames(tag: &str, b: &mut PortalCaptureBackend, n: usize) -> usize {
    let mut ok = 0;
    for _ in 0..n {
        match b.capture_frame().await {
            Ok(_) => ok += 1,
            Err(e) => {
                println!("  [{tag}] capture_frame failed: {e}");
                break;
            }
        }
        tokio::time::sleep(Duration::from_millis(33)).await;
    }
    ok
}

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() {
    tracing_subscriber::fmt().with_max_level(tracing::Level::INFO).init();
    let t0 = Instant::now();
    macro_rules! step {
        ($($a:tt)*) => { println!("[{:>8.3}s] {}", t0.elapsed().as_secs_f64(), format!($($a)*)) };
    }

    step!("A.initialize() — the idle live view");
    let mut a = PortalCaptureBackend::new();
    if let Err(e) = a.initialize().await {
        step!("A failed: {e}");
        return;
    }
    step!("A live; running it for 7s like the operator does");
    step!("A frames: {}", frames("A", &mut a, 20).await);
    tokio::time::sleep(Duration::from_secs(5)).await;

    step!("A.shutdown() — what pressing Record triggers first");
    a.shutdown().await.ok();
    step!("A.shutdown() returned");

    step!("B.initialize() — the recording session");
    let mut b = PortalCaptureBackend::new();
    match b.initialize().await {
        Ok(()) => step!("B initialised"),
        Err(e) => {
            step!("B FAILED: {e}");
            return;
        }
    }
    step!("B frames: {} / 30", frames("B", &mut b, 30).await);
    step!("done");
    std::process::exit(0);
}
