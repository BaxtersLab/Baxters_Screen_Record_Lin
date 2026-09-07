// SPDX-License-Identifier: MIT
//! Two overlapping ScreenCast sessions, and — the part that used to hang — shutting
//! both of them down. `PortalConnection::drop` joined its D-Bus thread with no bound;
//! this probe sat in that join for over 150 s before the join was made bounded.
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

    let mut a = PortalCaptureBackend::new();
    if let Err(e) = a.initialize().await {
        println!("A failed: {e}");
        return;
    }
    println!("A frames: {}", frames("A", &mut a, 5).await);

    let mut b = PortalCaptureBackend::new();
    if let Err(e) = b.initialize().await {
        println!("B failed to initialise: {e}");
        return;
    }
    println!("B frames: {}", frames("B", &mut b, 5).await);

    let t = Instant::now();
    let _ = a.shutdown().await;
    println!("A.shutdown() in {:?}", t.elapsed());

    let t = Instant::now();
    let _ = b.shutdown().await;
    println!("B.shutdown() in {:?}", t.elapsed());

    println!("[done] both sessions closed without hanging");
}
