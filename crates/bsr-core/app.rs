// SPDX-License-Identifier: MIT
// Baxter's Screen Record — AppState, lifecycle
// Block A-3: app.rs

use std::sync::{Arc, Mutex};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Instant;
use tokio::sync::watch;
use tracing::info;
use crate::config::BsrConfig;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppMode {
    Visible,
    Stealth,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RecordingState {
    Idle,
    Recording,
    Paused,
}

pub struct AppState {
    config: Arc<Mutex<BsrConfig>>,
    app_mode_tx: watch::Sender<AppMode>,
    app_mode_rx: watch::Receiver<AppMode>,
    recording_state_tx: watch::Sender<RecordingState>,
    recording_state_rx: watch::Receiver<RecordingState>,
    start_time: Instant,
    // Seed-BSR-G2-02-11: graceful degradation flag — false when FFmpeg is absent.
    encoding_available: AtomicBool,
    // Seed-BSR-G3-01-11: frame counters for health-check responses.
    frames_captured: AtomicU64,
    frames_dropped: AtomicU64,
    // ...existing code...
}

impl AppState {
    pub fn new(config: BsrConfig) -> Arc<Self> {
        let (app_mode_tx, app_mode_rx) = watch::channel(AppMode::Visible);
        let (recording_state_tx, recording_state_rx) = watch::channel(RecordingState::Idle);
        Arc::new(Self {
            config: Arc::new(Mutex::new(config)),
            app_mode_tx,
            app_mode_rx,
            recording_state_tx,
            recording_state_rx,
            start_time: Instant::now(),
            encoding_available: AtomicBool::new(false),
            frames_captured: AtomicU64::new(0),
            frames_dropped: AtomicU64::new(0),
        })
    }

    pub fn config(&self) -> BsrConfig {
        self.config.lock().unwrap().clone()
    }

    pub fn set_config(&self, config: BsrConfig) {
        *self.config.lock().unwrap() = config;
    }

    pub fn app_mode_rx(&self) -> &watch::Receiver<AppMode> {
        &self.app_mode_rx
    }

    pub fn recording_state_rx(&self) -> &watch::Receiver<RecordingState> {
        &self.recording_state_rx
    }

    pub fn set_app_mode(&self, mode: AppMode) {
        let _ = self.app_mode_tx.send(mode);
    }

    pub fn set_recording_state(&self, state: RecordingState) {
        let _ = self.recording_state_tx.send(state);
    }

    pub fn uptime_ms(&self) -> u64 {
        self.start_time.elapsed().as_millis() as u64
    }

    // Seed-BSR-G2-02-11 — encoding_available accessors
    pub fn set_encoding_available(&self, available: bool) {
        self.encoding_available.store(available, Ordering::Relaxed);
        info!("encoding_available set to {}", available);
    }

    pub fn encoding_available(&self) -> bool {
        self.encoding_available.load(Ordering::Relaxed)
    }

    // Seed-BSR-G3-01-11 — frame counter accessors
    pub fn increment_frames_captured(&self) {
        self.frames_captured.fetch_add(1, Ordering::Relaxed);
    }

    pub fn increment_frames_dropped(&self) {
        self.frames_dropped.fetch_add(1, Ordering::Relaxed);
    }

    pub fn frames_captured(&self) -> u64 {
        self.frames_captured.load(Ordering::Relaxed)
    }

    pub fn frames_dropped(&self) -> u64 {
        self.frames_dropped.load(Ordering::Relaxed)
    }

    pub fn recording_active(&self) -> bool {
        *self.recording_state_rx.borrow() == RecordingState::Recording
    }

    // ...existing code...
}

/// Single-instance guard backed by a loopback TCP bind.
///
/// The first instance binds `DEFAULT_PORT` on localhost and holds the listener
/// for its whole lifetime; a second instance's `bind` fails with
/// "address in use", so it knows another instance is already running. This is
/// portable (no OS-specific named mutex) and releases automatically on exit.
pub struct SingleInstanceGuard {
    _listener: std::net::TcpListener,
}

impl SingleInstanceGuard {
    /// Loopback port used for the single-instance lock.
    pub const DEFAULT_PORT: u16 = 51839;

    /// Attempt to acquire the single-instance lock on [`Self::DEFAULT_PORT`].
    pub fn new() -> Result<Self, String> {
        Self::acquire(Self::DEFAULT_PORT)
    }

    /// Attempt to acquire the single-instance lock on `port`. Returns `Err` if
    /// another instance already holds it (the bind fails).
    pub fn acquire(port: u16) -> Result<Self, String> {
        use std::net::{Ipv4Addr, SocketAddrV4, TcpListener};
        let addr = SocketAddrV4::new(Ipv4Addr::LOCALHOST, port);
        match TcpListener::bind(addr) {
            Ok(listener) => Ok(Self { _listener: listener }),
            Err(e) => Err(format!(
                "another instance is already running (loopback port {port}): {e}"
            )),
        }
    }
}

// main() will live in a top-level binary crate once all modules are wired.
// Removed draft that referenced external crate APIs not available from bsr-core.

#[cfg(test)]
#[path = "app_test.rs"]
mod app_test;
