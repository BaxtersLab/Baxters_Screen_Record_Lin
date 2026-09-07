// SPDX-License-Identifier: MIT
// Baxter's Screen Record — IPC Layer
// Minimal async IPC with JSON framing and mock in-process channels

// Seed-BSR-G3-02-11 / G3-03-11: audit log for the redaction transform.
pub mod redact_audit;
pub mod telemetry_pipe;

use serde::{Deserialize, Serialize};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::time;
use tracing::info;

/// Commands sent to the recorder
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum IpcCommand {
    StartRecording,
    /// Request that the UI ask the local user for permission to start a recording.
    /// Remote controllers SHOULD use this variant so the local UI can prompt.
    RequestStartRecording { requester: Option<String> },
    /// Ask the UI whether it wants a local copy of the recording at `path`.
    OfferSaveCopy { path: String, requester: Option<String> },
    StopRecording,
    GetStatus,
    Shutdown,
    ConfigureMuxer(MuxerConfig),
    /// Seed-BSR-G3-01-11: liveness probe via the command pipe.
    GetHealthStatus,
    /// Set the record space -- the rectangle of the screen that gets recorded.
    ///
    /// The backend entry point for cropping. An agent driving BSR has no pointer and
    /// does not need one: it states the two corners in screen pixels and BSR records
    /// that rectangle. `None` restores the full screen, which is also the default.
    ///
    /// Corners may be given in either order, may sit outside the screen, and may be an
    /// odd number of pixels apart -- `CaptureRegion::resolve` normalises, clamps and
    /// rounds to even before anything acts on it. A rectangle that leaves no usable
    /// area records the full screen rather than nothing.
    ///
    /// Takes effect on the NEXT recording; it does not resize one already running,
    /// because the encoder is opened at a fixed size and cannot change mid-stream.
    SetRecordSpace { region: Option<bsr_core::config::CaptureRegion> },
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub enum FileNamingStrategy {
    Simple(String),
    TimestampedFile,
    TimestampedFolder,
}

#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct MuxerConfig {
    pub base_output_path: std::path::PathBuf,
    pub file_naming_strategy: FileNamingStrategy,
    pub max_duration: std::time::Duration,
    /// Frames per second of the encoded stream. Sets the MP4 stream time_base so
    /// playback timing is correct (encoder pts are in 1/fps units).
    #[serde(default = "default_muxer_fps")]
    pub fps: u32,
    /// Encoded frame dimensions, written to the container's codec parameters.
    #[serde(default = "default_muxer_width")]
    pub width: u32,
    #[serde(default = "default_muxer_height")]
    pub height: u32,
}

fn default_muxer_fps() -> u32 {
    30
}
fn default_muxer_width() -> u32 {
    1920
}
fn default_muxer_height() -> u32 {
    1080
}

impl MuxerConfig {
    pub fn preview_output_path(&self) -> std::path::PathBuf {
        use chrono::Utc;

        let now = Utc::now();
        let base = &self.base_output_path;

        match &self.file_naming_strategy {
            FileNamingStrategy::Simple(name) => base.join(name),
            FileNamingStrategy::TimestampedFile => {
                let filename = format!("{}.mp4", now.format("%Y-%m-%d_%H-%M-%S"));
                base.join(filename)
            }
            FileNamingStrategy::TimestampedFolder => {
                let folder = now.format("%Y-%m-%d").to_string();
                let filename = format!("{}.mp4", now.format("%H-%M-%S"));
                base.join(folder).join(filename)
            }
        }
    }
}

impl Default for MuxerConfig {
    fn default() -> Self {
        Self {
            base_output_path: std::path::PathBuf::from("./output"),
            file_naming_strategy: FileNamingStrategy::TimestampedFile,
            max_duration: std::time::Duration::from_secs(18000), // 5 hours
            fps: default_muxer_fps(),
            width: default_muxer_width(),
            height: default_muxer_height(),
        }
    }
}

/// Responses sent from the recorder
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum IpcResponse {
    Ok,
    Status { recording: bool, uptime_ms: u64 },
    Error(String),
    /// Seed-BSR-G3-01-11: response to GetHealthStatus.
    HealthStatus {
        status: String,
        uptime_secs: f64,
        encoding_available: bool,
        recording_active: bool,
        /// `None` when the server cannot observe the capture pipeline, which is
        /// currently always: `TelemetryEvent` carries no frame counters. These were
        /// hardcoded `0`, which reads as "zero frames captured" rather than
        /// "not measured" -- a confident wrong answer instead of an honest absent one.
        frames_captured: Option<u64>,
        frames_dropped: Option<u64>,
    },
}

/// Async broadcast telemetry events
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum TelemetryEvent {
    AppStarted,
    AppStopped,
    RecordingStarted,
    RecordingStopped,
    ErrorOccurred { message: String },
}

/// Structured IPC error type
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IpcError {
    pub message: String,
}

impl std::fmt::Display for IpcError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "IPC Error: {}", self.message)
    }
}

impl std::error::Error for IpcError {}

/// Telemetry broadcaster
pub struct IpcTelemetry {
    tx: broadcast::Sender<TelemetryEvent>,
}

impl IpcTelemetry {
    pub fn new() -> Self {
        let (tx, _) = broadcast::channel(32);
        Self { tx }
    }

    pub fn subscribe(&self) -> broadcast::Receiver<TelemetryEvent> {
        self.tx.subscribe()
    }

    pub fn sender(&self) -> broadcast::Sender<TelemetryEvent> {
        self.tx.clone()
    }

    pub fn send(&self, event: TelemetryEvent) {
        let _ = self.tx.send(event);
    }
}

/// Async IPC server (mock in-process)
pub struct IpcServerState {
    pub muxer_config: Option<MuxerConfig>,
    /// Whether a recording is running. `GetStatus` reported a hardcoded `false // mock`,
    /// so every status query was a lie and nothing downstream could be tested.
    pub recording: bool,
    /// When the server started, for a real uptime instead of a hardcoded `0`.
    pub started_at: std::time::Instant,
}

impl Default for IpcServerState {
    fn default() -> Self {
        Self { muxer_config: None, recording: false, started_at: std::time::Instant::now() }
    }
}

pub struct IpcServer {
    cmd_rx: mpsc::Receiver<(IpcCommand, oneshot::Sender<IpcResponse>)>,
    _telemetry: IpcTelemetry,
    state: IpcServerState,
    consent_tx: Option<mpsc::Sender<ConsentRequest>>,
}

/// Consent request forwarded to the UI for user approval.
pub struct ConsentRequest {
    pub requester: Option<String>,
    pub resp: oneshot::Sender<bool>,
}

use once_cell::sync::Lazy;
use std::sync::Mutex;

static UI_CONSENT_QUEUE: Lazy<Mutex<Vec<ConsentRequest>>> = Lazy::new(|| Mutex::new(Vec::new()));

/// Push a consent request into the UI-visible global queue.
pub fn push_consent_request_to_ui(req: ConsentRequest) {
    let mut q = UI_CONSENT_QUEUE.lock().unwrap();
    q.push(req);
}

/// Try to take the oldest consent request from the UI queue.
pub fn try_take_consent_request() -> Option<ConsentRequest> {
    let mut q = UI_CONSENT_QUEUE.lock().unwrap();
    if q.is_empty() {
        None
    } else {
        Some(q.remove(0))
    }
}

/// Save offer forwarded to the UI when another participant offers a copy.
pub struct SaveOfferRequest {
    pub path: String,
    pub resp: oneshot::Sender<Option<String>>, // None == declined, Some(path) == where to save
}

static UI_SAVE_OFFER_QUEUE: Lazy<Mutex<Vec<SaveOfferRequest>>> = Lazy::new(|| Mutex::new(Vec::new()));

/// Latest record space requested over IPC, waiting for the UI thread to pick it up.
///
/// Deliberately a single slot rather than a queue: this is a setting, not an event, and
/// an agent that sends three regions in a row means the third one. The outer Option is
/// "is there a pending change", the inner is "crop, or full screen".
#[allow(clippy::option_option)]
static UI_RECORD_SPACE: Lazy<Mutex<Option<Option<bsr_core::config::CaptureRegion>>>> =
    Lazy::new(|| Mutex::new(None));

/// Queue a record-space change for the UI. Called from the IPC server thread.
pub fn push_record_space_to_ui(region: Option<bsr_core::config::CaptureRegion>) {
    if let Ok(mut slot) = UI_RECORD_SPACE.lock() {
        *slot = Some(region);
    }
}

/// Take a pending record-space change, if any. Called from the UI thread each frame.
#[allow(clippy::option_option)]
pub fn try_take_record_space() -> Option<Option<bsr_core::config::CaptureRegion>> {
    UI_RECORD_SPACE.lock().ok().and_then(|mut slot| slot.take())
}

/// Push a save offer into the UI-visible global queue.
pub fn push_save_offer_to_ui(req: SaveOfferRequest) {
    let mut q = UI_SAVE_OFFER_QUEUE.lock().unwrap();
    q.push(req);
}

/// Try to take the oldest save offer from the UI queue.
pub fn try_take_save_offer_request() -> Option<SaveOfferRequest> {
    let mut q = UI_SAVE_OFFER_QUEUE.lock().unwrap();
    if q.is_empty() {
        None
    } else {
        Some(q.remove(0))
    }
}

impl IpcServer {
    pub fn new() -> (Self, IpcClient, broadcast::Sender<TelemetryEvent>) {
        let (cmd_tx, cmd_rx) = mpsc::channel(32);
        let telemetry = IpcTelemetry::new();
        let telemetry_tx = telemetry.sender();
        let state = IpcServerState::default();
        let server = Self { cmd_rx, _telemetry: telemetry, state, consent_tx: None };
        let client = IpcClient { cmd_tx };
        (server, client, telemetry_tx)
    }

    /// Create a new server that exposes a consent receiver for UI handling.
    /// Returns (server, client, telemetry_tx, consent_rx).
    pub fn new_with_consent() -> (Self, IpcClient, broadcast::Sender<TelemetryEvent>, mpsc::Receiver<ConsentRequest>) {
        let (cmd_tx, cmd_rx) = mpsc::channel(32);
        let telemetry = IpcTelemetry::new();
        let telemetry_tx = telemetry.sender();
        let state = IpcServerState::default();
        let (consent_tx, consent_rx) = mpsc::channel(8);
        let server = Self { cmd_rx, _telemetry: telemetry, state, consent_tx: Some(consent_tx) };
        let client = IpcClient { cmd_tx };
        (server, client, telemetry_tx, consent_rx)
    }

    pub async fn run(mut self) {
        info!("IPC server starting");
        while let Some((cmd, resp_tx)) = self.cmd_rx.recv().await {
            info!("Received command: {:?}", cmd);
            let response = match cmd {
                IpcCommand::StartRecording => {
                    self.state.recording = true;
                    // The server never emitted these, so anything waiting on
                    // RecordingStopped waited forever.
                    self._telemetry.send(TelemetryEvent::RecordingStarted);
                    IpcResponse::Ok
                }
                IpcCommand::RequestStartRecording { requester } => {
                    if let Some(consent_tx) = &self.consent_tx {
                        // Create oneshot for UI response
                        let (resp_s, resp_r) = oneshot::channel();
                        let req = ConsentRequest { requester: requester.clone(), resp: resp_s };
                        // Send consent request to UI (best-effort)
                        if consent_tx.send(req).await.is_err() {
                            IpcResponse::Error("consent_channel_closed".to_string())
                        } else {
                            // Wait up to 5s for user response
                            match time::timeout(std::time::Duration::from_secs(5), resp_r).await {
                                Ok(Ok(accepted)) => {
                                    if accepted { IpcResponse::Ok } else { IpcResponse::Error("declined".to_string()) }
                                }
                                _ => IpcResponse::Error("consent_timeout".to_string()),
                            }
                        }
                    } else {
                        // No UI to ask; accept by default
                        IpcResponse::Ok
                    }
                }
                IpcCommand::OfferSaveCopy { path, requester } => {
                    tracing::debug!(?requester, ?path, "OfferSaveCopy received");
                    // Forward save offer to UI global queue (best-effort) and wait for reply
                    let (resp_s, resp_r) = oneshot::channel();
                    let req = SaveOfferRequest { path: path.clone(), resp: resp_s };
                    push_save_offer_to_ui(req);
                    match time::timeout(std::time::Duration::from_secs(30), resp_r).await {
                        Ok(Ok(Some(_chosen))) => IpcResponse::Ok,
                        Ok(Ok(None)) => IpcResponse::Error("declined".to_string()),
                        _ => IpcResponse::Error("save_offer_timeout".to_string()),
                    }
                }
                IpcCommand::StopRecording => {
                    self.state.recording = false;
                    self._telemetry.send(TelemetryEvent::RecordingStopped);
                    IpcResponse::Ok
                }
                IpcCommand::GetStatus => IpcResponse::Status {
                    recording: self.state.recording,
                    uptime_ms: self.state.started_at.elapsed().as_millis() as u64,
                },
                IpcCommand::ConfigureMuxer(cfg) => {
                    self.state.muxer_config = Some(cfg);
                    IpcResponse::Ok
                }
                IpcCommand::SetRecordSpace { region } => {
                    tracing::info!(?region, "record space set over IPC");
                    push_record_space_to_ui(region);
                    IpcResponse::Ok
                }
                IpcCommand::GetHealthStatus => {
                    IpcResponse::HealthStatus {
                        status: "ok".to_string(),
                        uptime_secs: self.state.started_at.elapsed().as_secs_f64(),
                        encoding_available: true,
                        recording_active: self.state.recording,
                        frames_captured: None,
                        frames_dropped: None,
                    }
                }
                IpcCommand::Shutdown => {
                    info!("Shutdown command received");
                    break;
                }
            };
            let _ = resp_tx.send(response);
        }
        info!("IPC server stopped");
    }
}

/// Async IPC client (mock in-process)
pub struct IpcClient {
    cmd_tx: mpsc::Sender<(IpcCommand, oneshot::Sender<IpcResponse>)>,
}

impl Clone for IpcClient {
    fn clone(&self) -> Self {
        Self { cmd_tx: self.cmd_tx.clone() }
    }
}

impl IpcClient {
    pub fn new(cmd_tx: mpsc::Sender<(IpcCommand, oneshot::Sender<IpcResponse>)>) -> Self {
        Self { cmd_tx }
    }

    pub async fn send_command(&self, cmd: IpcCommand) -> Result<IpcResponse, IpcError> {
        let (resp_tx, resp_rx) = oneshot::channel();
        self.cmd_tx
            .send((cmd, resp_tx))
            .await
            .map_err(|_| IpcError {
                message: "Failed to send command".to_string(),
            })?;
        resp_rx
            .await
            .map_err(|_| IpcError {
                message: "Failed to receive response".to_string(),
            })
    }
}

#[cfg(test)]
mod server_state_tests {
    use super::*;

    async fn server() -> (IpcClient, broadcast::Receiver<TelemetryEvent>, tokio::task::JoinHandle<()>) {
        let (srv, client, telemetry) = IpcServer::new();
        let rx = telemetry.subscribe();
        let handle = tokio::spawn(async move { srv.run().await });
        (client, rx, handle)
    }

    /// `GetStatus` must report whether a recording is actually running. It was hardcoded
    /// `recording: false, // mock`, which made every status query a lie and left the
    /// integration suite with nothing real to assert against.
    #[tokio::test]
    async fn status_reflects_start_and_stop() {
        let (client, _rx, handle) = server().await;

        match client.send_command(IpcCommand::GetStatus).await.unwrap() {
            IpcResponse::Status { recording, .. } => assert!(!recording, "idle at startup"),
            other => panic!("unexpected {other:?}"),
        }

        client.send_command(IpcCommand::StartRecording).await.unwrap();
        match client.send_command(IpcCommand::GetStatus).await.unwrap() {
            IpcResponse::Status { recording, .. } => {
                assert!(recording, "GetStatus must report a started recording")
            }
            other => panic!("unexpected {other:?}"),
        }

        client.send_command(IpcCommand::StopRecording).await.unwrap();
        match client.send_command(IpcCommand::GetStatus).await.unwrap() {
            IpcResponse::Status { recording, .. } => {
                assert!(!recording, "GetStatus must report a stopped recording")
            }
            other => panic!("unexpected {other:?}"),
        }

        let _ = client.send_command(IpcCommand::Shutdown).await;
        let _ = handle.await;
    }

    /// Starting and stopping must be observable on the telemetry bus. The server never
    /// emitted these, so anything waiting on `RecordingStopped` waited forever.
    #[tokio::test]
    async fn start_and_stop_are_broadcast() {
        let (client, mut rx, handle) = server().await;

        client.send_command(IpcCommand::StartRecording).await.unwrap();
        assert_eq!(rx.recv().await.unwrap(), TelemetryEvent::RecordingStarted);

        client.send_command(IpcCommand::StopRecording).await.unwrap();
        assert_eq!(rx.recv().await.unwrap(), TelemetryEvent::RecordingStopped);

        let _ = client.send_command(IpcCommand::Shutdown).await;
        let _ = handle.await;
    }

    /// Health must not invent numbers it cannot observe. The server has no view of the
    /// capture pipeline, so frame counts are `None`, not a confident zero.
    #[tokio::test]
    async fn health_reports_only_what_the_server_can_know() {
        let (client, _rx, handle) = server().await;
        client.send_command(IpcCommand::StartRecording).await.unwrap();

        match client.send_command(IpcCommand::GetHealthStatus).await.unwrap() {
            IpcResponse::HealthStatus { recording_active, frames_captured, frames_dropped, .. } => {
                assert!(recording_active, "health must agree with the recording state");
                assert!(frames_captured.is_none(), "server cannot observe frame counts");
                assert!(frames_dropped.is_none(), "server cannot observe drop counts");
            }
            other => panic!("unexpected {other:?}"),
        }

        let _ = client.send_command(IpcCommand::Shutdown).await;
        let _ = handle.await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_ipc_round_trip() {
        let (server, client, _) = IpcServer::new();

        // Spawn server
        tokio::spawn(async move {
            server.run().await;
        });

        // Test commands
        let resp = client.send_command(IpcCommand::GetStatus).await.unwrap();
        match resp {
            IpcResponse::Status { .. } => {}
            _ => panic!("Expected Status response"),
        }

        let resp = client.send_command(IpcCommand::StartRecording).await.unwrap();
        assert!(matches!(resp, IpcResponse::Ok));

        let resp = client.send_command(IpcCommand::StopRecording).await.unwrap();
        assert!(matches!(resp, IpcResponse::Ok));

        // Shutdown
        let _ = client.send_command(IpcCommand::Shutdown).await;
    }

    #[test]
    fn test_serialization_round_trip() {
        // Test IpcCommand
        let cmds = vec![
            IpcCommand::StartRecording,
            IpcCommand::StopRecording,
            IpcCommand::GetStatus,
            IpcCommand::Shutdown,
        ];
        for cmd in cmds {
            let json = serde_json::to_string(&cmd).unwrap();
            let deserialized: IpcCommand = serde_json::from_str(&json).unwrap();
            assert_eq!(cmd, deserialized);
        }

        // Test IpcResponse
        let resps = vec![
            IpcResponse::Ok,
            IpcResponse::Status {
                recording: true,
                uptime_ms: 12345,
            },
            IpcResponse::Error("test error".to_string()),
        ];
        for resp in resps {
            let json = serde_json::to_string(&resp).unwrap();
            let deserialized: IpcResponse = serde_json::from_str(&json).unwrap();
            assert_eq!(resp, deserialized);
        }

        // Test TelemetryEvent
        let events = vec![
            TelemetryEvent::AppStarted,
            TelemetryEvent::AppStopped,
            TelemetryEvent::RecordingStarted,
            TelemetryEvent::RecordingStopped,
            TelemetryEvent::ErrorOccurred {
                message: "test".to_string(),
            },
        ];
        for event in events {
            let json = serde_json::to_string(&event).unwrap();
            let deserialized: TelemetryEvent = serde_json::from_str(&json).unwrap();
            assert_eq!(event, deserialized);
        }

        // Test IpcError
        let error = IpcError {
            message: "test".to_string(),
        };
        let json = serde_json::to_string(&error).unwrap();
        let deserialized: IpcError = serde_json::from_str(&json).unwrap();
        assert_eq!(error.message, deserialized.message);
    }

    #[test]
    fn test_configure_muxer_stores_config() {
        let (server, _, _) = IpcServer::new();
        assert!(server.state.muxer_config.is_none());

        let cfg = MuxerConfig::default();
        // Simulate storing
        let mut state = server.state;
        state.muxer_config = Some(cfg.clone());
        assert_eq!(state.muxer_config, Some(cfg));
    }

    #[test]
    fn test_muxer_config_roundtrip() {
        let cfg = MuxerConfig {
            base_output_path: std::path::PathBuf::from("test"),
            file_naming_strategy: FileNamingStrategy::TimestampedFile,
            max_duration: std::time::Duration::from_secs(100),
            ..Default::default()
        };

        let json = serde_json::to_string(&cfg).unwrap();
        let deserialized: MuxerConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(cfg, deserialized);
    }
}
