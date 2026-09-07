// SPDX-License-Identifier: MIT
//! The recording lifecycle as seen across the IPC boundary.
//!
//! These replace the old `e2e_tests.rs` / `lifecycle_validation.rs`, which asserted
//! nothing: their `verify_*` calls had empty bodies, and the server they questioned
//! answered `recording: false, // mock` regardless of what had happened.

mod common;

use std::time::Duration;

use bsr_ipc::{IpcCommand, IpcResponse, TelemetryEvent};
use common::TestHarness;

const SOON: Duration = Duration::from_secs(5);

#[tokio::test]
async fn recording_state_is_observable_over_ipc() {
    let h = TestHarness::new().await;

    assert!(!h.status().await.0, "a fresh server is not recording");
    h.send(IpcCommand::StartRecording).await;
    assert!(h.status().await.0, "after StartRecording the server must say so");
    h.send(IpcCommand::StopRecording).await;
    assert!(!h.status().await.0, "after StopRecording the server must say so");

    h.shutdown().await;
}

#[tokio::test]
async fn lifecycle_transitions_are_broadcast_in_order() {
    let mut h = TestHarness::new().await;

    h.send(IpcCommand::StartRecording).await;
    h.expect_event(TelemetryEvent::RecordingStarted, SOON).await;
    h.send(IpcCommand::StopRecording).await;
    h.expect_event(TelemetryEvent::RecordingStopped, SOON).await;

    h.shutdown().await;
}

/// Uptime was hardcoded `0`. A monotonic clock that never advances is indistinguishable
/// from a hung server, which is precisely what a health probe exists to detect.
#[tokio::test]
async fn uptime_advances() {
    let h = TestHarness::new().await;
    let (_, first) = h.status().await;
    tokio::time::sleep(Duration::from_millis(30)).await;
    let (_, second) = h.status().await;
    assert!(second > first, "uptime went {first} -> {second}; it must advance");
    h.shutdown().await;
}

#[tokio::test]
async fn health_agrees_with_recording_state() {
    let h = TestHarness::new().await;
    h.send(IpcCommand::StartRecording).await;

    match h.send(IpcCommand::GetHealthStatus).await {
        IpcResponse::HealthStatus { recording_active, uptime_secs, frames_captured, .. } => {
            assert!(recording_active, "health must not disagree with GetStatus");
            assert!(uptime_secs > 0.0, "uptime must be real");
            assert!(frames_captured.is_none(), "the server cannot see the capture pipeline");
        }
        other => panic!("GetHealthStatus returned {other:?}"),
    }

    h.shutdown().await;
}

/// The muxer configuration crosses the IPC boundary as serialized data, so the wire
/// format is part of the contract — a field added without a default silently breaks
/// every older client.
#[tokio::test]
async fn muxer_config_survives_the_wire() {
    let h = TestHarness::new().await;
    let cfg = bsr_ipc::MuxerConfig {
        base_output_path: h.output_path("clips"),
        file_naming_strategy: bsr_ipc::FileNamingStrategy::Simple("take.mp4".into()),
        max_duration: Duration::from_secs(600),
        fps: 30,
        width: 1280,
        height: 720,
    };
    let cmd = IpcCommand::ConfigureMuxer(cfg.clone());
    let round_tripped: IpcCommand =
        serde_json::from_str(&serde_json::to_string(&cmd).expect("serialize")).expect("deserialize");
    assert_eq!(round_tripped, cmd, "MuxerConfig must survive serialization unchanged");

    assert_eq!(h.send(cmd).await, IpcResponse::Ok, "the server must accept it");
    h.shutdown().await;
}
