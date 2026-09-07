// SPDX-License-Identifier: MIT
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::{info, warn, error};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FrameInfo {
    pub sequence: u64,
    pub width: u32,
    pub height: u32,
    pub size_bytes: usize,
    pub timestamp: u64,
}

/// Telemetry client: connects to a named pipe and writes newline-delimited
/// JSON telemetry envelopes for each `FrameInfo` received on `rx`.
///
/// Behaviour:
/// - On Windows: attempt to connect to `pipe_path` and stream JSON lines.
///   Reconnects on failure.
/// - On non-Windows: writes JSON lines to a log file at env `BSR_TELEMETRY_LOG`
///   (useful for tests).
/// Where the non-Windows telemetry log goes.
///
/// `BSR_TELEMETRY_LOG` overrides it. The default was the bare relative name
/// `bsr_telemetry.log`, which resolves against the current directory — `/` for an app
/// started from the desktop — so it failed with "Permission denied" on every launch
/// that was not from a writable shell. Same shape as the config path defect: a
/// relative default is a different file depending on who started the process.
#[cfg(not(target_os = "windows"))]
fn default_telemetry_log_path() -> std::path::PathBuf {
    if let Ok(explicit) = std::env::var("BSR_TELEMETRY_LOG") {
        if !explicit.trim().is_empty() {
            return std::path::PathBuf::from(explicit);
        }
    }
    let base = std::env::var_os("XDG_STATE_HOME")
        .map(std::path::PathBuf::from)
        .filter(|p| p.is_absolute())
        .or_else(|| {
            std::env::var_os("HOME")
                .map(std::path::PathBuf::from)
                .map(|h| h.join(".local").join("state"))
        })
        .unwrap_or_else(std::env::temp_dir);
    base.join("bsr").join("telemetry.log")
}

pub async fn telemetry_client(mut rx: mpsc::Receiver<FrameInfo>, pipe_path: String) {
    #[cfg(target_os = "windows")]
    {
        use std::time::Duration;
        use tokio::io::{AsyncWriteExt, BufReader};
        use tokio::io::AsyncBufReadExt;
        use tokio::net::windows::named_pipe::ClientOptions;

        loop {
                    match ClientOptions::new().open(&pipe_path) {
                        Ok(pipe) => {
                            info!(pipe = %pipe_path, "Connected to telemetry pipe");
                            let (r, mut w) = tokio::io::split(pipe);

                    // Track number of frames written so we can report it in the final TelemetryDone
                    let mut frames_written: u64 = 0;

                    while let Some(fi) = rx.recv().await {
                        // Build telemetry envelope
                        let env = serde_json::json!(
                            {
                                "timestamp": chrono::Utc::now().to_rfc3339(),
                                "sequence": fi.sequence,
                                "event": {
                                    "event": "FrameCaptured",
                                    "data": {
                                        "sequence": fi.sequence,
                                        "width": fi.width,
                                        "height": fi.height,
                                        "size_bytes": fi.size_bytes,
                                    }
                                }
                            }
                        );
                        let s = match serde_json::to_string(&env) {
                            Ok(s) => s,
                            Err(e) => { error!(%e, "serialize telemetry envelope failed"); continue; }
                        };

                        // Log attempt to write this frame so we can detect early closes
                        let now_ts = chrono::Utc::now().to_rfc3339();
                        info!(ts = %now_ts, sequence = fi.sequence, size = fi.size_bytes, "telemetry_client: writing frame to pipe");

                        if let Err(e) = w.write_all(s.as_bytes()).await {
                            let now_ts = chrono::Utc::now().to_rfc3339();
                            warn!(ts = %now_ts, %e, sequence = fi.sequence, "write to telemetry pipe failed, will reconnect");
                            break; // reconnect outer loop
                        }
                        if let Err(e) = w.write_all(b"\n").await {
                            let now_ts = chrono::Utc::now().to_rfc3339();
                            warn!(ts = %now_ts, %e, sequence = fi.sequence, "write newline failed, will reconnect");
                            break;
                        }
                        if let Err(e) = w.flush().await {
                            let now_ts = chrono::Utc::now().to_rfc3339();
                            warn!(ts = %now_ts, %e, sequence = fi.sequence, "flush failed, will reconnect");
                            break;
                        }

                        frames_written = frames_written.wrapping_add(1);
                        info!(sequence = fi.sequence, written = frames_written, "telemetry_client: frame successfully flushed to pipe");
                    }

                    // If rx closed, send a TelemetryDone envelope and wait
                    // for an explicit drain acknowledgment from the shim
                    if rx.is_closed() {
                        let now_ts = chrono::Utc::now().to_rfc3339();
                        info!(ts = %now_ts, "telemetry rx closed, sending TelemetryDone, waiting for ack before closing pipe");

                        let done_env = serde_json::json!({
                            "timestamp": chrono::Utc::now().to_rfc3339(),
                            "event": {
                                "event": "TelemetryDone",
                                "data": {
                                    "frames_sent": frames_written
                                }
                            }
                        });

                        let s = match serde_json::to_string(&done_env) {
                            Ok(s) => s,
                            Err(e) => { error!(%e, "serialize TelemetryDone failed"); return; }
                        };

                            if let Err(e) = w.write_all(s.as_bytes()).await {
                                let now_ts = chrono::Utc::now().to_rfc3339();
                                warn!(ts = %now_ts, %e, "write TelemetryDone failed, closing");
                                return;
                            }
                            if let Err(e) = w.write_all(b"\n").await {
                                let now_ts = chrono::Utc::now().to_rfc3339();
                                warn!(ts = %now_ts, %e, "write TelemetryDone newline failed, closing");
                                return;
                            }
                            if let Err(e) = w.flush().await {
                                let now_ts = chrono::Utc::now().to_rfc3339();
                                warn!(ts = %now_ts, %e, "flush after TelemetryDone failed, closing");
                                return;
                            }

                            let now_ts = chrono::Utc::now().to_rfc3339();
                            info!(ts = %now_ts, frames = frames_written, "telemetry_client: TelemetryDone written and flushed to pipe");
                            info!(ts = %now_ts, "telemetry_client: waiting up to 5s for drain_complete ack from shim");

                            // Wait for ack line from the shim with a timeout
                            let mut reader = BufReader::new(r);
                            let mut ack_line = String::new();
                            match tokio::time::timeout(Duration::from_secs(5), reader.read_line(&mut ack_line)).await {
                                Ok(Ok(0)) => {
                                    let now_ts = chrono::Utc::now().to_rfc3339();
                                    warn!(ts = %now_ts, "telemetry_client: ack read returned 0 bytes (server closed connection)");
                                }
                                Ok(Ok(_n)) => {
                                    let now_ts = chrono::Utc::now().to_rfc3339();
                                    let trimmed = ack_line.trim();
                                    match serde_json::from_str::<serde_json::Value>(trimmed) {
                                        Ok(v) => {
                                            if v.get("ack").and_then(|a| a.as_str()) == Some("drain_complete") {
                                                info!(ts = %now_ts, frames = frames_written, "telemetry_client: received drain_complete ack");
                                            } else {
                                                warn!(ts = %now_ts, received = %trimmed, "telemetry_client: unexpected ack payload");
                                            }
                                        }
                                        Err(e) => {
                                            warn!(ts = %now_ts, %e, payload = %trimmed, "telemetry_client: failed to parse ack");
                                        }
                                    }
                                }
                                Ok(Err(e)) => {
                                    warn!(%e, "telemetry_client: error while reading ack");
                                }
                                Err(_) => {
                                    let now_ts = chrono::Utc::now().to_rfc3339();
                                    warn!(ts = %now_ts, "telemetry_client: timeout waiting for drain ack");
                                }
                            }

                            let now_ts = chrono::Utc::now().to_rfc3339();
                            info!(ts = %now_ts, "telemetry_client: attempting graceful shutdown of write half");
                            if let Err(e) = w.shutdown().await {
                                warn!(ts = %now_ts, %e, "telemetry_client: write-half shutdown failed");
                            }

                            info!(ts = %now_ts, "telemetry rx closed, exiting telemetry client");
                            return;
                    }
                }
                Err(e) => {
                    warn!(%e, pipe = %pipe_path, "failed to open telemetry pipe, retrying in 1s");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                    continue;
                }
            }
        }
    }

    #[cfg(not(target_os = "windows"))]
    {
        use std::fs::OpenOptions;
        use std::io::Write;

        let log_path = default_telemetry_log_path();
        if let Some(dir) = log_path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let mut file = match OpenOptions::new().create(true).append(true).open(&log_path) {
            Ok(f) => f,
            Err(e) => {
                // Best-effort diagnostics, never a failure of the application. This
                // was `error!` on a path relative to the current directory, so a
                // launcher-started app (cwd `/`) logged an ERROR about Permission
                // denied on every recording — a red herring sitting next to a real
                // capture failure while it was being diagnosed.
                warn!(
                    %e,
                    path = %log_path.display(),
                    "telemetry log unavailable; continuing without it"
                );
                return;
            }
        };

        while let Some(fi) = rx.recv().await {
            let env = serde_json::json!(
                {
                    "timestamp": chrono::Utc::now().to_rfc3339(),
                    "sequence": fi.sequence,
                    "event": {
                        "event": "FrameCaptured",
                        "data": {
                            "sequence": fi.sequence,
                            "width": fi.width,
                            "height": fi.height,
                            "size_bytes": fi.size_bytes,
                        }
                    }
                }
            );
            let s = match serde_json::to_string(&env) {
                Ok(s) => s,
                Err(e) => { error!(%e, "serialize telemetry envelope failed"); continue; }
            };

            if let Err(e) = writeln!(file, "{}", s) {
                warn!(%e, "failed to write telemetry line to log");
            }
        }
    }
}

#[cfg(all(test, not(target_os = "windows")))]
mod tests {

    /// The default was the bare relative name `bsr_telemetry.log`, so it resolved
    /// against the current directory. An app launched from the desktop has cwd `/`,
    /// which is not writable, so telemetry aborted with "Permission denied" on every
    /// single launch that was not from a shell.
    #[test]
    fn the_default_telemetry_log_path_is_absolute() {
        let saved = std::env::var("BSR_TELEMETRY_LOG").ok();
        std::env::remove_var("BSR_TELEMETRY_LOG");

        let p = super::default_telemetry_log_path();
        assert!(
            p.is_absolute(),
            "default telemetry log path {p:?} is relative, so it lands wherever the \
             process happened to be started from"
        );
        assert!(
            p.parent().is_some(),
            "path {p:?} has no parent directory to create"
        );

        if let Some(v) = saved {
            std::env::set_var("BSR_TELEMETRY_LOG", v);
        }
    }

    /// An explicit override must still win, and be taken verbatim.
    #[test]
    fn an_explicit_telemetry_log_path_wins() {
        let saved = std::env::var("BSR_TELEMETRY_LOG").ok();
        let dir = tempfile::tempdir().unwrap();
        let want = dir.path().join("chosen.log");
        std::env::set_var("BSR_TELEMETRY_LOG", &want);

        assert_eq!(super::default_telemetry_log_path(), want);

        std::env::remove_var("BSR_TELEMETRY_LOG");
        if let Some(v) = saved {
            std::env::set_var("BSR_TELEMETRY_LOG", v);
        }
    }

    use super::*;
    use tempfile::NamedTempFile;

    #[tokio::test]
    async fn telemetry_client_writes_to_log() {
        // Non-Windows path uses file; set env to tmp file
        let tmp = NamedTempFile::new().expect("tmp file");
        let path = tmp.path().to_str().unwrap().to_string();
        std::env::set_var("BSR_TELEMETRY_LOG", &path);

        let (tx, rx) = mpsc::channel(8);
        let handle = tokio::spawn(async move {
            telemetry_client(rx, "unused".to_string()).await;
        });

        let fi = FrameInfo { sequence: 1, width: 1280, height: 720, size_bytes: 1024, timestamp: 123 };
        tx.send(fi).await.unwrap();
        // Close sender so client exits
        drop(tx);

        handle.await.unwrap();

        let contents = std::fs::read_to_string(&path).unwrap();
        assert!(contents.contains("FrameCaptured"));
    }
}
