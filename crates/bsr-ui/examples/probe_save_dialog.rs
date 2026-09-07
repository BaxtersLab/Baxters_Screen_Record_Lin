// SPDX-License-Identifier: MIT
//! Reproduce the "portal accepts and never answers" hang without a GUI.
//!
//! ashpd caches ONE session bus connection process-wide (`static SESSION: OnceLock<
//! zbus::Connection>`). zbus binds its socket-reader task to the runtime alive when that
//! connection is built. If the first ashpd call happens on a short-lived runtime — which
//! is exactly what bsr-capture's portal thread is — the cached connection outlives its
//! own reader, and every later request is written but never answered.
//!
//! `poison` reproduces that. `clean` is the control.
use std::time::{Duration, Instant};

async fn save_file() -> String {
    let t0 = Instant::now();
    let req = ashpd::desktop::file_chooser::SelectedFiles::save_file()
        .title("probe")
        .accept_label("Save")
        .modal(false)
        .current_name("probe.mp4");
    match tokio::time::timeout(Duration::from_secs(20), req.send()).await {
        Err(_) => format!("NO ANSWER after {:?} (the hang)", t0.elapsed()),
        Ok(Ok(r)) => match r.response() {
            Ok(f) => format!("answered in {:?}: {:?}", t0.elapsed(), f.uris()),
            Err(e) => format!("answered in {:?} with refusal: {e}", t0.elapsed()),
        },
        Ok(Err(e)) => format!("send error after {:?}: {e}", t0.elapsed()),
    }
}

fn main() {
    let mode = std::env::args().nth(1).unwrap_or_else(|| "clean".into());

    if mode == "poison" {
        // Exactly what bsr-capture does: a dedicated thread with its own current-thread
        // runtime, which then goes away. The first ashpd connection is built here.
        let t = std::thread::spawn(|| {
            let rt = tokio::runtime::Builder::new_current_thread().enable_all().build().unwrap();
            rt.block_on(async {
                // Any portal proxy initialises the cached SESSION connection.
                match ashpd::desktop::screencast::Screencast::new().await {
                    Ok(_) => eprintln!("[poison] cached connection built on the temporary runtime"),
                    Err(e) => eprintln!("[poison] proxy failed: {e}"),
                }
            });
            // Runtime dropped here; its tasks — including zbus's reader — die with it.
        });
        t.join().unwrap();
        eprintln!("[poison] temporary runtime is gone");
    }

    let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(4).enable_all().build().unwrap();
    println!("[{mode}] {}", rt.block_on(save_file()));
}
