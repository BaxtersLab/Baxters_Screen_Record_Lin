// SPDX-License-Identifier: MIT
//! Regression tests for the Linux capture backend.
//!
//! These exist because of a specific defect: `bsr-capture` shipped a synthetic
//! `mock_backend` under `#[cfg(not(windows))]`, so on Linux the *product* invented its
//! frames, logged success and passed every test. Everything here is aimed at the same
//! property from a different angle — **when this crate cannot see the screen, it must
//! fail; it must never produce a frame.**

use std::sync::{Arc, Mutex, OnceLock};

use bsr_capture::{CaptureBackend, CaptureConfig, CaptureService};
use bsr_core::buffer::DropOldestBuffer;

/// Environment variables are process-global while cargo runs tests on many threads.
/// Every test that mutates one holds this first.
fn env_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

/// Restores an environment variable to whatever it was, including "unset".
struct EnvGuard(&'static str, Option<std::ffi::OsString>);

impl EnvGuard {
    fn set(key: &'static str, value: &str) -> Self {
        let prev = std::env::var_os(key);
        std::env::set_var(key, value);
        Self(key, prev)
    }
    fn unset(key: &'static str) -> Self {
        let prev = std::env::var_os(key);
        std::env::remove_var(key);
        Self(key, prev)
    }
}

impl Drop for EnvGuard {
    fn drop(&mut self) {
        match self.1.take() {
            Some(v) => std::env::set_var(self.0, v),
            None => std::env::remove_var(self.0),
        }
    }
}

/// The product's backend must be the real one. If this alias is ever re-pointed at a
/// synthetic source, the whole recorder silently becomes a fiction generator — which is
/// exactly what it was doing on Linux before this port.
#[test]
fn platform_backend_is_the_real_portal_backend() {
    let name = std::any::type_name::<bsr_capture::PlatformCaptureBackend>();
    assert!(
        name.contains("PortalCaptureBackend"),
        "the Linux platform backend must be PortalCaptureBackend, got `{name}`. A synthetic \
         or mock backend must never be reachable from a product build."
    );
}

/// Asking for a frame without a session is an error, not a picture. A backend that
/// answers this call with *anything* is inventing it.
#[tokio::test]
async fn capture_frame_before_initialize_is_an_error() {
    let mut backend = bsr_capture::platform_backend();
    // Deliberately not `expect_err`: a frame is megabytes wide, and a failure here
    // would otherwise print the whole invented picture into the gate log.
    let msg = match backend.capture_frame().await {
        Ok(f) => panic!(
            "an uninitialised backend must not produce a frame, got {}x{} ({} bytes)",
            f.width,
            f.height,
            f.data.len()
        ),
        Err(e) => e.to_string(),
    };
    assert!(
        msg.contains("initialize"),
        "the error should say the session was never started, got: {msg}"
    );
}

/// The fail-closed path, exercised on a box where the portal *is* available.
/// `BSR_NO_PORTAL=1` must produce an error — never a fallback frame source.
#[tokio::test]
async fn initialize_refuses_when_the_portal_is_disabled() {
    let _guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let _no_portal = EnvGuard::set("BSR_NO_PORTAL", "1");

    let mut backend = bsr_capture::platform_backend();
    let err = backend
        .initialize()
        .await
        .expect_err("BSR_NO_PORTAL=1 must refuse to start a capture session");
    assert!(
        err.to_string().contains("BSR_NO_PORTAL"),
        "the refusal should name the reason, got: {err}"
    );

    // And it must still not hand out a frame afterwards.
    assert!(
        backend.capture_frame().await.is_err(),
        "a backend that failed to initialize must not produce frames"
    );
}

/// With no display at all there is nothing to capture, and the backend must say so
/// rather than starting and delivering black or synthetic frames.
#[tokio::test]
async fn initialize_refuses_without_a_graphical_session() {
    let _guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let _no_portal = EnvGuard::unset("BSR_NO_PORTAL");
    let _wayland = EnvGuard::unset("WAYLAND_DISPLAY");
    let _x11 = EnvGuard::unset("DISPLAY");

    let mut backend = bsr_capture::platform_backend();
    let err = backend
        .initialize()
        .await
        .expect_err("no WAYLAND_DISPLAY and no DISPLAY must refuse to start");
    assert!(
        err.to_string().contains("no graphical session"),
        "the refusal should name the reason, got: {err}"
    );
}

/// The property that matters most for the product: when capture cannot start, the
/// pipeline stops with an error and **nothing at all** reaches the frame buffer.
/// Before this port the equivalent path started a mock and filled the buffer with
/// invented frames, which the encoder then wrote to an MP4.
#[tokio::test]
async fn capture_service_fails_and_emits_no_frames_when_the_backend_cannot_start() {
    let _guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let _no_portal = EnvGuard::set("BSR_NO_PORTAL", "1");

    let config = CaptureConfig::default();
    let (telemetry_tx, _telemetry_rx) = tokio::sync::broadcast::channel(32);
    let (_shutdown_tx, shutdown_rx) = tokio::sync::mpsc::channel(1);
    let frame_buf = Arc::new(Mutex::new(DropOldestBuffer::new(config.buffer_capacity)));
    let frame_notify = Arc::new(tokio::sync::Notify::new());

    let service = CaptureService::new(
        bsr_capture::platform_backend(),
        config,
        telemetry_tx,
        shutdown_rx,
        frame_buf.clone(),
        frame_notify,
    );

    let result = service.run().await;
    assert!(
        result.is_err(),
        "CaptureService::run must return Err when the backend cannot initialize"
    );
    assert_eq!(
        frame_buf.lock().unwrap().len(),
        0,
        "a capture service that never started must not have pushed a single frame"
    );
}

/// An Xorg session must be refused, not silently recorded as black.
///
/// This is the failure the whole portal backend exists to avoid: under X11 a root-window
/// grab returns a uniform black frame with no error, so the recording looks successful
/// and contains nothing. Refusing is the only honest outcome.
#[tokio::test]
async fn initialize_refuses_an_xorg_session() {
    let _guard = env_lock().lock().unwrap_or_else(|e| e.into_inner());
    let _no_portal = EnvGuard::unset("BSR_NO_PORTAL");
    let _session = EnvGuard::set("XDG_SESSION_TYPE", "x11");
    // A display must be present, or the earlier "no graphical session" check fires first
    // and this would pass for the wrong reason.
    let _display = EnvGuard::set("DISPLAY", ":0");

    let mut backend = bsr_capture::platform_backend();
    let err = backend
        .initialize()
        .await
        .expect_err("an Xorg session must be refused");
    let msg = err.to_string();
    assert!(
        msg.contains("Xorg") && msg.contains("Wayland"),
        "the refusal must name the problem and the fix, got: {msg}"
    );
}
