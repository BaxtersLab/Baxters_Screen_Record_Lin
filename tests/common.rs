// SPDX-License-Identifier: MIT
//! Shared harness for the workspace's cross-crate integration tests.
//!
//! **Every method here does something real.** The previous harness exposed 14 methods
//! with empty bodies (`verify_muxer_finalization`, `verify_no_orphaned_tasks`,
//! `verify_timebase_alignment`, …) plus a `get_current_duration` that returned the
//! literal `1` under a `// Mock implementation` comment. The 47 tests built on it could
//! not fail, and were never compiled anyway — the repository root is a virtual workspace,
//! so nothing owned them. If a capability cannot be asserted for real, it does not get a
//! method here; the test is dropped instead of being made to look green.

use std::time::Duration;

use bsr_ipc::{IpcClient, IpcCommand, IpcResponse, IpcServer, TelemetryEvent};
use tokio::sync::broadcast;

pub struct TestHarness {
    client: IpcClient,
    telemetry: broadcast::Receiver<TelemetryEvent>,
    server: tokio::task::JoinHandle<()>,
    /// Disposable output directory; dropped (and deleted) with the harness.
    pub out_dir: tempfile::TempDir,
}

impl TestHarness {
    pub async fn new() -> Self {
        let (server, client, telemetry) = IpcServer::new();
        let rx = telemetry.subscribe();
        let handle = tokio::spawn(async move { server.run().await });
        Self {
            client,
            telemetry: rx,
            server: handle,
            out_dir: tempfile::tempdir().expect("temp output dir"),
        }
    }

    pub async fn send(&self, cmd: IpcCommand) -> IpcResponse {
        self.client.send_command(cmd).await.expect("IPC command failed")
    }

    /// `(recording, uptime_ms)` as the server actually reports them.
    pub async fn status(&self) -> (bool, u64) {
        match self.send(IpcCommand::GetStatus).await {
            IpcResponse::Status { recording, uptime_ms } => (recording, uptime_ms),
            other => panic!("GetStatus returned {other:?}"),
        }
    }

    /// Wait for a specific telemetry event, failing the test if it does not arrive.
    /// A bounded wait, so a missing event is a failure rather than a hung suite.
    pub async fn expect_event(&mut self, want: TelemetryEvent, within: Duration) {
        let deadline = tokio::time::Instant::now() + within;
        loop {
            let left = deadline.saturating_duration_since(tokio::time::Instant::now());
            if left.is_zero() {
                panic!("timed out waiting {within:?} for {want:?}");
            }
            match tokio::time::timeout(left, self.telemetry.recv()).await {
                Ok(Ok(ev)) if ev == want => return,
                Ok(Ok(_)) => continue,
                Ok(Err(e)) => panic!("telemetry channel error waiting for {want:?}: {e}"),
                Err(_) => panic!("timed out waiting {within:?} for {want:?}"),
            }
        }
    }

    pub fn output_path(&self, name: &str) -> std::path::PathBuf {
        self.out_dir.path().join(name)
    }

    pub async fn shutdown(self) {
        let _ = self.client.send_command(IpcCommand::Shutdown).await;
        let _ = tokio::time::timeout(Duration::from_secs(5), self.server).await;
    }
}

/// Serialises tests that touch `bsr_ipc`'s process-global record-space slot.
///
/// That slot is deliberately global — it is the IPC-thread-to-UI-thread handoff — so
/// two tests using it in parallel genuinely contend, and one steals the other's
/// delivery. Locking is the correct fix rather than a workaround; the alternative would
/// be weakening the assertions until the race stopped showing.
pub fn record_space_lock() -> std::sync::MutexGuard<'static, ()> {
    static LOCK: std::sync::OnceLock<std::sync::Mutex<()>> = std::sync::OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}
