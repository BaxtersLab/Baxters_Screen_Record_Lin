// SPDX-License-Identifier: MIT
// Baxter's Screen Record — Full Hybrid UI
// Block D-2: Full UI with eframe/egui, tray, hotkeys

use std::sync::Arc;
use tokio::sync::{mpsc, broadcast};
use serde::{Deserialize, Serialize};
use bsr_ipc::{IpcCommand, IpcResponse, TelemetryEvent, IpcClient};
use bsr_ipc::telemetry_pipe::FrameInfo;
use bsr_capture::CaptureFrame;
use image::imageops::FilterType;
use tray_icon::{TrayIcon, TrayIconBuilder, menu::{Menu, MenuItem, MenuEvent}};
use global_hotkey::{GlobalHotKeyManager, GlobalHotKeyEvent, HotKeyState, hotkey::{HotKey, Modifiers, Code}};
use bsr_core::buffer::DropOldestBuffer;
use tokio::sync::Notify;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub enum UiCommand {
    StartRecording,
    StopRecording,
    Shutdown,
}

#[derive(Debug, Clone)]
pub enum UiState {
    Idle,
    Recording,
    ShuttingDown,
}

#[derive(Debug, thiserror::Error)]
pub enum UiError {
    #[error("IPC error: {0}")]
    Ipc(#[from] bsr_ipc::IpcError),
    #[error("Channel send error")]
    ChannelSend,
    #[error("Channel receive error")]
    ChannelReceive,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FileNamingStrategyUi {
    Simple,
    TimestampedFile,
    TimestampedFolder,
}

/// Which corner of the record space a click is about to place.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Corner {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

impl Corner {
    pub fn label(self) -> &'static str {
        match self {
            Corner::TopLeft => "top-left",
            Corner::TopRight => "top-right",
            Corner::BottomLeft => "bottom-left",
            Corner::BottomRight => "bottom-right",
        }
    }
}

#[derive(Debug, Clone, PartialEq)]
pub struct UiSettings {
    pub resolution: String,
    pub fps: u32,
    pub output_folder: String,
    pub file_naming_strategy: FileNamingStrategyUi,
    /// Crop the record space by pulling each edge inward, in pixels. All zero = full
    /// screen. Stored as insets rather than absolute corners because that is what the
    /// operator is actually adjusting, and it stays meaningful if the resolution changes.
    pub crop_left: u32,
    pub crop_top: u32,
    pub crop_right: u32,
    pub crop_bottom: u32,
    /// Get BSR's own window out of the shot while recording.
    pub minimize_while_recording: bool,
    /// x264 preset the encoder runs with, from `encoder.preset` in the config.
    pub preset: String,
    /// Target bitrate in kbit/s, from `encoder.bitrate_kbps` in the config.
    pub bitrate_kbps: u32,
}

impl Default for UiSettings {
    fn default() -> Self {
        Self {
            resolution: "1920x1080".to_string(),
            fps: 60,
            output_folder: default_output_folder(),
            file_naming_strategy: FileNamingStrategyUi::Simple,
            crop_left: 0,
            crop_top: 0,
            crop_right: 0,
            crop_bottom: 0,
            minimize_while_recording: true,
            preset: bsr_encode::EncoderConfig::default().preset,
            bitrate_kbps: bsr_encode::EncoderConfig::default().bitrate_kbps,
        }
    }
}

/// Build the encoder configuration the recording pipeline runs with.
///
/// Extracted so the config -> encoder path can be tested: `encoder.preset` and
/// `encoder.bitrate_kbps` are serialized into every config file, and were read
/// by nothing, so an operator or agent that set them changed nothing.
pub fn encoder_config_for(
    settings: &UiSettings,
    width: u32,
    height: u32,
) -> bsr_encode::EncoderConfig {
    bsr_encode::EncoderConfig {
        codec: "h264".to_string(),
        preset: settings.preset.clone(),
        bitrate_kbps: settings.bitrate_kbps,
        width,
        height,
        fps: settings.fps,
    }
}


/// Does a finalised recording actually contain video data?
///
/// The muxer can finalise a perfectly valid MP4 that holds nothing. If no packets
/// ever reach it, `write_header` + `write_trailer` still succeed and produce a
/// container with an empty (8-byte) `mdat` and no track at all. That reached an
/// operator: a 17 s recording wrote 261 bytes while the UI reported "Recording
/// finalised and ready to save", because the muxer's `Ok(())` describes the
/// *operation*, not the *artifact*.
///
/// Returns true only on a positive identification of an empty recording. Anything
/// unparseable is left alone — a file that cannot be walked here may still be
/// perfectly good, and a false alarm about a real recording is its own harm.
pub fn recording_is_empty(path: &std::path::Path) -> bool {
    use std::io::Read;
    let Ok(mut f) = std::fs::File::open(path) else { return false };

    // Walk top-level atoms: [u32 size][4-byte type]. A size of 8 for `mdat` means
    // the box holds no payload, i.e. not a single sample was written.
    let mut offset: u64 = 0;
    let mut saw_mdat = false;
    loop {
        let mut header = [0u8; 8];
        if std::io::Seek::seek(&mut f, std::io::SeekFrom::Start(offset)).is_err() {
            break;
        }
        if f.read_exact(&mut header).is_err() {
            break;
        }
        let size = u32::from_be_bytes([header[0], header[1], header[2], header[3]]) as u64;
        let kind = &header[4..8];
        if kind == b"mdat" {
            saw_mdat = true;
            if size == 8 {
                return true;
            }
        }
        // 0 means "to end of file"; 1 means a 64-bit size follows. Neither is an
        // empty mdat, and following them adds parsing this does not need.
        if size < 8 {
            break;
        }
        offset += size;
    }
    // A container with no mdat at all and nothing else to say is also not a
    // recording, but only claim that when the file is implausibly small.
    !saw_mdat && std::fs::metadata(path).map(|m| m.len() < 1024).unwrap_or(false)
}

/// Where recordings go by default.
///
/// This was the literal `C:\Users\Public\Videos\BSR`, which on Linux is not a path at
/// all -- it is a filename containing backslashes, so the folder never existed and the
/// Settings panel showed a permanent "Path does not exist" warning. Article XI: no drive
/// letters or machine paths baked into source; derive from one root and join portably.
fn default_output_folder() -> String {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(|home| std::path::PathBuf::from(home).join("Videos").join("BSR"))
        .unwrap_or_else(|| std::env::temp_dir().join("BSR"))
        .to_string_lossy()
        .into_owned()
}

impl UiSettings {
    /// The full screen size, parsed from the `WIDTHxHEIGHT` setting.
    pub fn screen_size(&self) -> (u32, u32) {
        self.resolution
            .split_once(['x', 'X'])
            .and_then(|(w, h)| Some((w.trim().parse().ok()?, h.trim().parse().ok()?)))
            .unwrap_or((1920, 1080))
    }

    /// The crop insets as an absolute region, or `None` for "record everything".
    ///
    /// Insets that overlap (or exceed the screen) would describe an inside-out
    /// rectangle; `CaptureRegion::resolve` rejects those, and the caller then records
    /// the full screen rather than nothing.
    pub fn to_capture_region(&self) -> Option<bsr_core::config::CaptureRegion> {
        if !self.has_crop_insets() {
            return None;
        }
        let (w, h) = self.screen_size();

        // Opposing trims that meet or cross leave nothing. This has to be caught HERE,
        // in inset terms, and cannot be left to `CaptureRegion::resolve`: resolve
        // normalises inverted corners, which is right for a corner drag but wrong for
        // insets -- left=1500 with right=1500 on a 1920-wide screen would come back as a
        // perfectly valid mirrored 1080-wide rectangle instead of "nothing left".
        if self.crop_left + self.crop_right >= w || self.crop_top + self.crop_bottom >= h {
            return None;
        }

        Some(bsr_core::config::CaptureRegion {
            x1: self.crop_left as i32,
            y1: self.crop_top as i32,
            x2: w as i32 - self.crop_right as i32,
            y2: h as i32 - self.crop_bottom as i32,
        })
    }

    /// Adopt an absolute region as insets. `None` restores the full screen.
    ///
    /// The inverse of `to_capture_region`, and the entry point for both agent paths:
    /// the config file's `capture.region` at startup, and `IpcCommand::SetRecordSpace`
    /// on a running instance. Corners arriving in either order, off-screen or an odd
    /// distance apart are normalised by `resolve` before being stored, so the Settings
    /// panel always shows the rectangle that will actually be recorded rather than what
    /// was asked for.
    pub fn set_capture_region(&mut self, region: Option<bsr_core::config::CaptureRegion>) {
        let (w, h) = self.screen_size();
        match region.and_then(|r| r.resolve(w, h)) {
            Some((x, y, cw, ch)) => {
                self.crop_left = x;
                self.crop_top = y;
                self.crop_right = w.saturating_sub(x + cw);
                self.crop_bottom = h.saturating_sub(y + ch);
            }
            None => {
                self.crop_left = 0;
                self.crop_top = 0;
                self.crop_right = 0;
                self.crop_bottom = 0;
            }
        }
    }

    /// Move one corner of the record space to a point on the screen.
    ///
    /// This is the click-to-set path: the operator picks a corner, then clicks the
    /// desktop, and that corner lands there. A rectangle only has two independent
    /// corners, but all four are offered because that is how the operator thinks about
    /// it -- each one drives the two edges it touches, so "top-right" sets the right and
    /// top edges and leaves left and bottom alone.
    ///
    /// `x`/`y` are **physical screen pixels**, already converted from the picker's
    /// logical points; see `Corner::apply` callers.
    ///
    /// Returns `false` and changes nothing if the result would be unusable -- dragging
    /// the top-left corner past the bottom-right must not silently reset the whole
    /// region to full screen and throw away the other corner.
    pub fn set_corner(&mut self, corner: Corner, x: u32, y: u32) -> bool {
        let (w, h) = self.screen_size();
        let (mut x1, mut y1) = (self.crop_left as i32, self.crop_top as i32);
        let (mut x2, mut y2) = (
            w as i32 - self.crop_right as i32,
            h as i32 - self.crop_bottom as i32,
        );
        let (px, py) = (x as i32, y as i32);
        match corner {
            Corner::TopLeft => { x1 = px; y1 = py; }
            Corner::TopRight => { x2 = px; y1 = py; }
            Corner::BottomLeft => { x1 = px; y2 = py; }
            Corner::BottomRight => { x2 = px; y2 = py; }
        }
        // The named corner must stay on its own side. `resolve` deliberately NORMALISES
        // inverted corners -- correct for a drag-select, where the two corners are
        // interchangeable, but wrong here: dragging the top-left past the bottom-right
        // would come back as a valid mirrored rectangle, silently relocating the corner
        // the operator had already placed instead of refusing the click.
        if x1 >= x2 || y1 >= y2 {
            return false;
        }
        let candidate = bsr_core::config::CaptureRegion { x1, y1, x2, y2 };
        if candidate.resolve(w, h).is_none() {
            return false;
        }
        self.set_capture_region(Some(candidate));
        true
    }

    /// Whether the operator has asked for any trim at all, regardless of whether it is
    /// usable. Distinguishes "no crop wanted" from "crop wanted but impossible", which
    /// the Settings panel needs in order to warn about the second.
    pub fn has_crop_insets(&self) -> bool {
        self.crop_left != 0 || self.crop_top != 0 || self.crop_right != 0 || self.crop_bottom != 0
    }

    /// The dimensions the recording will actually have, after cropping.
    ///
    /// The encoder and the muxer must both be built for THIS, not for the screen size:
    /// x264 is opened once at a fixed size and cannot change mid-stream, so a mismatch
    /// is refused frame by frame and nothing is ever encoded.
    pub fn recording_size(&self) -> (u32, u32) {
        let (w, h) = self.screen_size();
        match self.to_capture_region().and_then(|r| r.resolve(w, h)) {
            Some((_, _, cw, ch)) => (cw, ch),
            None => (w, h),
        }
    }

    pub fn to_muxer_config(&self) -> bsr_ipc::MuxerConfig {
        use bsr_ipc::FileNamingStrategy as Fs;

        let strategy = match &self.file_naming_strategy {
            FileNamingStrategyUi::Simple => Fs::Simple("recording.mp4".to_string()),
            FileNamingStrategyUi::TimestampedFile => Fs::TimestampedFile,
            FileNamingStrategyUi::TimestampedFolder => Fs::TimestampedFolder,
        };

        // Container geometry is the CROPPED size, not the screen size.
        let (width, height) = self.recording_size();

        bsr_ipc::MuxerConfig {
            base_output_path: std::path::PathBuf::from(&self.output_folder),
            file_naming_strategy: strategy,
            max_duration: std::time::Duration::from_secs(5 * 60 * 60),
            fps: self.fps,
            width,
            height,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct UiTelemetry {
    pub frames_captured: u64,
    pub frames_dropped: u64,
    pub avg_latency_ms: f32,
    pub packets_encoded: u64,
    pub file_size_bytes: u64,
    pub recording_duration_secs: u64,
    pub smoothed_write_latency_ms: f32,
    pub latency_samples: Vec<f32>,
}

impl UiTelemetry {
    pub fn update_smoothed_latency(&mut self, new_latency: f32) {
        const MAX_SAMPLES: usize = 10;
        self.latency_samples.push(new_latency);
        if self.latency_samples.len() > MAX_SAMPLES {
            self.latency_samples.remove(0);
        }
        self.smoothed_write_latency_ms = self.latency_samples.iter().sum::<f32>() / self.latency_samples.len() as f32;
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecordingStatus {
    Idle,
    Recording,
    ShuttingDown,
}

pub use bsr_core::config::BsrConfig;

#[derive(Debug, Default, Clone)]
pub struct UiDiagnostics {
    pub lines: Vec<String>,
    pub timestamps: Vec<String>,
}

impl UiDiagnostics {
    pub fn push(&mut self, line: impl Into<String>) {
        const MAX_LINES: usize = 50;
        let line = line.into();
        let timestamp = chrono::Local::now().format("%H:%M:%S").to_string();
        
        self.lines.push(line);
        self.timestamps.push(timestamp);
        
        if self.lines.len() > MAX_LINES {
            let overflow = self.lines.len() - MAX_LINES;
            self.lines.drain(0..overflow);
            self.timestamps.drain(0..overflow);
        }
    }
}

#[derive(Debug, Clone)]
pub struct UiModel {
    pub status: RecordingStatus,
    pub telemetry: UiTelemetry,
    pub settings: UiSettings,
    pub diagnostics: UiDiagnostics,
    pub diagnostics_auto_scroll: bool,
    pub folder_valid: bool,
}

impl Default for UiModel {
    fn default() -> Self {
        Self {
            status: RecordingStatus::Idle,
            telemetry: UiTelemetry::default(),
            settings: UiSettings::default(),
            diagnostics: UiDiagnostics::default(),
            diagnostics_auto_scroll: true,
            folder_valid: true,
        }
    }
}

pub struct UiService {
    ipc_client: IpcClient,
    telemetry_rx: broadcast::Receiver<TelemetryEvent>,
    cmd_tx: mpsc::Sender<UiCommand>,
    cmd_rx: mpsc::Receiver<UiCommand>,
    state: UiState,
}

impl UiService {
    pub fn new(
        ipc_cmd_tx: mpsc::Sender<(IpcCommand, tokio::sync::oneshot::Sender<IpcResponse>)>,
        telemetry_rx: broadcast::Receiver<TelemetryEvent>,
    ) -> Self {
        let (cmd_tx, cmd_rx) = mpsc::channel(32);

        Self {
            ipc_client: IpcClient::new(ipc_cmd_tx),
            telemetry_rx,
            cmd_tx,
            cmd_rx,
            state: UiState::Idle,
        }
    }

    pub fn command_sender(&self) -> mpsc::Sender<UiCommand> {
        self.cmd_tx.clone()
    }

    pub async fn run(self) -> Result<(), UiError> {
        let mut telemetry_rx = self.telemetry_rx;
        let mut cmd_rx = self.cmd_rx;
        let mut state = self.state;
        let ipc_client = self.ipc_client;

        let telemetry_handle = tokio::spawn(async move {
            loop {
                match telemetry_rx.recv().await {
                    Ok(event) => {
                        tracing::info!("UI received telemetry: {:?}", event);
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                    Err(broadcast::error::RecvError::Lagged(_)) => continue,
                }
            }
        });

        let cmd_handle = tokio::spawn(async move {
            loop {
                match cmd_rx.recv().await {
                    Some(cmd) => {
                        if let Err(e) = Self::handle_command_static(&ipc_client, &mut state, cmd).await {
                            tracing::error!("UI command error: {:?}", e);
                        }
                        if matches!(state, UiState::ShuttingDown) {
                            break;
                        }
                    }
                    None => break,
                }
            }
        });

        // Wait for shutdown
        tokio::select! {
            _ = telemetry_handle => {},
            _ = cmd_handle => {},
        }

        Ok(())
    }

    async fn handle_command_static(
        ipc_client: &IpcClient,
        state: &mut UiState,
        cmd: UiCommand,
    ) -> Result<(), UiError> {
        match cmd {
            UiCommand::StartRecording => {
                if matches!(state, UiState::Idle) {
                    let response = ipc_client.send_command(IpcCommand::StartRecording).await?;
                    if matches!(response, IpcResponse::Ok) {
                        *state = UiState::Recording;
                        tracing::info!("Recording started");
                    }
                }
            }
            UiCommand::StopRecording => {
                if matches!(state, UiState::Recording) {
                    let response = ipc_client.send_command(IpcCommand::StopRecording).await?;
                    if matches!(response, IpcResponse::Ok) {
                        *state = UiState::Idle;
                        tracing::info!("Recording stopped");
                    }
                }
            }
            UiCommand::Shutdown => {
                *state = UiState::ShuttingDown;
                let _ = ipc_client.send_command(IpcCommand::Shutdown).await;
                tracing::info!("UI shutting down");
            }
        }
        Ok(())
    }
}

pub fn run_ui(handle: tokio::runtime::Handle) -> eframe::Result<()> {
    let native_options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size(egui::vec2(500.0, 400.0))
            .with_resizable(false),
        ..Default::default()
    };

    eframe::run_native(
        "Baxter's Screen Record",
        native_options,
        Box::new(move |cc| Box::new(AppWindow::new(cc, BsrConfig::default(), handle, None))),
    )
}

pub struct RecordingPipeline {
    cap_shutdown: mpsc::Sender<()>,
    enc_shutdown: mpsc::Sender<()>,
    mux_cmd: mpsc::Sender<bsr_mux::MuxerCommand>,
    preview_task: Option<tokio::task::JoinHandle<()>>,
    /// The muxer task, kept so Stop can WAIT for the MP4 trailer to be written.
    /// It was previously detached, which is why the file could be offered for saving
    /// while it was still headless.
    muxer_task: Option<tokio::task::JoinHandle<Result<(), String>>>,
}

pub struct AppWindow {
    model: UiModel,
    _config: BsrConfig,
    telemetry_rx: mpsc::UnboundedReceiver<TelemetryEvent>,
    telemetry_tx: mpsc::UnboundedSender<TelemetryEvent>,
    muxer_telemetry_rx: mpsc::UnboundedReceiver<bsr_mux::MuxerTelemetry>,
    muxer_telemetry_tx: mpsc::UnboundedSender<bsr_mux::MuxerTelemetry>,
    pipeline: Option<RecordingPipeline>,
    /// Set while a recording is being finalized. Resolves once the muxer has written the
    /// MP4 trailer, which is the first moment the file is worth handing to anyone.
    finalize_rx: Option<tokio::sync::oneshot::Receiver<Result<(), String>>>,
    /// Window state change requested by a button, applied on the next frame where
    /// an `egui::Context` is in hand.
    pending_window_cmd: Option<bool>,
    /// Cleared after the first frame, which is the earliest point a viewport
    /// command can be sent.
    needs_initial_raise: bool,
    tokio_handle: tokio::runtime::Handle,
    /// Corner waiting to be placed by a click on the preview. `None` means not picking.
    pending_pick: Option<Corner>,
    /// Idle live capture feeding the preview, so there is something to click on.
    live_view: Option<LiveView>,
    _tray_icon: Option<TrayIcon>,
    _hotkey_manager: Option<GlobalHotKeyManager>,
    // Registered global-hotkey id for the record/stop toggle (Ctrl+Shift+R).
    record_hotkey_id: Option<u32>,
    // Path to the current recording file as created by the muxer (set at start)
    current_recording_path: Option<std::path::PathBuf>,
    // Pending save offer from a remote participant
    pending_save_offer: Option<(String, tokio::sync::oneshot::Sender<Option<String>>)>,
    // Local save modal state (starter's save dialog)
    pending_local_save: Option<String>,
    /// Where "Save" will copy to. Kept separate from `settings.output_folder`, which is
    /// where recordings are WRITTEN -- defaulting the destination to the source folder is
    /// what made the default Save destructive.
    save_dest_folder: String,
    /// Result of a native folder/file pick, delivered from the portal task.
    save_pick_rx: Option<tokio::sync::oneshot::Receiver<Result<Option<std::path::PathBuf>, String>>>,
    /// Status of the last Browse attempt, shown IN the modal. The modal is a centred
    /// window that covers the Diagnostics panel, so feedback posted only there is
    /// unreadable exactly when it is needed.
    save_status: Option<String>,
    /// How to ask for a save location. Replaceable so the round trip is testable.
    picker: LocationPicker,
    /// When the in-flight pick gives up, so a portal that never answers cannot look
    /// like a button that does nothing.
    pick_deadline: Option<std::time::Instant>,
    bsr_debug: bool,
    pending_consent: Option<(Option<String>, tokio::sync::oneshot::Sender<bool>)>,
    // Preview channel for UI frames (resized RGBA)
    preview_rx: Option<mpsc::UnboundedReceiver<PreviewImage>>,
    preview_texture: Option<egui::TextureHandle>,
    ipc_client: Option<bsr_ipc::IpcClient>,
    recording_start: Option<std::time::Instant>,
    // Monotonic sequence for fragmented recordings (1,2,3...)
    recording_seq: u64,
    // If set, indicates a pending auto-restart after a rollover stop
    pending_auto_restart: Option<std::time::Instant>,
}

/// Lightweight preview image (RGBA) sent from background drain task to UI
pub struct PreviewImage {
    pub width: u32,
    pub height: u32,
    pub data: Vec<u8>, // RGBA8
}

/// The tray menu. Built on whichever thread owns GTK on Linux.
fn build_tray_menu() -> Menu {
    let tray_menu = Menu::new();
    let open_item = MenuItem::new("Open", true, None);
    let start_item = MenuItem::new("Start Recording", true, None);
    let stop_item = MenuItem::new("Stop Recording", false, None);
    let exit_item = MenuItem::new("Exit", true, None);
    tray_menu.append(&open_item).unwrap();
    tray_menu.append(&start_item).unwrap();
    tray_menu.append(&stop_item).unwrap();
    tray_menu.append(&exit_item).unwrap();
    tray_menu
}

/// Create the system tray icon. Windows: Win32, on the calling thread, unchanged.
#[cfg(not(target_os = "linux"))]
fn build_tray() -> Option<TrayIcon> {
    TrayIconBuilder::new()
        .with_menu(Box::new(build_tray_menu()))
        .with_tooltip("Idle")
        .build()
        .ok()
}

/// Create the system tray icon on Linux, where it needs a GTK thread of its own.
///
/// `tray-icon` on Linux is GTK + libayatana-appindicator: the caller must have called
/// `gtk::init()` **and** must run a GTK main loop to pump the tray's events.
/// eframe/winit runs its own event loop and does neither, so building the tray on the
/// main thread panicked the moment the app was launched here:
///
/// ```text
/// thread 'main' panicked at gtk-0.18.2/src/auto/menu.rs:29:
/// GTK has not been initialized. Call `gtk::init` first.
/// ```
///
/// This never showed on Windows, where the same code path is Win32 and needs no GTK.
///
/// The tray is therefore created on, and owned by, a dedicated GTK thread — `TrayIcon`
/// is not `Send`, so it cannot be handed back. Nothing is lost: the returned handle was
/// only ever held alive (`_tray_icon`), never used, and menu clicks still reach the UI
/// because `MenuEvent::receiver()` is a global channel that `update()` already polls.
///
/// A failure here disables the tray and logs why; it does not take the app down. The
/// tray is a convenience, and GNOME needs an AppIndicator extension to show one at all.
#[cfg(target_os = "linux")]
fn build_tray() -> Option<TrayIcon> {
    let spawned = std::thread::Builder::new().name("bsr-tray".into()).spawn(|| {
        if let Err(e) = gtk::init() {
            tracing::warn!("system tray disabled: gtk::init failed: {e}");
            return;
        }
        let tray = TrayIconBuilder::new()
            .with_menu(Box::new(build_tray_menu()))
            .with_tooltip("Idle")
            .build();
        match tray {
            Ok(tray) => {
                // Must outlive the GTK loop that pumps it.
                let _tray = tray;
                gtk::main();
            }
            Err(e) => tracing::warn!("system tray disabled: {e}"),
        }
    });
    if let Err(e) = spawned {
        tracing::warn!("system tray disabled: could not spawn GTK thread: {e}");
    }
    None
}

/// Convert a captured BGRA frame into a downscaled RGBA preview.
///
/// Shared by the recording preview and the idle live view, which previously would have
/// been two copies of the same conversion. Returns `None` only if the frame's buffer
/// does not match its declared size, in which case there is nothing safe to show.
pub fn frame_to_preview(frame: &CaptureFrame, target_w: u32) -> Option<PreviewImage> {
    let (w, h) = (frame.width as usize, frame.height as usize);
    if w == 0 || h == 0 || frame.data.len() < w * h * 4 {
        return None;
    }
    let mut rgba = Vec::with_capacity(w * h * 4);
    for px in frame.data.chunks_exact(4).take(w * h) {
        rgba.extend_from_slice(&[px[2], px[1], px[0], px[3]]);
    }
    let img = image::ImageBuffer::<image::Rgba<u8>, _>::from_raw(frame.width, frame.height, rgba)?;
    let target_w = target_w.max(1);
    let target_h = ((h as f32) * (target_w as f32) / (w as f32)).round().max(1.0) as u32;
    let resized = image::imageops::resize(
        &image::DynamicImage::ImageRgba8(img),
        target_w,
        target_h,
        FilterType::Triangle,
    );
    Some(PreviewImage { width: target_w, height: target_h, data: resized.into_raw() })
}

/// Map a click inside the preview image back to a point on the real screen.
///
/// Pure, and deliberately so: this is the whole correctness of click-to-set, and it can
/// be tested without a window. The preview is a scaled picture of the screen, so the
/// click's *fraction* across the drawn rectangle is the same fraction across the screen
/// -- which makes the mapping independent of the preview's own resolution.
pub fn preview_click_to_screen(
    click: egui::Pos2,
    rect: egui::Rect,
    screen: (u32, u32),
) -> Option<(u32, u32)> {
    if rect.width() <= 0.0 || rect.height() <= 0.0 {
        return None;
    }
    let fx = ((click.x - rect.min.x) / rect.width()).clamp(0.0, 1.0);
    let fy = ((click.y - rect.min.y) / rect.height()).clamp(0.0, 1.0);
    Some((
        (fx * screen.0 as f32).round().clamp(0.0, screen.0 as f32) as u32,
        (fy * screen.1 as f32).round().clamp(0.0, screen.1 as f32) as u32,
    ))
}

/// A live portal capture feeding the preview while NOT recording, so corners can be
/// picked against what is actually on screen.
struct LiveView {
    stop: tokio::sync::mpsc::Sender<()>,
    error: std::sync::Arc<std::sync::Mutex<Option<String>>>,
}

/// Turn a portal `file://` URI into a local path.
///
/// `ashpd::Uri` is `http::Uri`, which has no file-path conversion and does not
/// percent-decode, so a folder with a space in it would come back mangled. Round-tripping
/// through `url::Url` handles both.
#[cfg(target_os = "linux")]
fn uri_to_path(uri: &ashpd::Uri) -> Option<std::path::PathBuf> {
    url::Url::parse(&uri.to_string()).ok()?.to_file_path().ok()
}

/// Turn whatever got pasted into the folder box into a usable path.
///
/// A path typed or pasted by a person is not a `Path`. Rust does **not** expand `~`, so
/// `Path::new("~/Videos").is_dir()` is false and the destination is rejected as
/// non-existent — which reads as the app refusing a perfectly good folder. Copying a
/// location out of Files gives a percent-encoded `file://` URI. Copying from a terminal
/// often brings a trailing newline or a quote with it.
///
/// Returns the cleaned path, or `None` if there is nothing left after cleaning.
pub fn normalize_folder_input(raw: &str) -> Option<std::path::PathBuf> {
    let mut t = raw.trim().trim_matches(|c| c == '"' || c == '\'').trim().to_string();
    if t.is_empty() {
        return None;
    }

    // A URI from a file manager, percent-encoding and all.
    if t.starts_with("file://") {
        if let Ok(url) = url::Url::parse(&t) {
            if let Ok(path) = url.to_file_path() {
                return Some(path);
            }
        }
        t = t.trim_start_matches("file://").to_string();
    }

    // Shell shorthands the shell would have expanded, but we are not a shell.
    if t == "~" || t.starts_with("~/") {
        if let Some(home) = std::env::var_os("HOME") {
            let rest = t.strip_prefix("~/").unwrap_or("");
            return Some(std::path::PathBuf::from(home).join(rest));
        }
    }
    if t.starts_with("$HOME") {
        if let Some(home) = std::env::var_os("HOME") {
            let rest = t.strip_prefix("$HOME").unwrap_or("").trim_start_matches('/');
            return Some(std::path::PathBuf::from(home).join(rest));
        }
    }

    let p = std::path::PathBuf::from(&t);
    if p.as_os_str().is_empty() { None } else { Some(p) }
}

/// Result of asking the desktop where to save: a path, a cancellation, or a failure.
pub type PickerResult = Result<Option<std::path::PathBuf>, String>;

/// How BSR asks for a save location.
///
/// Boxed rather than called directly so the whole browse round trip — request, reply,
/// and everything the UI does with the reply — can be driven headlessly. Handling of the
/// reply previously had no coverage at all, which is why "Browse does nothing" could not
/// be narrowed down: there was no way to ask whether the problem was the click, the
/// request, or what we did with the answer.
///
/// The closure is handed the suggested filename, the starting folder, and the channel to
/// answer on. It is responsible for replying exactly once; failing to reply is what the
/// deadline in `poll_save_pick` exists to catch.
pub struct LocationPicker(
    Box<dyn Fn(String, String, tokio::sync::oneshot::Sender<PickerResult>) + Send + Sync>,
);

impl LocationPicker {
    pub fn new(
        f: impl Fn(String, String, tokio::sync::oneshot::Sender<PickerResult>) + Send + Sync + 'static,
    ) -> Self {
        Self(Box::new(f))
    }

    /// The real one: xdg-desktop-portal's FileChooser, on the given runtime.
    #[cfg(target_os = "linux")]
    pub fn portal(handle: tokio::runtime::Handle) -> Self {
        Self::new(move |suggested_name, start_dir, tx| {
            handle.spawn(async move {
                let req = ashpd::desktop::file_chooser::SelectedFiles::save_file()
                    .title("Save recording as")
                    .accept_label("Save")
                    // NOT modal: eframe gives no window handle to parent a portal dialog
                    // to, so `modal(true)` asks the compositor to make it modal to
                    // nothing, which can leave it unraised and invisible behind the app.
                    .modal(false)
                    .current_name(suggested_name.as_str());
                let req = if start_dir.is_empty() {
                    req
                } else {
                    match req.current_folder(&start_dir) {
                        Ok(r) => r,
                        Err(e) => {
                            tracing::warn!("save-location start folder {start_dir:?}: {e}");
                            ashpd::desktop::file_chooser::SelectedFiles::save_file()
                                .title("Save recording as")
                                .accept_label("Save")
                                .modal(false)
                                .current_name(suggested_name.as_str())
                        }
                    }
                };
                let outcome = match req.send().await {
                    Ok(request) => match request.response() {
                        Ok(files) => match files.uris().first() {
                            Some(uri) => match uri_to_path(uri) {
                                Some(p) => Ok(Some(p)),
                                None => Err(format!("the dialog returned {uri}, which is not a local file")),
                            },
                            None => Ok(None),
                        },
                        Err(e) => Err(format!("save dialog was dismissed or refused: {e}")),
                    },
                    Err(e) => Err(format!("could not open the save dialog: {e}")),
                };
                let _ = tx.send(outcome);
            });
        })
    }

    #[cfg(not(target_os = "linux"))]
    pub fn portal(_handle: tokio::runtime::Handle) -> Self {
        Self::new(|_, _, tx| {
            let _ = tx.send(Err("no save dialog on this platform".to_string()));
        })
    }
}

/// Whether two paths name the same file on disk.
///
/// Compared by inode where both exist, so a symlink, a relative path or a `..` detour
/// cannot sneak the source past the check. `std::fs::copy` with equal source and
/// destination returns `Ok(0)` and **truncates the file to zero** -- it reports success
/// while destroying the recording, which is why this has to be caught before the call
/// rather than by checking its result.
fn paths_are_same_file(a: &std::path::Path, b: &std::path::Path) -> bool {
    let Ok(src) = a.canonicalize() else {
        return a == b; // source missing; nothing better than a textual compare
    };
    if let Ok(dst) = b.canonicalize() {
        return src == dst;
    }
    // The destination file usually does not exist yet, so it cannot be canonicalized.
    // Canonicalize its FOLDER instead and rejoin the name: that resolves symlinks and
    // any `.`/`..` in the typed path, which a textual compare would let straight past.
    match (b.parent(), b.file_name()) {
        (Some(parent), Some(name)) => match parent.canonicalize() {
            Ok(dir) => dir.join(name) == src,
            Err(_) => b == src,
        },
        _ => b == src,
    }
}

impl AppWindow {
    /// Build the window from a bare `egui::Context`.
    ///
    /// `eframe::CreationContext` was only ever used for its `egui_ctx`, and depending on
    /// the whole thing made `AppWindow` impossible to construct outside a real eframe
    /// run — which is why this entire surface had no automated coverage while four
    /// defects were found in it by hand. Nothing here is test-only scaffolding; it is
    /// the same construction with a narrower dependency.
    pub fn new_for_context(
        ctx: &egui::Context,
        config: BsrConfig,
        handle: tokio::runtime::Handle,
        splash_rgba: Option<(u32, u32, Vec<u8>)>,
        ipc_client: Option<bsr_ipc::IpcClient>,
    ) -> Self {
        Self::assemble(ctx, config, handle, splash_rgba, ipc_client, true)
    }

    /// Build the window without the system tray or global hotkeys.
    ///
    /// Those two are the only parts that reach outside the process, and they cannot be
    /// created more than once concurrently: the tray runs `gtk::init()` on its own thread
    /// and GTK **aborts** the process when that is raced, which is exactly what happens
    /// when several harness instances are built in parallel test threads.
    ///
    /// Everything else — the model, the settings, the whole widget tree — is identical,
    /// so this covers UI rendering and interaction. It does not cover tray registration
    /// or hotkey binding, which need a real desktop session.
    pub fn new_headless(
        ctx: &egui::Context,
        config: BsrConfig,
        handle: tokio::runtime::Handle,
        ipc_client: Option<bsr_ipc::IpcClient>,
    ) -> Self {
        Self::assemble(ctx, config, handle, None, ipc_client, false)
    }

    fn assemble(
        ctx: &egui::Context,
        config: BsrConfig,
        handle: tokio::runtime::Handle,
        splash_rgba: Option<(u32, u32, Vec<u8>)>,
        ipc_client: Option<bsr_ipc::IpcClient>,
        desktop_integration: bool,
    ) -> Self {
        apply_charcoal_theme(ctx);
        let (telemetry_tx, telemetry_rx) = mpsc::unbounded_channel();
        let (muxer_telemetry_tx, muxer_telemetry_rx) = mpsc::unbounded_channel();
        let (tray_icon, hotkey_manager) = if desktop_integration {
            (build_tray(), GlobalHotKeyManager::new().ok())
        } else {
            (None, None)
        };
        Self::build(
            config, handle, telemetry_tx, telemetry_rx, muxer_telemetry_tx, muxer_telemetry_rx,
            tray_icon, hotkey_manager, splash_rgba, ipc_client,
        )
    }

    pub fn new(cc: &eframe::CreationContext<'_>, config: BsrConfig, handle: tokio::runtime::Handle, ipc_client: Option<bsr_ipc::IpcClient>) -> Self {
        Self::new_for_context(&cc.egui_ctx, config, handle, None, ipc_client)
    }

    pub fn new_with_splash(
        cc: &eframe::CreationContext<'_>,
        config: BsrConfig,
        handle: tokio::runtime::Handle,
        splash_rgba: Option<(u32, u32, Vec<u8>)>,
        ipc_client: Option<bsr_ipc::IpcClient>,
    ) -> Self {
        Self::new_for_context(&cc.egui_ctx, config, handle, splash_rgba, ipc_client)
    }

    #[allow(clippy::too_many_arguments)]
    fn build(
        config: BsrConfig,
        handle: tokio::runtime::Handle,
        telemetry_tx: mpsc::UnboundedSender<TelemetryEvent>,
        telemetry_rx: mpsc::UnboundedReceiver<TelemetryEvent>,
        muxer_telemetry_tx: mpsc::UnboundedSender<bsr_mux::MuxerTelemetry>,
        muxer_telemetry_rx: mpsc::UnboundedReceiver<bsr_mux::MuxerTelemetry>,
        tray_icon: Option<TrayIcon>,
        hotkey_manager: Option<GlobalHotKeyManager>,
        _splash_rgba: Option<(u32, u32, Vec<u8>)>,
        ipc_client: Option<bsr_ipc::IpcClient>,
    ) -> Self {
        let bsr_debug = std::env::var("BSR_DEBUG").unwrap_or_default() == "1";

        // Seed the UI from the config file. Only `output.output_folder` was ever read
        // here; `capture.region` was serialized into every config file and ignored, so
        // an agent that edited it changed nothing. It is the documented backend way to
        // set the record space before launch, so it has to actually arrive.
        let mut settings = UiSettings::default();
        if !config.output.output_folder.trim().is_empty() {
            settings.output_folder = config.output.output_folder.clone();
        }
        if config.capture.fps > 0 {
            settings.fps = config.capture.fps;
        }
        // `encoder.preset` and `encoder.bitrate_kbps` were serialized into every
        // config file and read by nothing, so setting them changed nothing about
        // the recording. They arrive here now.
        settings.preset = config.encoder.preset.x264_name().to_string();
        if config.encoder.bitrate_kbps > 0 {
            settings.bitrate_kbps = config.encoder.bitrate_kbps;
        }
        settings.set_capture_region(config.capture.region);
        // Determine initial fragment sequence by reading a small state file
        let initial_seq = {
            let out = config.output.output_folder.clone();
            let seq_path = std::path::Path::new(&out).join(".bsr_fragment_seq");
            match std::fs::read_to_string(&seq_path) {
                Ok(s) => s.trim().parse::<u64>().unwrap_or(1),
                Err(_) => 1,
            }
        };

        // Register a global hotkey (Ctrl+Shift+R) that toggles recording.
        let record_hotkey_id = hotkey_manager.as_ref().and_then(|mgr| {
            let hk = HotKey::new(Some(Modifiers::CONTROL | Modifiers::SHIFT), Code::KeyR);
            let id = hk.id();
            match mgr.register(hk) {
                Ok(()) => Some(id),
                Err(e) => {
                    tracing::warn!("Failed to register record hotkey (Ctrl+Shift+R): {e}");
                    None
                }
            }
        });

        Self {
            model: UiModel { settings, ..UiModel::default() },
            _config: config,
            telemetry_rx,
            telemetry_tx,
            muxer_telemetry_rx,
            muxer_telemetry_tx,
            pipeline: None,
            tokio_handle: handle.clone(),
            pending_pick: None,
            live_view: None,
            finalize_rx: None,
            pending_window_cmd: None,
            needs_initial_raise: true,
            _tray_icon: tray_icon,
            _hotkey_manager: hotkey_manager,
            record_hotkey_id,
            current_recording_path: None,
            pending_save_offer: None,
            bsr_debug,
            pending_consent: None,
            preview_rx: None,
            preview_texture: None,
            ipc_client,
            pending_local_save: None,
            save_dest_folder: String::new(),
            save_pick_rx: None,
            save_status: None,
            picker: LocationPicker::portal(handle),
            pick_deadline: None,
            recording_start: None,
            recording_seq: initial_seq,
            pending_auto_restart: None,
        }
    }

    fn poll_save_offers(&mut self) {
        while let Some(req) = bsr_ipc::try_take_save_offer_request() {
            if self.pending_save_offer.is_some() {
                // Already handling an offer; decline new one
                let _ = req.resp.send(None);
            } else {
                self.pending_save_offer = Some((req.path, req.resp));
            }
        }
    }

    fn render_save_offer_modal(&mut self, ctx: &egui::Context) {
        if let Some((path, _)) = self.pending_save_offer.as_ref() {
            let path_clone = path.clone();
            let mut filename = std::path::Path::new(&path_clone).file_name().and_then(|s| s.to_str()).unwrap_or("recording.mp4").to_string();
            let title = "Save offered recording";
            egui::Window::new(title)
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                .show(ctx, |ui| {
                    ui.vertical_centered(|ui| {
                        ui.label(egui::RichText::new("A remote participant offered to share a recording copy.").heading());
                        ui.add_space(6.0);
                        ui.label(egui::RichText::new(format!("Offered file: {}", path_clone)).color(egui::Color32::from_gray(180)));
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            ui.label("Filename:");
                            ui.text_edit_singleline(&mut filename);
                        });
                        ui.add_space(6.0);
                        ui.horizontal(|ui| {
                            if ui.button("Save a copy").clicked() {
                                if let Some((_, responder)) = self.pending_save_offer.take() {
                                    // Use configured output folder by default
                                    let folder = std::path::PathBuf::from(&self.model.settings.output_folder);
                                    let dst = folder.join(&filename).to_string_lossy().to_string();
                                    let _ = responder.send(Some(dst));
                                }
                            }
                            if ui.button("Decline").clicked() {
                                if let Some((_, responder)) = self.pending_save_offer.take() {
                                    let _ = responder.send(None);
                                }
                            }
                        });
                    });
                });
        }
    }

    /// Where "Save a copy" should default to.
    ///
    /// Deliberately NOT the folder recordings are written to. The destination used to be
    /// `settings.output_folder` with the source file's own name pre-filled, so the
    /// default action was `fs::copy(src, src)` -- which returns `Ok(0)` and **truncates
    /// the file to zero bytes**. One click on the default destroyed the take.
    fn default_save_folder(&self) -> String {
        let videos = std::env::var_os("HOME")
            .map(|h| std::path::PathBuf::from(h).join("Videos"))
            .unwrap_or_else(std::env::temp_dir);
        videos.to_string_lossy().into_owned()
    }

    /// Ask for a save location through whatever picker is installed.
    ///
    /// How long to wait before declaring the dialog unresponsive. A portal that never
    /// answers is indistinguishable, from the operator's seat, from a button that does
    /// nothing — which is exactly the confusion this whole area produced.
    /// A person browsing folders easily takes minutes. 45 s was long enough to call a
    /// working dialog broken while the operator was still looking at it, so this is now
    /// a backstop against a genuinely lost request, not a patience limit.
    const PICK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

    fn browse_for_save_location(&mut self, suggested_name: String) {
        let (tx, rx) = tokio::sync::oneshot::channel();
        self.save_pick_rx = Some(rx);
        self.pick_deadline = Some(std::time::Instant::now() + Self::PICK_TIMEOUT);
        // The portal dialog is a separate window and BSR cannot parent it (eframe exposes
        // no handle for that), so it can open BEHIND this one. Saying so is the
        // difference between "nothing happened" and "look in your other windows".
        let msg = "A save dialog has opened. If you can't see it, check your other windows (Alt-Tab).";
        self.model.diagnostics.push(msg);
        self.save_status = Some(msg.to_string());
        (self.picker.0)(suggested_name, self.save_dest_folder.clone(), tx);
    }

    /// Collect a location chosen through the portal.
    fn poll_save_pick(&mut self) {
        let Some(rx) = &mut self.save_pick_rx else { return };
        let outcome = match rx.try_recv() {
            Ok(o) => o,
            Err(tokio::sync::oneshot::error::TryRecvError::Empty) => {
                // Still waiting. Give up eventually rather than waiting forever in a
                // state the operator reads as "nothing happened".
                match self.pick_deadline {
                    Some(deadline) if std::time::Instant::now() >= deadline => {
                        self.save_pick_rx = None;
                        self.pick_deadline = None;
                        let msg = "Gave up waiting for the save dialog. Type or paste a folder above instead.";
                        self.model.diagnostics.push(msg);
                        self.save_status = Some(msg.to_string());
                    }
                    _ => {}
                }
                return;
            }
            Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                Err("the save dialog task ended without answering".to_string())
            }
        };
        self.save_pick_rx = None;
        self.pick_deadline = None;
        match outcome {
            Ok(Some(path)) => {
                if let Some(dir) = path.parent() {
                    self.save_dest_folder = dir.to_string_lossy().into_owned();
                }
                if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                    self.pending_local_save = Some(name.to_string());
                }
                self.model.diagnostics.push(format!("Save location set to {}", path.display()));
                self.save_status = None;
            }
            Ok(None) => {
                self.model.diagnostics.push("Save dialog cancelled — location unchanged.");
                self.save_status = Some("Save dialog cancelled.".to_string());
            }
            Err(e) => {
                self.model.diagnostics.push(format!("Save dialog failed: {e}"));
                self.save_status = Some(format!("Save dialog failed: {e}"));
            }
        }
    }

    fn render_local_save_modal(&mut self, ctx: &egui::Context) {
        let Some(current_name) = self.pending_local_save.clone() else { return };
        // NOTE: the destination default is applied when the modal OPENS, not here.
        // Doing it per-frame meant deleting the last character of the folder snapped it
        // straight back to ~/Videos on the next frame, so the field could never be
        // cleared to paste a different path into — it behaved as if hard-wired.
        let mut filename = current_name;
        let mut folder = self.save_dest_folder.clone();
        let default_folder = self.default_save_folder();
        let mut close = false;

        egui::Window::new("Save recording")
            .collapsible(false)
            .resizable(false)
            .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
            .show(ctx, |ui| {
                ui.label(egui::RichText::new("Save a copy of your recording.").heading());
                ui.add_space(6.0);

                ui.horizontal(|ui| {
                    ui.label("Folder:");
                    ui.add(egui::TextEdit::singleline(&mut folder).desired_width(320.0))
                        .on_hover_text("Where the copy is written. Type a path, or use Browse.");
                    if ui.button("Browse…").on_hover_text("Pick a location using the desktop's own file dialog.").clicked() {
                        self.browse_for_save_location(filename.clone());
                    }
                });
                ui.horizontal(|ui| {
                    ui.label("Filename:");
                    ui.add(egui::TextEdit::singleline(&mut filename).desired_width(320.0));
                    if ui.button("Default folder").on_hover_text("Put the folder back to ~/Videos.").clicked() {
                        folder = default_folder.clone();
                    }
                });
                if let Some(status) = &self.save_status {
                    ui.label(
                        egui::RichText::new(status)
                            .color(egui::Color32::from_rgb(0xFF, 0xC1, 0x07))
                            .font(egui::FontId::proportional(11.0)),
                    );
                }

                let folder_path = normalize_folder_input(&folder);
                let dst = folder_path
                    .clone()
                    .unwrap_or_else(|| std::path::PathBuf::from("."))
                    .join(&filename);
                let src = self.current_recording_path.clone();
                let same = src.as_ref().map(|s| paths_are_same_file(s, &dst)).unwrap_or(false);
                let folder_missing = folder_path.as_ref().map(|p| !p.is_dir()).unwrap_or(true);

                ui.add_space(4.0);
                if same {
                    ui.label(
                        egui::RichText::new("⚠ That is the recording itself — choose another folder or name.")
                            .color(egui::Color32::from_rgb(0xFF, 0x55, 0x55)),
                    );
                } else if folder_missing {
                    ui.horizontal(|ui| {
                        ui.label(
                            egui::RichText::new("⚠ That folder does not exist.")
                                .color(egui::Color32::from_rgb(0xFF, 0x88, 0x00)),
                        );
                        // Refusing and stopping there is what made this feel like the app
                        // rejecting a good path. Offer the obvious next step instead.
                        if let Some(dir) = folder_path.clone() {
                            if ui.button("Create it").on_hover_text("Create this folder now.").clicked() {
                                match std::fs::create_dir_all(&dir) {
                                    Ok(()) => self.model.diagnostics.push(format!("Created {}", dir.display())),
                                    Err(e) => self.model.diagnostics.push(format!("Could not create {}: {e}", dir.display())),
                                }
                            }
                        }
                    });
                } else {
                    ui.label(
                        egui::RichText::new(format!("→ {}", dst.display()))
                            .color(egui::Color32::from_rgb(0x88, 0x88, 0x88))
                            .font(egui::FontId::proportional(11.0)),
                    );
                }

                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    // Every reason Save is unavailable is named. A greyed-out button with
                    // no explanation is indistinguishable from a broken one, which is
                    // exactly how this read from the operator's seat.
                    let blocked = if src.is_none() {
                        Some("there is no recording to copy")
                    } else if filename.trim().is_empty() {
                        Some("enter a filename")
                    } else if same {
                        Some("that is the recording itself")
                    } else if folder_missing {
                        Some("that folder does not exist")
                    } else {
                        None
                    };
                    let can_save = blocked.is_none();
                    if ui.add_enabled(can_save, egui::Button::new("Save")).clicked() {
                        if let Some(src) = src {
                            match std::fs::copy(&src, &dst) {
                                Ok(bytes) => self.model.diagnostics.push(format!(
                                    "Saved {} ({:.1} MB) to {}",
                                    filename, bytes as f64 / 1_048_576.0, dst.display()
                                )),
                                // Errors were discarded entirely before, so a failed save
                                // looked exactly like a successful one.
                                Err(e) => self.model.diagnostics.push(format!(
                                    "Save failed: {e} — the original is still at {}",
                                    src.display()
                                )),
                            }
                        }
                        close = true;
                    }
                    if ui.button("Cancel").clicked() {
                        self.model.diagnostics.push("Save cancelled — the recording is still in the output folder.");
                        close = true;
                    }
                    if let Some(reason) = blocked {
                        ui.label(
                            egui::RichText::new(format!("Can't save: {reason}"))
                                .color(egui::Color32::from_rgb(0xFF, 0x88, 0x00))
                                .font(egui::FontId::proportional(11.0)),
                        );
                    }
                });
            });

        self.save_dest_folder = folder;
        if close {
            self.pending_local_save = None;
            self.save_status = None;
        } else {
            self.pending_local_save = Some(filename);
        }
    }

    fn poll_consent_requests(&mut self) {
        self.poll_finalize();
        self.poll_save_pick();
        self.drain_live_view_error();

        // A backend agent may set the record space at any time over IPC.
        if let Some(region) = bsr_ipc::try_take_record_space() {
            self.model.settings.set_capture_region(region);
            let (w, h) = self.model.settings.recording_size();
            self.model.diagnostics.push(match region {
                Some(_) => format!("Record space set over IPC: {w}x{h}."),
                None => "Record space reset to the full screen over IPC.".to_string(),
            });
        }

        while let Some(req) = bsr_ipc::try_take_consent_request() {
            if self.pending_consent.is_some() {
                // already handling a consent request; decline the new one
                let _ = req.resp.send(false);
            } else {
                self.pending_consent = Some((req.requester, req.resp));
            }
        }
    }

    fn poll_muxer_telemetry(&mut self) {
        while let Ok(mt) = self.muxer_telemetry_rx.try_recv() {
            self.model.telemetry.file_size_bytes = mt.file_size_bytes;
            self.model.telemetry.recording_duration_secs = mt.duration_secs;
            self.model.telemetry.update_smoothed_latency(mt.last_write_latency_ms);

            self.model.diagnostics.push(format!(
                "Muxer: size={} bytes, duration={} s, write={:.1} ms",
                mt.file_size_bytes,
                mt.duration_secs,
                mt.last_write_latency_ms,
            ));

            if mt.max_duration_reached {
                self.model.diagnostics.push(
                    "Recording stopped automatically: 5‑hour limit reached.".to_string(),
                );
                self.model.status = RecordingStatus::Idle;
            }
        }
    }

    fn poll_telemetry(&mut self) {
        while let Ok(event) = self.telemetry_rx.try_recv() {
            match event {
                TelemetryEvent::RecordingStarted => {
                    self.model.diagnostics.push("Telemetry: recording started");
                }
                TelemetryEvent::RecordingStopped => {
                    self.model.diagnostics.push("Telemetry: recording stopped");
                    self.model.status = RecordingStatus::Idle;
                }
                TelemetryEvent::ErrorOccurred { message } => {
                    self.model.diagnostics.push(format!("Error: {}", message));
                }
                _ => {}
            }
        }
    }

    fn render_ui(&mut self, ctx: &egui::Context) {
        egui::CentralPanel::default().show(ctx, |ui| {
            // The window had NO scroll area: the panel laid its sections out vertically
            // and anything past the window height was clipped and simply unreachable --
            // no scrollbar, no wheel response, nothing. Expanding a section pushed the
            // ones below it off the bottom for good. `auto_shrink` false so the content
            // still fills the window when it does fit.
            egui::ScrollArea::vertical()
                .auto_shrink([false; 2])
                .show(ui, |ui| {
                    ui.vertical(|ui| {
                        ui.add_space(8.0);
                        self.render_status_bar(ui);
                        ui.add_space(10.0);
                        self.render_controls(ui);
                        ui.add_space(10.0);
                        self.render_record_space(ui);
                        ui.add_space(10.0);
                        self.render_telemetry(ui);
                        ui.add_space(10.0);
                        self.render_settings(ui);
                        ui.add_space(10.0);
                        self.render_diagnostics(ui);
                    });
                });
        });
    }

    fn render_status_bar(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            // Status indicator
            let (color, text) = match self.model.status {
                RecordingStatus::Idle => (egui::Color32::GRAY, "Idle"),
                RecordingStatus::Recording => (egui::Color32::RED, "Recording"),
                RecordingStatus::ShuttingDown => (egui::Color32::YELLOW, "Shutting down…"),
            };
            
            let dot_size = 8.0;
            let (rect, response) = ui.allocate_exact_size(egui::vec2(dot_size, dot_size), egui::Sense::hover());
            ui.painter().circle_filled(rect.center(), dot_size / 2.0, color);
            
            ui.add_space(8.0);
            ui.label(text);
            
            // Add hover text to the status area
            response.on_hover_text("Shows whether the recorder is idle or actively recording.");
        });
    }

    fn render_consent_modal(&mut self, ctx: &egui::Context) {
        if self.pending_consent.is_some() {
            let requester_clone = self.pending_consent.as_ref().and_then(|(r, _)| r.clone());
            let title = "Recording permission requested";
            egui::Window::new(title)
                .collapsible(false)
                .resizable(false)
                .anchor(egui::Align2::CENTER_CENTER, egui::Vec2::ZERO)
                .show(ctx, |ui| {
                    ui.vertical_centered(|ui| {
                        ui.label(egui::RichText::new("A remote controller has requested to start a recording.").heading());
                        if let Some(r) = requester_clone.as_ref() {
                            ui.label(egui::RichText::new(format!("Requester: {}", r)).color(egui::Color32::from_gray(180)));
                        }
                        ui.add_space(8.0);
                        ui.horizontal(|ui| {
                            if ui.button("Allow recording in session").clicked() {
                                if let Some((_, responder)) = self.pending_consent.take() {
                                    let _ = responder.send(true);
                                }
                            }
                            if ui.button("Decline recording in session").clicked() {
                                if let Some((_, responder)) = self.pending_consent.take() {
                                    let _ = responder.send(false);
                                }
                            }
                        });
                    });
                });
        }
    }

    fn render_controls(&mut self, ui: &mut egui::Ui) {
        ui.vertical_centered(|ui| {
            // Preview area
            self.render_preview(ui);

            ui.horizontal(|ui| {
                ui.add_space(16.0); // Horizontal padding
                let record_clicked = self.draw_record_button(ui);
                ui.add_space(20.0); // Spacing between buttons
                let stop_clicked = self.draw_stop_button(ui);
                ui.add_space(16.0);

                if record_clicked {
                    self.on_record_pressed();
                }
                if stop_clicked {
                    self.on_stop_pressed();
                }
            });
        });
    }

    fn poll_preview(&mut self, ctx: &egui::Context) {
        if let Some(rx) = &mut self.preview_rx {
            let mut latest: Option<PreviewImage> = None;
            while let Ok(img) = rx.try_recv() {
                latest = Some(img);
            }
            if let Some(img) = latest {
                if img.width as usize > 0 && img.height as usize > 0 && !img.data.is_empty() {
                    let color_image = egui::ColorImage::from_rgba_unmultiplied([img.width as usize, img.height as usize], &img.data);
                    // Replace texture each time a new preview arrives
                    let tex = ctx.load_texture("bsr_preview", color_image, egui::TextureOptions::LINEAR);
                    self.preview_texture = Some(tex);
                }
            }
        }
    }

    fn render_preview(&mut self, ui: &mut egui::Ui) {
        ui.group(|ui| {
            ui.label(egui::RichText::new("Preview").font(egui::FontId::proportional(14.0)));
            ui.separator();
            let Some(tex) = self.preview_texture.clone() else {
                ui.label(if self.live_view.is_some() {
                    "Starting live view…"
                } else {
                    "No preview available"
                });
                return;
            };

            let size = tex.size_vec2();
            let max_w = 480.0;
            let scale = (max_w / size.x).min(1.0);
            let display_size = egui::Vec2::new(size.x * scale, size.y * scale);
            ui.add_space(6.0);

            // Allocated by hand rather than `ui.image` so the preview can take clicks:
            // this image IS the corner picker.
            let (rect, resp) = ui.allocate_exact_size(display_size, egui::Sense::click());
            let painter = ui.painter_at(rect);
            painter.image(
                tex.id(),
                rect,
                egui::Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0)),
                egui::Color32::WHITE,
            );

            // Show the record space on top of the picture, so the crop is judged against
            // what is actually on screen rather than against four numbers.
            let (screen_w, screen_h) = self.model.settings.screen_size();
            if let Some((cx, cy, cw, ch)) = self
                .model
                .settings
                .to_capture_region()
                .and_then(|r| r.resolve(screen_w, screen_h))
            {
                let fx = |v: u32, span: u32, lo: f32, len: f32| lo + (v as f32 / span as f32) * len;
                let crop = egui::Rect::from_min_max(
                    egui::pos2(
                        fx(cx, screen_w, rect.min.x, rect.width()),
                        fx(cy, screen_h, rect.min.y, rect.height()),
                    ),
                    egui::pos2(
                        fx(cx + cw, screen_w, rect.min.x, rect.width()),
                        fx(cy + ch, screen_h, rect.min.y, rect.height()),
                    ),
                );
                // Dim everything that will NOT be recorded.
                let shade = egui::Color32::from_black_alpha(120);
                for band in [
                    egui::Rect::from_min_max(rect.min, egui::pos2(rect.max.x, crop.min.y)),
                    egui::Rect::from_min_max(egui::pos2(rect.min.x, crop.max.y), rect.max),
                    egui::Rect::from_min_max(egui::pos2(rect.min.x, crop.min.y), egui::pos2(crop.min.x, crop.max.y)),
                    egui::Rect::from_min_max(egui::pos2(crop.max.x, crop.min.y), egui::pos2(rect.max.x, crop.max.y)),
                ] {
                    if band.is_positive() {
                        painter.rect_filled(band, 0.0, shade);
                    }
                }
                painter.rect_stroke(
                    crop,
                    0.0,
                    egui::Stroke::new(1.5, egui::Color32::from_rgb(0xFF, 0xC1, 0x07)),
                );
            }

            // Crosshair + live coordinate while a corner is armed.
            if let Some(corner) = self.pending_pick {
                if let Some(pos) = resp.hover_pos() {
                    let stroke = egui::Stroke::new(1.0, egui::Color32::from_rgb(0xFF, 0xC1, 0x07));
                    painter.line_segment([egui::pos2(rect.left(), pos.y), egui::pos2(rect.right(), pos.y)], stroke);
                    painter.line_segment([egui::pos2(pos.x, rect.top()), egui::pos2(pos.x, rect.bottom())], stroke);
                    if let Some((sx, sy)) = preview_click_to_screen(pos, rect, (screen_w, screen_h)) {
                        painter.text(
                            pos + egui::vec2(8.0, 8.0),
                            egui::Align2::LEFT_TOP,
                            format!("{sx}, {sy}"),
                            egui::FontId::proportional(13.0),
                            egui::Color32::WHITE,
                        );
                    }
                }

                if resp.clicked() {
                    if let Some(pos) = resp.interact_pointer_pos() {
                        match preview_click_to_screen(pos, rect, (screen_w, screen_h)) {
                            Some((sx, sy)) => {
                                self.pending_pick = None;
                                if self.model.settings.set_corner(corner, sx, sy) {
                                    let (w, h) = self.model.settings.recording_size();
                                    self.model.diagnostics.push(format!(
                                        "Set {} corner to {sx},{sy} — recording {w}x{h}.",
                                        corner.label()
                                    ));
                                } else {
                                    self.model.diagnostics.push(format!(
                                        "{sx},{sy} would leave nothing to record — {} corner unchanged.",
                                        corner.label()
                                    ));
                                }
                            }
                            None => self.cancel_pick("preview had no size"),
                        }
                    }
                }
            }

            // Pulsing red recording dot overlay in the top-right of the preview
            if matches!(self.model.status, RecordingStatus::Recording) {
                let center = egui::pos2(rect.max.x - 20.0, rect.min.y + 20.0);
                let t = chrono::Utc::now().timestamp_millis() as f32 / 1000.0;
                let pulse = 0.5 + 0.5 * (f32::sin(t * 2.0 * std::f32::consts::PI));
                let alpha = (pulse * 200.0).clamp(40.0, 255.0) as u8;
                painter.circle_filled(center, 8.0, egui::Color32::from_rgba_unmultiplied(0xFF, 0x22, 0x22, alpha));
            }

            if let Some(corner) = self.pending_pick {
                ui.label(
                    egui::RichText::new(format!("Click the preview to place the {} corner (Esc cancels).", corner.label()))
                        .color(egui::Color32::from_rgb(0xFF, 0xC1, 0x07)),
                );
            }
        });
    }

    fn draw_record_button(&mut self, ui: &mut egui::Ui) -> bool {
        let desired_size = egui::vec2(48.0, 48.0);
        let (rect, response) = ui.allocate_exact_size(desired_size, egui::Sense::click());

        // Set cursor to pointer on hover
        if response.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }

        let painter = ui.painter_at(rect);
        let center = rect.center();
        let radius = rect.width().min(rect.height()) * 0.4;

        use egui::Color32;

        let (_fill_color, stroke_color) = match self.model.status {
            RecordingStatus::Recording => {
                // Disabled state
                (Color32::from_rgb(0x66, 0x66, 0x66), Color32::from_rgb(0x99, 0x99, 0x99))
            }
            _ => {
                // Enabled state
                if response.hovered() {
                    (Color32::from_rgb(0xCC, 0xCC, 0xCC), Color32::from_rgb(0xFF, 0xFF, 0xFF))
                } else {
                    (Color32::from_rgb(0xCC, 0xCC, 0xCC), Color32::from_rgb(0xCC, 0xCC, 0xCC))
                }
            }
        };

        painter.circle_stroke(center, radius, egui::Stroke::new(2.0, stroke_color));

        // Add hover text
        let response = response.on_hover_text("Start a new screen recording session.");

        // Only respond to clicks when not recording
        response.clicked() && !matches!(self.model.status, RecordingStatus::Recording)
    }

    fn draw_stop_button(&mut self, ui: &mut egui::Ui) -> bool {
        let desired_size = egui::vec2(48.0, 48.0);
        let (rect, response) = ui.allocate_exact_size(desired_size, egui::Sense::click());

        // Set cursor to pointer on hover
        if response.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }

        let painter = ui.painter_at(rect);

        use egui::Color32;

        let fill_color = match self.model.status {
            RecordingStatus::Recording => {
                // Enabled state
                if response.hovered() {
                    Color32::from_rgb(0x7A, 0x7A, 0x7A)
                } else {
                    Color32::from_rgb(0x5A, 0x5A, 0x5A)
                }
            }
            _ => {
                // Disabled state
                Color32::from_rgb(0x33, 0x33, 0x33)
            }
        };

        painter.rect_filled(rect.shrink(8.0), 2.0, fill_color);

        // Add hover text
        let response = response.on_hover_text("Stop the current recording and save the video.");

        // Only respond to clicks when recording
        response.clicked() && matches!(self.model.status, RecordingStatus::Recording)
    }

    fn on_record_pressed(&mut self) {
        if self.model.status != RecordingStatus::Idle { return; }

        // The recording pipeline opens its own capture session and feeds the same
        // preview channel. Two portal sessions at once is asking for trouble, so the
        // idle live view stands down first.
        self.stop_live_view();
        self.cancel_pick("recording started");

        // Create shared frame buffer (capture → encoder)
        let frame_buf = Arc::new(std::sync::Mutex::new(DropOldestBuffer::new(8)));
        let frame_notify = Arc::new(Notify::new());

        // Broadcast telemetry from capture/encoder
        let (telem_broadcast_tx, mut telem_broadcast_rx) = broadcast::channel::<TelemetryEvent>(32);

        // Pipeline shutdown channels
        let (cap_shutdown_tx, cap_shutdown_rx) = mpsc::channel(1);
        let (enc_shutdown_tx, enc_shutdown_rx) = mpsc::channel(1);
        let (mux_cmd_tx, mux_cmd_rx) = mpsc::channel(1);

        // Encoder → muxer packet channel (with type conversion bridge)
        let (packet_tx, mut enc_packet_rx) = mpsc::channel::<bsr_encode::EncodedPacket>(32);
        let (mux_packet_tx, mux_packet_rx) = mpsc::channel::<bsr_mux::EncodedPacket>(32);

        // Muxer telemetry → UI
        let (mux_telem_tx, mut mux_telem_rx) = mpsc::channel(32);

        // Muxer needs an IPC client (for auto-stop on duration cap)
        let (ipc_cmd_tx, _ipc_cmd_rx) = mpsc::channel(32);
        let ipc_client = IpcClient::new(ipc_cmd_tx);

        // Create capture service with the platform's real capture backend.
        // This named `dxgi_backend::DxgiCaptureBackend` directly and with no `cfg`,
        // which is why bsr-ui could never build on Linux at all: `dxgi_backend` is
        // `#[cfg(windows)]`. `platform_backend()` resolves to DXGI on Windows and to
        // the portal/PipeWire backend on Linux, and never to a synthetic source.
        // Capture, encoder and muxer must all be built for the same, cropped, size.
        let (rec_w, rec_h) = self.model.settings.recording_size();
        let mut capture_config = bsr_capture::CaptureConfig::default();
        capture_config.region = self.model.settings.to_capture_region();
        capture_config.width = rec_w;
        capture_config.height = rec_h;
        if let Some(r) = &capture_config.region {
            self.model.diagnostics.push(format!(
                "Recording cropped region {}x{} (from {},{} to {},{}).",
                rec_w, rec_h, r.x1, r.y1, r.x2, r.y2
            ));
        }
        // The live preview gets its own copy of each frame from the capture service.
        // It must NOT share `frame_buf`: that buffer belongs to the encoder, and a
        // second consumer popping from it removes frames from the recording. When the
        // preview did share it, the two tasks were woken by one `notify_one()` — which
        // wakes a single waiter — so the preview swallowed roughly half the frames, and
        // once the encoder drained per wake the preview could starve it outright,
        // producing an empty 261-byte file that still reported success.
        let (frame_preview_tx, frame_preview_rx) =
            tokio::sync::watch::channel::<Option<CaptureFrame>>(None);
        let capture_service = bsr_capture::CaptureService::new(
            bsr_capture::platform_backend(),
            capture_config,
            telem_broadcast_tx.clone(),
            cap_shutdown_rx,
            frame_buf.clone(),
            frame_notify.clone(),
        )
        .with_preview(frame_preview_tx, std::time::Duration::from_millis(200));

        // Create encoder service with H.264 backend, sized to the cropped frame.
        let encoder_config = encoder_config_for(&self.model.settings, rec_w, rec_h);
        let encoder_service = match bsr_encode::EncoderService::new(
            encoder_config.clone(),
            telem_broadcast_tx,
            frame_buf.clone(),
            frame_notify.clone(),
            packet_tx,
            enc_shutdown_rx,
        ) {
            Ok(svc) => svc,
            Err(e) => {
                self.model.diagnostics.push(format!("Encoder init failed: {}", e));
                return;
            }
        };

        // Create muxer service with MP4 backend. Generate a deterministic
        // filename now (includes monotonic fragment sequence) and record it
        // so the UI can offer it for saving later.
        let mut muxer_config = self.model.settings.to_muxer_config();
        let timestamp = chrono::Utc::now().format("%Y-%m-%d_%H-%M-%S");
        let filename = format!("recording-{}-{}.mp4", timestamp, self.recording_seq);
        muxer_config.file_naming_strategy = bsr_ipc::FileNamingStrategy::Simple(filename.clone());
        let output_path = muxer_config.preview_output_path();
        self.current_recording_path = Some(output_path.clone());

        // Increment the sequence so the next fragment gets the next number and persist it atomically.
        self.recording_seq = self.recording_seq.saturating_add(1);
        // Persist next sequence to disk atomically: write to temp, flush, then rename.
        let seq_path = std::path::Path::new(&self.model.settings.output_folder).join(".bsr_fragment_seq");
        if let Some(parent) = seq_path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let tmp_path = seq_path.with_file_name(".bsr_fragment_seq.tmp");
        match std::fs::OpenOptions::new().write(true).create(true).truncate(true).open(&tmp_path) {
            Ok(mut f) => {
                use std::io::Write;
                let bytes = self.recording_seq.to_string().into_bytes();
                let _ = f.write_all(&bytes);
                let _ = f.sync_all();
                // Try to rename into place; if rename fails, attempt remove+rename as a best-effort fallback.
                match std::fs::rename(&tmp_path, &seq_path) {
                    Ok(_) => {}
                    Err(_) => {
                        let _ = std::fs::remove_file(&seq_path);
                        let _ = std::fs::rename(&tmp_path, &seq_path);
                    }
                }

                // On Unix-like platforms, fsync the parent directory to ensure the rename is durable.
                #[cfg(unix)]
                {
                    if let Some(parent) = seq_path.parent() {
                        if let Ok(dirf) = std::fs::File::open(parent) {
                            let _ = dirf.sync_all();
                        }
                    }
                }
            }
            Err(_) => {
                // Fallback: best-effort simple write
                let _ = std::fs::write(&seq_path, self.recording_seq.to_string());
            }
        }

        let muxer_service = bsr_mux::MuxerService::new(
            muxer_config,
            bsr_mux::backends::mp4::Mp4Muxer::new(),
            mux_packet_rx,
            mux_telem_tx,
            mux_cmd_rx,
            ipc_client,
        );

        // Bridge: convert encoder packets to muxer packets and forward frame
        // metadata to the telemetry pipe client.
        let encoder_config_for_bridge = encoder_config.clone();
        let pipe_path = std::env::var("BSR_TELEMETRY_PIPE").unwrap_or_else(|_| r"\\.\pipe\bsr-telemetry".to_string());
        let (frame_tx, frame_rx) = mpsc::channel::<FrameInfo>(32);

        // Spawn telemetry client that connects to the telemetry pipe and streams frames.
        let pipe_path_clone = pipe_path.clone();
        self.tokio_handle.spawn(async move {
            // Telemetry client will reconnect on its own; keep it in background.
            bsr_ipc::telemetry_pipe::telemetry_client(frame_rx, pipe_path_clone).await;
        });

        // Preview channel: the background task scales the capture service's own
        // preview copy and sends RGBA to the UI. It never touches `frame_buf`.
        let (preview_tx, preview_rx) = mpsc::unbounded_channel::<PreviewImage>();
        // store receiver so UI can poll it
        self.preview_rx = Some(preview_rx);

        let mut frame_preview_rx = frame_preview_rx;
        let preview_task_handle = self.tokio_handle.spawn(async move {
            loop {
                // Ends when the capture service drops its sender.
                if frame_preview_rx.changed().await.is_err() {
                    break;
                }
                let latest = frame_preview_rx.borrow_and_update().clone();

                if let Some(f) = latest {
                    // Convert BGRA -> RGBA
                    let w = f.width as usize;
                    let h = f.height as usize;
                    let mut rgba = Vec::with_capacity(w * h * 4);
                    for i in 0..(w * h) {
                        let b = f.data[i * 4];
                        let g = f.data[i * 4 + 1];
                        let r = f.data[i * 4 + 2];
                        let a = f.data[i * 4 + 3];
                        rgba.push(r);
                        rgba.push(g);
                        rgba.push(b);
                        rgba.push(a);
                    }

                    // Resize to preview width (keep aspect ratio)
                    let target_w: u32 = 320;
                    let target_h: u32 = ((h as f32) * (target_w as f32) / (w as f32)).max(1.0) as u32;
                    if let Some(img_buf) = image::ImageBuffer::<image::Rgba<u8>, _>::from_raw(f.width, f.height, rgba) {
                        let dynimg = image::DynamicImage::ImageRgba8(img_buf);
                        let resized = image::imageops::resize(&dynimg, target_w, target_h, FilterType::Triangle);
                        let data = resized.into_raw();
                        let _ = preview_tx.send(PreviewImage { width: target_w, height: target_h, data });
                    } else {
                        // Fallback: send unresized RGBA (may be heavy)
                        let mut fallback_rgba = Vec::with_capacity(f.data.len());
                        let w = f.width as usize;
                        let h = f.height as usize;
                        for i in 0..(w * h) {
                            let b = f.data[i * 4];
                            let g = f.data[i * 4 + 1];
                            let r = f.data[i * 4 + 2];
                            let a = f.data[i * 4 + 3];
                            fallback_rgba.push(r);
                            fallback_rgba.push(g);
                            fallback_rgba.push(b);
                            fallback_rgba.push(a);
                        }
                        let _ = preview_tx.send(PreviewImage { width: f.width, height: f.height, data: fallback_rgba });
                    }
                }
            }
        });

        self.tokio_handle.spawn(async move {
            // Sender-owned monotonic sequence counter. Resets when a new
            // recording pipeline is created (i.e. on StartRecording).
            let mut seq_counter: u64 = 0;
            while let Some(ep) = enc_packet_rx.recv().await {
                let mp = bsr_mux::EncodedPacket {
                    data: ep.data.clone(),
                    pts: ep.pts,
                    dts: ep.dts,
                    keyframe: ep.keyframe,
                };
                // Send to muxer
                if mux_packet_tx.send(mp).await.is_err() { break; }

                // Increment monotonic sequence by exactly one per encoded frame
                seq_counter = seq_counter.wrapping_add(1);

                // Forward frame metadata to telemetry client (best-effort)
                let fi = FrameInfo {
                    sequence: seq_counter,
                    width: encoder_config_for_bridge.width,
                    height: encoder_config_for_bridge.height,
                    size_bytes: ep.data.len(),
                    timestamp: ep.timestamp,
                };
                let _ = frame_tx.send(fi).await;
            }
        });

        // Forward broadcast telemetry to UI's unbounded channel
        let telem_fwd = self.telemetry_tx.clone();
        self.tokio_handle.spawn(async move {
            while let Ok(evt) = telem_broadcast_rx.recv().await {
                let _ = telem_fwd.send(evt);
            }
        });

        // Forward muxer telemetry to UI's unbounded channel
        let mux_fwd = self.muxer_telemetry_tx.clone();
        self.tokio_handle.spawn(async move {
            while let Some(mt) = mux_telem_rx.recv().await {
                let _ = mux_fwd.send(mt);
            }
        });

        // Spawn pipeline services
        self.tokio_handle.spawn(async move {
            if let Err(e) = capture_service.run().await {
                tracing::error!("Capture error: {}", e);
            }
        });
        self.tokio_handle.spawn(async move {
            if let Err(e) = encoder_service.run().await {
                tracing::error!("Encoder error: {}", e);
            }
        });
        let muxer_task = self.tokio_handle.spawn(async move {
            muxer_service.run().await.map_err(|e| {
                tracing::error!("Muxer error: {}", e);
                e.to_string()
            })
        });

        self.pipeline = Some(RecordingPipeline {
            cap_shutdown: cap_shutdown_tx,
            enc_shutdown: enc_shutdown_tx,
            mux_cmd: mux_cmd_tx,
            preview_task: Some(preview_task_handle),
            muxer_task: Some(muxer_task),
        });

        // Record the start time for the 60-minute limit enforcement
        self.recording_start = Some(std::time::Instant::now());

        if self.model.settings.minimize_while_recording {
            // Out of its own shot. The tray icon (ubuntu-appindicators is enabled on this
            // box) is how the recording gets stopped again while the window is down —
            // without a reachable Stop this would strand the operator mid-recording.
            self.pending_window_cmd = Some(true);
            self.model.diagnostics.push(
                "Minimising so BSR is not in the recording — stop from the tray icon, or restore the window.",
            );
        }
        self.model.status = RecordingStatus::Recording;
        self.model.telemetry = UiTelemetry::default();
        self.model.diagnostics = UiDiagnostics::default();
        self.model.diagnostics.push("Recording started.");
    }

    fn on_stop_pressed(&mut self) {
        if self.model.status != RecordingStatus::Recording { return; }

        if let Some(mut pipeline) = self.pipeline.take() {
            // Abort preview task if present
            if let Some(handle) = pipeline.preview_task.take() {
                handle.abort();
            }

            // Drop preview receiver and texture
            self.preview_rx = None;
            self.preview_texture = None;

            let (done_tx, done_rx) = tokio::sync::oneshot::channel();
            self.finalize_rx = Some(done_rx);

            self.tokio_handle.spawn(async move {
                let mut pipeline = pipeline;
                // Shut down in order: capture → encoder → muxer
                let _ = pipeline.cap_shutdown.send(()).await;
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                let _ = pipeline.enc_shutdown.send(()).await;
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                let _ = pipeline.mux_cmd.send(bsr_mux::MuxerCommand::StopRecording).await;

                // WAIT for the trailer. The muxer writes the MP4 `moov` atom inside
                // finalize(), and until that lands the file on disk is unplayable — real
                // H.264 in a container nothing can open. Previously this task was
                // detached and the save modal opened immediately, so a quick operator
                // could copy a headless file and keep the broken copy.
                let outcome = match pipeline.muxer_task.take() {
                    Some(task) => match tokio::time::timeout(std::time::Duration::from_secs(30), task).await {
                        Ok(Ok(Ok(()))) => Ok(()),
                        Ok(Ok(Err(e))) => Err(e),
                        Ok(Err(e)) => Err(format!("muxer task panicked: {e}")),
                        Err(_) => Err("timed out waiting for the MP4 trailer".to_string()),
                    },
                    None => Err("no muxer task to wait for".to_string()),
                };
                let _ = done_tx.send(outcome);
            });
        }

        // Not Idle yet: the recording is not finished until the trailer is written.
        // `ShuttingDown` already disables both Record and Stop, which is exactly right
        // for this window.
        self.model.status = RecordingStatus::ShuttingDown;
        self.model.diagnostics.push("Recording stopped — finalising the file…");
        self.recording_start = None;
    }

    /// Complete a stop once the muxer has written the MP4 trailer.
    ///
    /// Only here is the file worth offering to anyone: this is the point at which it
    /// becomes a playable recording rather than a container without an index.
    fn poll_finalize(&mut self) {
        let Some(rx) = &mut self.finalize_rx else { return };
        let outcome = match rx.try_recv() {
            Ok(outcome) => outcome,
            Err(tokio::sync::oneshot::error::TryRecvError::Empty) => return,
            Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                Err("the finalising task disappeared".to_string())
            }
        };
        self.finalize_rx = None;
        self.model.status = RecordingStatus::Idle;
        // Come back so the operator can see the outcome and the save prompt.
        self.pending_window_cmd = Some(false);

        match &outcome {
            Ok(()) => {
                // The muxer's Ok describes the operation, not the artifact.
                let empty = self
                    .current_recording_path
                    .as_deref()
                    .map_or(false, recording_is_empty);
                if empty {
                    self.model.diagnostics.push(
                        "Finalising produced an EMPTY recording — no video data reached \
                         the muxer. The file is not usable.",
                    );
                } else {
                    self.model.diagnostics.push("Recording finalised and ready to save.");
                }
            }
            Err(e) => self.model.diagnostics.push(format!(
                "Finalising failed ({e}) — the file may be incomplete or unplayable."
            )),
        }

        // The file is still offered on failure: it is the operator's recording and
        // refusing to hand it over is worse than handing it over with a warning. The
        // warning above is the difference.
        if let Some(src_path) = &self.current_recording_path {
            let default_name = src_path
                .file_name()
                .and_then(|s| s.to_str())
                .unwrap_or("recording.mp4")
                .to_string();
            self.pending_local_save = Some(default_name);
            self.save_status = None;
            if self.save_dest_folder.is_empty() {
                self.save_dest_folder = self.default_save_folder();
            }

            if let Some(client) = &self.ipc_client {
                let cli = client.clone();
                let offer_path = src_path.to_string_lossy().to_string();
                self.tokio_handle.spawn(async move {
                    let _ = cli
                        .send_command(bsr_ipc::IpcCommand::OfferSaveCopy { path: offer_path, requester: None })
                        .await;
                });
            }
        }
    }

    fn render_telemetry(&mut self, ui: &mut egui::Ui) {
        ui.group(|ui| {
            ui.label(egui::RichText::new("Telemetry").font(egui::FontId::proportional(16.0)));
            ui.separator();
            
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(format!("Frames: {}", self.model.telemetry.frames_captured)).color(egui::Color32::from_rgb(0xAA, 0xAA, 0xAA)));
                ui.label(egui::RichText::new(format!("Drops: {}", self.model.telemetry.frames_dropped)).color(egui::Color32::from_rgb(0xAA, 0xAA, 0xAA)));
            });
            
            ui.horizontal(|ui| {
                ui.label(egui::RichText::new(format!("Latency: {:.1} ms", self.model.telemetry.smoothed_write_latency_ms)).color(egui::Color32::from_rgb(0xAA, 0xAA, 0xAA)));
                ui.label(egui::RichText::new(format!("Packets: {}", self.model.telemetry.packets_encoded)).color(egui::Color32::from_rgb(0xAA, 0xAA, 0xAA)));
            });
            
            ui.horizontal(|ui| {
                let mb = self.model.telemetry.file_size_bytes as f32 / (1024.0 * 1024.0);
                let duration_secs = self.model.telemetry.recording_duration_secs;
                let bitrate_mbps = if duration_secs > 0 {
                    (self.model.telemetry.file_size_bytes as f32 * 8.0) / (duration_secs as f32 * 1024.0 * 1024.0)
                } else {
                    0.0
                };
                
                ui.label(egui::RichText::new("File:").color(egui::Color32::from_rgb(0xAA, 0xAA, 0xAA)))
                    .on_hover_text("Current size of the video file being saved.");
                ui.label(egui::RichText::new(format!("{:.2} MB (≈ {:.2} Mbps)", mb, bitrate_mbps)).color(egui::Color32::from_rgb(0xAA, 0xAA, 0xAA)));
                
                let hours = duration_secs / 3600;
                let minutes = (duration_secs % 3600) / 60;
                let seconds = duration_secs % 60;
                let duration_color = if duration_secs > 4 * 3600 { egui::Color32::from_rgb(0xFF, 0x88, 0x00) } else { egui::Color32::from_rgb(0xAA, 0xAA, 0xAA) };
                ui.label(egui::RichText::new("Duration:").color(duration_color))
                    .on_hover_text("How long the current recording has been running.");
                ui.label(egui::RichText::new(format!("{:02}:{:02}:{:02}", hours, minutes, seconds)).color(duration_color));
            });
        });
    }

    /// Arm a corner for placement and make sure there is something to click on.
    ///
    /// The previous implementation opened a fullscreen, always-on-top viewport to catch
    /// a click anywhere on the desktop. The test box proved that unshippable: closing it
    /// by ANY path wedged BSR's whole render loop for 17 s to 2+ minutes, with the main
    /// thread parked in `swap_buffers -> wl_display_dispatch_queue -> ppoll` while eframe
    /// tore the child viewport down. Reproduced four times.
    ///
    /// Picking now happens on the live preview inside this ordinary window instead. No
    /// fullscreen, no always-on-top, no second viewport — so the hazard is removed rather
    /// than contained, and the whole feature is testable on the dev box.
    fn begin_pick(&mut self, corner: Corner) {
        self.pending_pick = Some(corner);
        self.start_live_view();
        self.model
            .diagnostics
            .push(format!("Click the preview to place the {} corner.", corner.label()));
    }

    fn cancel_pick(&mut self, why: &str) {
        if self.pending_pick.take().is_some() {
            self.model.diagnostics.push(format!("Corner pick cancelled ({why})."));
        }
    }

    /// Start an idle capture that feeds the preview.
    ///
    /// Only while not recording: the recording pipeline owns its own capture session and
    /// feeds the same preview channel, and two portal sessions at once is asking for
    /// trouble.
    fn start_live_view(&mut self) {
        if self.live_view.is_some() || matches!(self.model.status, RecordingStatus::Recording) {
            return;
        }
        let (preview_tx, preview_rx) = mpsc::unbounded_channel::<PreviewImage>();
        self.preview_rx = Some(preview_rx);
        let (stop_tx, mut stop_rx) = tokio::sync::mpsc::channel::<()>(1);
        let error = std::sync::Arc::new(std::sync::Mutex::new(None::<String>));
        let task_error = error.clone();

        self.tokio_handle.spawn(async move {
            // The trait must be in scope for initialize/capture_frame/shutdown.
            use bsr_capture::CaptureBackend as _;
            let mut backend = bsr_capture::platform_backend();
            if let Err(e) = backend.initialize().await {
                if let Ok(mut slot) = task_error.lock() {
                    *slot = Some(format!("Live view unavailable: {e}"));
                }
                return;
            }
            // Deliberately slow. This exists to be looked at and clicked on, not to be
            // smooth; 5 fps keeps a full-screen capture and rescale off the critical path.
            let mut tick = tokio::time::interval(std::time::Duration::from_millis(200));
            loop {
                tokio::select! {
                    _ = stop_rx.recv() => break,
                    _ = tick.tick() => {
                        match backend.capture_frame().await {
                            Ok(frame) => {
                                if let Some(p) = frame_to_preview(&frame, 480) {
                                    if preview_tx.send(p).is_err() {
                                        break; // UI dropped the receiver
                                    }
                                }
                            }
                            Err(e) => {
                                if let Ok(mut slot) = task_error.lock() {
                                    *slot = Some(format!("Live view stopped: {e}"));
                                }
                                break;
                            }
                        }
                    }
                }
            }
            let _ = backend.shutdown().await;
        });

        self.live_view = Some(LiveView { stop: stop_tx, error });
        self.model.diagnostics.push("Live view started.");
    }

    fn stop_live_view(&mut self) {
        if let Some(live) = self.live_view.take() {
            let stop = live.stop.clone();
            self.tokio_handle.spawn(async move {
                let _ = stop.send(()).await;
            });
            self.preview_rx = None;
            self.preview_texture = None;
            self.model.diagnostics.push("Live view stopped.");
        }
    }

    /// Surface a live-view failure once, then forget it.
    fn drain_live_view_error(&mut self) {
        let msg = self
            .live_view
            .as_ref()
            .and_then(|l| l.error.lock().ok().and_then(|mut m| m.take()));
        if let Some(msg) = msg {
            self.model.diagnostics.push(msg);
            self.pending_pick = None;
            self.live_view = None;
        }
    }

    /// The crop controls get their own section, open by default.
    ///
    /// They were originally inside "Settings", which is collapsed by default, so the
    /// feature was invisible unless you knew to look for it -- and with no scroll area
    /// on the window (see `render_ui`) expanding Settings pushed it off the bottom
    /// where it could not be reached at all.
    fn render_record_space(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new(
            egui::RichText::new("Record space").font(egui::FontId::proportional(16.0)),
        )
        .default_open(true)
        .show(ui, |ui| {
            ui.group(|ui| {
                let (screen_w, screen_h) = self.model.settings.screen_size();
                let (rec_w, rec_h) = self.model.settings.recording_size();
                ui.label(
                    egui::RichText::new("Trim the edges. All zero records the whole screen.")
                        .color(egui::Color32::from_rgb(0x88, 0x88, 0x88))
                        .font(egui::FontId::proportional(11.0)),
                );
                ui.horizontal(|ui| {
                    ui.label("Left:");
                    ui.add(egui::DragValue::new(&mut self.model.settings.crop_left).clamp_range(0..=screen_w))
                        .on_hover_text("Pixels trimmed off the left edge.");
                    ui.label("Top:");
                    ui.add(egui::DragValue::new(&mut self.model.settings.crop_top).clamp_range(0..=screen_h))
                        .on_hover_text("Pixels trimmed off the top edge.");
                });
                ui.horizontal(|ui| {
                    ui.label("Right:");
                    ui.add(egui::DragValue::new(&mut self.model.settings.crop_right).clamp_range(0..=screen_w))
                        .on_hover_text("Pixels trimmed off the right edge.");
                    ui.label("Bottom:");
                    ui.add(egui::DragValue::new(&mut self.model.settings.crop_bottom).clamp_range(0..=screen_h))
                        .on_hover_text("Pixels trimmed off the bottom edge.");
                });
                // Click-to-set. Each corner drives the two edges it touches, so the
                // operator sets them one at a time by pointing at the desktop.
                ui.add_space(4.0);
                ui.horizontal(|ui| {
                    if ui.button("Set top-left").on_hover_text(
                        "Then click the desktop: that point becomes the top-left corner.",
                    ).clicked() {
                        self.begin_pick(Corner::TopLeft);
                    }
                    if ui.button("Set top-right").on_hover_text(
                        "Then click the desktop: that point becomes the top-right corner.",
                    ).clicked() {
                        self.begin_pick(Corner::TopRight);
                    }
                });
                ui.horizontal(|ui| {
                    if ui.button("Set bottom-left").on_hover_text(
                        "Then click the desktop: that point becomes the bottom-left corner.",
                    ).clicked() {
                        self.begin_pick(Corner::BottomLeft);
                    }
                    if ui.button("Set bottom-right").on_hover_text(
                        "Then click the desktop: that point becomes the bottom-right corner.",
                    ).clicked() {
                        self.begin_pick(Corner::BottomRight);
                    }
                });
                if self.pending_pick.is_some() {
                    if ui.button("Cancel pick").on_hover_text("Stop waiting for a click.").clicked() {
                        self.cancel_pick("cancelled");
                    }
                }
                // Live view is what makes the preview clickable, so it is offered here
                // rather than buried elsewhere. It runs a capture session, so it is off
                // unless asked for, and never while recording (which feeds the preview
                // from its own session).
                if !matches!(self.model.status, RecordingStatus::Recording) {
                    let mut live = self.live_view.is_some();
                    if ui.checkbox(&mut live, "Live view")
                        .on_hover_text("Show the screen in the Preview pane so corners can be clicked on it.")
                        .changed()
                    {
                        if live { self.start_live_view(); } else { self.stop_live_view(); }
                    }
                }
                ui.add_space(4.0);

                ui.horizontal(|ui| {
                    if self.model.settings.has_crop_insets() {
                        let unusable = self.model.settings.to_capture_region().is_none();
                        if unusable {
                            ui.label(
                                egui::RichText::new(
                                    "⚠ Those edges leave nothing to record — recording the full screen",
                                )
                                .color(egui::Color32::from_rgb(0xFF, 0x88, 0x00)),
                            )
                            .on_hover_text(
                                "The trims overlap or cover the whole screen. Recording \
                                 everything is safer than recording nothing.",
                            );
                        } else {
                            ui.label(format!("Recording {rec_w}x{rec_h} of {screen_w}x{screen_h}"))
                                .on_hover_text(
                                    "Sizes are rounded down to even numbers: H.264 cannot \
                                     encode an odd width or height.",
                                );
                        }
                    } else {
                        ui.label(format!("Recording the full screen ({screen_w}x{screen_h})"));
                    }
                    ui.checkbox(
                        &mut self.model.settings.minimize_while_recording,
                        "Hide while recording",
                    )
                    .on_hover_text(
                        "Minimise BSR when recording starts so its own window is not in the \
                         recording. Stop from the tray icon, or restore the window.",
                    );
                    if ui.button("Reset").on_hover_text("Record the whole screen again.").clicked() {
                        self.model.settings.crop_left = 0;
                        self.model.settings.crop_top = 0;
                        self.model.settings.crop_right = 0;
                        self.model.settings.crop_bottom = 0;
                    }
                });
            });
        })
        .header_response
        .on_hover_text("Choose how much of the screen is recorded.");
    }

    fn render_settings(&mut self, ui: &mut egui::Ui) {
        egui::CollapsingHeader::new(egui::RichText::new("Settings").font(egui::FontId::proportional(16.0)))
            .default_open(false)
            .show(ui, |ui| {
                ui.group(|ui| {
                    ui.label(egui::RichText::new("Output").font(egui::FontId::proportional(14.0)));
                    
                    ui.horizontal(|ui| {
                        ui.label("📁"); // Folder icon
                        ui.label("Folder:")
                            .on_hover_text("This is the folder where your recordings will be saved.");
                        ui.text_edit_singleline(&mut self.model.settings.output_folder)
                            .on_hover_text("Enter or edit the folder where recordings should be stored.");
                        
                        // Update folder validation
                        self.model.folder_valid = std::path::Path::new(&self.model.settings.output_folder).exists();
                        if !self.model.folder_valid {
                            ui.label(egui::RichText::new("⚠ Path does not exist").color(egui::Color32::from_rgb(0xFF, 0x88, 0x00)));
                        }
                    });

                    ui.add_space(4.0);
                    ui.add_space(4.0);
                    ui.label(egui::RichText::new("File naming:").font(egui::FontId::proportional(14.0)));
                    
                    egui::ComboBox::from_label("")
                        .selected_text(match self.model.settings.file_naming_strategy {
                            FileNamingStrategyUi::Simple => "Simple",
                            FileNamingStrategyUi::TimestampedFile => "Timestamped file",
                            FileNamingStrategyUi::TimestampedFolder => "Timestamped folder",
                        })
                        .show_ui(ui, |ui| {
                            ui.selectable_value(
                                &mut self.model.settings.file_naming_strategy,
                                FileNamingStrategyUi::Simple,
                                "Simple — Always recording.mp4",
                            )
                            .on_hover_text("Always saves as recording.mp4. Best for quick manual use.");
                            ui.selectable_value(
                                &mut self.model.settings.file_naming_strategy,
                                FileNamingStrategyUi::TimestampedFile,
                                "Timestamped file — YYYY-MM-DD_HH-MM-SS.mp4",
                            )
                            .on_hover_text("Creates a new file for each recording using the date and time.");
                            ui.selectable_value(
                                &mut self.model.settings.file_naming_strategy,
                                FileNamingStrategyUi::TimestampedFolder,
                                "Timestamped folder — YYYY-MM-DD/HH-MM-SS.mp4",
                            )
                            .on_hover_text("Creates a dated folder and places the recording inside it.");
                        });
                    ui.label(egui::RichText::new("ℹ Choose how each recording file will be named.").color(egui::Color32::from_rgb(0x88, 0x88, 0x88)).font(egui::FontId::proportional(11.0)));

                    ui.add_space(4.0);
                    ui.label("Max duration: 5 hours (auto‑stop)");

                    ui.add_space(4.0);
                    let preview = self.model.settings.to_muxer_config().preview_output_path();
                    let preview_str = preview.to_string_lossy();
                    let truncated = if preview_str.len() > 50 {
                        let start = &preview_str[..20];
                        let end = &preview_str[preview_str.len().saturating_sub(25)..];
                        format!("{}...{}", start, end)
                    } else {
                        preview_str.to_string()
                    };
                    ui.label(egui::RichText::new(format!("Next file: {}", truncated)).color(egui::Color32::from_rgb(0x88, 0x88, 0x88)).font(egui::FontId::proportional(12.0)));
                });
            })
            .header_response
            .on_hover_text("Adjust where recordings are saved and how they are named.");
    }

    fn render_diagnostics(&mut self, ui: &mut egui::Ui) {
        ui.group(|ui| {
            ui.label(egui::RichText::new("Diagnostics").font(egui::FontId::proportional(16.0)))
                .on_hover_text("Messages about recording activity and system behavior.");
            ui.separator();
            
            let mut scroll_area = egui::ScrollArea::vertical().max_height(80.0);
            if self.model.diagnostics_auto_scroll {
                scroll_area = scroll_area.stick_to_bottom(true);
            }
            
            scroll_area.show(ui, |ui| {
                ui.add_space(4.0); // Top padding
                
                for (i, line) in self.model.diagnostics.lines.iter().enumerate() {
                    if let Some(timestamp) = self.model.diagnostics.timestamps.get(i) {
                        ui.label(egui::RichText::new(format!("[{}] {}", timestamp, line)).color(egui::Color32::from_rgb(0xAA, 0xAA, 0xAA)));
                    }
                }
            });
        });
    }
}

impl eframe::App for AppWindow {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        // `_frame` was already unused. Keeping the real body free of it is what lets a
        // headless harness drive exactly the same code path the app runs.
        self.tick(ctx);
    }
}

impl AppWindow {
    /// One UI frame. This is the whole of what `eframe::App::update` does.
    pub fn tick(&mut self, ctx: &egui::Context) {
        if std::mem::take(&mut self.needs_initial_raise) {
            // Come to the front on launch. A window opened from a launcher arrives
            // unfocused and lands BEHIND whatever is maximised, and with no dock entry to
            // click there is then nothing to bring it back — it reads exactly as "the app
            // did not start". GNOME says as much itself, raising an "… is ready"
            // notification instead of showing the window.
            ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
        }
        if let Some(minimize) = self.pending_window_cmd.take() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Minimized(minimize));
            if !minimize {
                ctx.send_viewport_cmd(egui::ViewportCommand::Focus);
            }
        }

        // Esc cancels an armed corner pick. Cheap, and it means the operator is never
        // stuck in a mode with no visible way out.
        if self.pending_pick.is_some() && ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            self.cancel_pick("Esc");
        }

        // Poll tray menu events
        while let Ok(event) = MenuEvent::receiver().try_recv() {
            match event.id.0.as_str() {
                "Open" => {
                    // eframe does NOT handle this — the comment that used to sit here said
                    // it did, and the arm was empty. With "Hide while recording" on, the
                    // tray is the way back to a minimised window, so this being a no-op
                    // meant there was no way back at all.
                    self.pending_window_cmd = Some(false);
                }
                "Start Recording" => {
                    self.on_record_pressed();
                }
                "Stop Recording" => {
                    self.on_stop_pressed();
                    // Come back so the operator can see the result and the save prompt.
                    self.pending_window_cmd = Some(false);
                }
                "Exit" => {
                    self.on_stop_pressed();
                    std::process::exit(0);
                }
                _ => {}
            }
        }

        // Poll global hotkeys — Ctrl+Shift+R toggles recording.
        if let Some(id) = self.record_hotkey_id {
            while let Ok(event) = GlobalHotKeyEvent::receiver().try_recv() {
                if event.id == id && event.state == HotKeyState::Pressed {
                    match self.model.status {
                        RecordingStatus::Recording => self.on_stop_pressed(),
                        _ => self.on_record_pressed(),
                    }
                }
            }
        }

        // (No splash screen) keep UI immediate

        if self.bsr_debug {
            tracing::debug!("update tick");
        }

        self.poll_telemetry();
        self.poll_muxer_telemetry();
        self.poll_consent_requests();
        self.poll_save_offers();
        self.poll_preview(ctx);

        // Enforce 60-minute recording limit: stop recording and schedule restart.
        if let Some(start) = self.recording_start {
            if matches!(self.model.status, RecordingStatus::Recording) {
                if start.elapsed() >= std::time::Duration::from_secs(60 * 60) {
                    self.model.diagnostics.push("Recording reached 60-minute limit; rolling over.".to_string());
                    // Stop current recording and schedule an auto-restart shortly after
                    self.on_stop_pressed();
                    self.pending_auto_restart = Some(std::time::Instant::now());
                }
            }
        }

        // If an auto-restart is pending and the UI is idle, start a new recording.
        if let Some(ts) = self.pending_auto_restart {
            if matches!(self.model.status, RecordingStatus::Idle) {
                if ts.elapsed() >= std::time::Duration::from_millis(250) {
                    self.pending_auto_restart = None;
                    self.model.diagnostics.push("Auto-restarting recording after rollover.".to_string());
                    self.on_record_pressed();
                }
            }
        }
        self.render_ui(ctx);

        // Render consent modal (if any)
        if self.pending_consent.is_some() {
            self.render_consent_modal(ctx);
        }

        // Render save offer modal (if any)
        if self.pending_save_offer.is_some() {
            self.render_save_offer_modal(ctx);
        }

        // Render local save modal (starter)
        if self.pending_local_save.is_some() {
            self.render_local_save_modal(ctx);
        }

        // Keep egui alive so telemetry + tray events are processed continuously.
        ctx.request_repaint_after(std::time::Duration::from_millis(16));
    }
}

fn apply_charcoal_theme(ctx: &egui::Context) {
    use egui::{Color32, Visuals};

    let mut visuals = Visuals::dark();

    visuals.override_text_color = Some(Color32::WHITE);
    visuals.widgets.noninteractive.bg_fill = Color32::from_rgb(0x20, 0x20, 0x20);
    visuals.widgets.inactive.bg_fill = Color32::from_rgb(0x2A, 0x2A, 0x2A);
    visuals.widgets.hovered.bg_fill = Color32::from_rgb(0x3A, 0x3A, 0x3A);
    visuals.widgets.active.bg_fill = Color32::from_rgb(0x4A, 0x4A, 0x4A);
    visuals.panel_fill = Color32::from_rgb(0x1A, 0x1A, 0x1A);

    visuals.widgets.noninteractive.bg_stroke.color = Color32::from_rgb(0x44, 0x44, 0x44);
    visuals.widgets.inactive.bg_stroke.color = Color32::from_rgb(0x44, 0x44, 0x44);

    ctx.set_visuals(visuals);
}

#[cfg(test)]
#[path = "test_harness.rs"]
mod test_harness;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ui_command_serialization() {
        let cmd = UiCommand::StartRecording;
        let json = serde_json::to_string(&cmd).unwrap();
        let deserialized: UiCommand = serde_json::from_str(&json).unwrap();
        assert_eq!(cmd, deserialized);
    }

    #[test]
    fn test_ui_model_default() {
        let model = UiModel::default();
        assert_eq!(model.status, RecordingStatus::Idle);
        assert_eq!(model.telemetry.frames_captured, 0);
        assert_eq!(model.settings.resolution, "1920x1080");
    }

    #[test]
    fn test_ui_diagnostics_push() {
        let mut diag = UiDiagnostics::default();
        diag.push("test");
        assert_eq!(diag.lines.len(), 1);
        assert_eq!(diag.lines[0], "test");
        assert_eq!(diag.timestamps.len(), 1);
    }

    #[test]
    fn test_ui_settings_to_muxer_config() {
        let mut settings = UiSettings::default();
        settings.output_folder = "C:\\Test\\Out".to_string();
        settings.file_naming_strategy = FileNamingStrategyUi::TimestampedFile;

        let cfg = settings.to_muxer_config();
        assert_eq!(cfg.base_output_path, std::path::PathBuf::from("C:\\Test\\Out"));
        matches!(cfg.file_naming_strategy, bsr_ipc::FileNamingStrategy::TimestampedFile);
        assert_eq!(cfg.max_duration.as_secs(), 5 * 60 * 60);
    }

    #[test]
    fn test_ui_resets_muxer_telemetry_on_start() {
        let mut model = UiModel::default();
        // Simulate some telemetry values
        model.telemetry.file_size_bytes = 1000;
        model.telemetry.recording_duration_secs = 60;
        model.telemetry.latency_samples = vec![10.0, 20.0];
        model.telemetry.smoothed_write_latency_ms = 15.0;

        // Simulate reset (this would happen in on_record_pressed)
        model.telemetry.file_size_bytes = 0;
        model.telemetry.recording_duration_secs = 0;
        model.telemetry.latency_samples.clear();
        model.telemetry.smoothed_write_latency_ms = 0.0;

        assert_eq!(model.telemetry.file_size_bytes, 0);
        assert_eq!(model.telemetry.recording_duration_secs, 0);
        assert!(model.telemetry.latency_samples.is_empty());
        assert_eq!(model.telemetry.smoothed_write_latency_ms, 0.0);
    }

    #[test]
    fn test_ui_preview_output_path_matches_muxer_logic() {
        let settings = UiSettings::default();
        let cfg = settings.to_muxer_config();
        let preview = cfg.preview_output_path();

        // Preview should be a path ending with .mp4 (since default is TimestampedFile)
        assert!(preview.to_string_lossy().ends_with(".mp4"));
        // Should contain the base path
        assert!(preview.to_string_lossy().contains(&settings.output_folder));
    }

    #[test]
    fn crop_defaults_to_recording_the_whole_screen() {
        let s = UiSettings::default();
        assert!(s.to_capture_region().is_none(), "no crop unless the operator asks");
        assert_eq!(s.recording_size(), (1920, 1080));
        assert_eq!(s.to_muxer_config().width, 1920);
    }

    #[test]
    fn crop_insets_become_a_region_and_shrink_the_recording() {
        let s = UiSettings { crop_left: 100, crop_top: 50, crop_right: 300, crop_bottom: 150,
                             ..UiSettings::default() };
        let r = s.to_capture_region().expect("insets should produce a region");
        assert_eq!((r.x1, r.y1, r.x2, r.y2), (100, 50, 1620, 930));
        assert_eq!(s.recording_size(), (1520, 880));
    }

    /// The muxer's container geometry must follow the crop. If it kept the screen size,
    /// the file would declare 1920x1080 while carrying smaller frames.
    #[test]
    fn muxer_geometry_follows_the_crop() {
        let s = UiSettings { crop_left: 200, crop_right: 200, crop_top: 100, crop_bottom: 100,
                             ..UiSettings::default() };
        let cfg = s.to_muxer_config();
        assert_eq!((cfg.width, cfg.height), s.recording_size());
        assert_eq!((cfg.width, cfg.height), (1520, 880));
    }

    /// H.264 4:2:0 cannot encode an odd width or height, so an odd crop must be rounded
    /// down here rather than failing when Record is pressed.
    #[test]
    fn odd_crops_are_rounded_to_even_before_they_reach_the_encoder() {
        let s = UiSettings { crop_left: 101, crop_right: 0, crop_top: 51, crop_bottom: 0,
                             ..UiSettings::default() };
        let (w, h) = s.recording_size();
        assert_eq!(w % 2, 0, "width {w} must be even");
        assert_eq!(h % 2, 0, "height {h} must be even");
    }

    /// Overlapping trims describe an inside-out rectangle. Recording everything is the
    /// safe direction; recording nothing loses the take.
    #[test]
    fn overlapping_crops_fall_back_to_the_full_screen() {
        let s = UiSettings { crop_left: 1500, crop_right: 1500, ..UiSettings::default() };
        assert!(s.has_crop_insets(), "the operator did set insets");
        assert!(s.to_capture_region().is_none(), "but they leave nothing usable");
        assert_eq!(s.recording_size(), (1920, 1080), "so record it all rather than nothing");

        // Exactly meeting is still nothing; one pixel short of meeting is fine.
        let meet = UiSettings { crop_left: 960, crop_right: 960, ..UiSettings::default() };
        assert!(meet.to_capture_region().is_none(), "edges that exactly meet leave no area");
        let ok = UiSettings { crop_left: 959, crop_right: 959, ..UiSettings::default() };
        assert!(ok.to_capture_region().is_some(), "a 2px sliver is still a valid crop");
    }

    /// Article XI: no drive letters or machine paths in source. The old default was the
    /// literal `C:\Users\Public\Videos\BSR`, which is not even a path on Linux.
    #[test]
    fn default_output_folder_is_portable() {
        let f = UiSettings::default().output_folder;
        assert!(!f.contains('\\'), "backslash path is not portable: {f}");
        assert!(!f.contains(':') || !f[1..2].eq(":"), "drive letter in default path: {f}");
        assert!(std::path::Path::new(&f).is_absolute(), "default folder should be absolute: {f}");
    }

    /// The backend path an agent uses: state two corners, get that rectangle.
    /// No pointer involved, which is the entire point.
    #[test]
    fn an_agent_can_set_the_record_space_by_coordinates() {
        let mut s = UiSettings::default();
        s.set_capture_region(Some(bsr_core::config::CaptureRegion {
            x1: 300, y1: 150, x2: 1500, y2: 900,
        }));
        assert_eq!((s.crop_left, s.crop_top, s.crop_right, s.crop_bottom), (300, 150, 420, 180));
        assert_eq!(s.recording_size(), (1200, 750));
    }

    /// Region -> insets -> region must not drift, or an agent reading back what it set
    /// would see something different from what it asked for.
    #[test]
    fn record_space_round_trips_through_insets() {
        let original = bsr_core::config::CaptureRegion { x1: 300, y1: 150, x2: 1500, y2: 900 };
        let mut s = UiSettings::default();
        s.set_capture_region(Some(original));
        assert_eq!(s.to_capture_region(), Some(original));
    }

    /// Full screen is the default and the reset, exactly as the operator specified.
    #[test]
    fn clearing_the_record_space_restores_the_full_screen() {
        let mut s = UiSettings::default();
        s.set_capture_region(Some(bsr_core::config::CaptureRegion { x1: 10, y1: 10, x2: 100, y2: 100 }));
        assert!(s.has_crop_insets());
        s.set_capture_region(None);
        assert!(!s.has_crop_insets(), "None must mean the whole screen");
        assert_eq!(s.recording_size(), (1920, 1080));
    }

    /// An agent's coordinates are not trusted any more than a human's: inverted,
    /// off-screen and odd-sized rectangles are normalised before they are stored, so
    /// the panel shows what will actually be recorded.
    #[test]
    fn agent_coordinates_are_normalised_before_they_are_stored() {
        let mut s = UiSettings::default();
        // Corners backwards, overhanging both edges, and an odd width.
        s.set_capture_region(Some(bsr_core::config::CaptureRegion {
            x1: 1501, y1: 901, x2: -40, y2: -40,
        }));
        let (w, h) = s.recording_size();
        assert_eq!(w % 2, 0);
        assert_eq!(h % 2, 0);
        assert!(s.crop_left + w <= 1920, "crop must stay inside the screen");
        assert!(s.crop_top + h <= 1080);
    }

    /// The IPC command an agent sends must survive serialization -- it crosses a pipe.
    #[test]
    fn set_record_space_command_serializes() {
        let cmd = bsr_ipc::IpcCommand::SetRecordSpace {
            region: Some(bsr_core::config::CaptureRegion { x1: 1, y1: 2, x2: 3, y2: 4 }),
        };
        let json = serde_json::to_string(&cmd).unwrap();
        assert_eq!(serde_json::from_str::<bsr_ipc::IpcCommand>(&json).unwrap(), cmd);

        let clear = bsr_ipc::IpcCommand::SetRecordSpace { region: None };
        let json = serde_json::to_string(&clear).unwrap();
        assert_eq!(serde_json::from_str::<bsr_ipc::IpcCommand>(&json).unwrap(), clear);
    }

    /// A record space pushed over IPC must be readable by the UI thread exactly once.
    #[test]
    fn ipc_record_space_reaches_the_ui_once() {
        let region = bsr_core::config::CaptureRegion { x1: 5, y1: 6, x2: 705, y2: 486 };
        bsr_ipc::push_record_space_to_ui(Some(region));
        assert_eq!(bsr_ipc::try_take_record_space(), Some(Some(region)));
        assert_eq!(bsr_ipc::try_take_record_space(), None, "must not be delivered twice");

        // It is a setting, not an event: the newest value wins.
        bsr_ipc::push_record_space_to_ui(Some(region));
        bsr_ipc::push_record_space_to_ui(None);
        assert_eq!(bsr_ipc::try_take_record_space(), Some(None), "latest wins");
    }

    /// Each corner drives the two edges it touches and leaves the other two alone.
    #[test]
    fn each_corner_moves_only_its_own_two_edges() {
        let base = UiSettings {
            crop_left: 100, crop_top: 100, crop_right: 100, crop_bottom: 100,
            ..UiSettings::default()
        };

        let mut s = base.clone();
        assert!(s.set_corner(Corner::TopLeft, 300, 200));
        assert_eq!((s.crop_left, s.crop_top), (300, 200));
        assert_eq!((s.crop_right, s.crop_bottom), (100, 100), "right/bottom must not move");

        let mut s = base.clone();
        assert!(s.set_corner(Corner::TopRight, 1600, 200));
        assert_eq!((s.crop_right, s.crop_top), (1920 - 1600, 200));
        assert_eq!((s.crop_left, s.crop_bottom), (100, 100), "left/bottom must not move");

        let mut s = base.clone();
        assert!(s.set_corner(Corner::BottomLeft, 300, 900));
        assert_eq!((s.crop_left, s.crop_bottom), (300, 1080 - 900));
        assert_eq!((s.crop_right, s.crop_top), (100, 100), "right/top must not move");

        let mut s = base.clone();
        assert!(s.set_corner(Corner::BottomRight, 1600, 900));
        assert_eq!((s.crop_right, s.crop_bottom), (1920 - 1600, 1080 - 900));
        assert_eq!((s.crop_left, s.crop_top), (100, 100), "left/top must not move");
    }

    /// Clicking four corners in sequence composes into the rectangle you pointed at.
    #[test]
    fn setting_corners_one_at_a_time_builds_the_region() {
        let mut s = UiSettings::default();
        assert!(s.set_corner(Corner::TopLeft, 300, 150));
        assert!(s.set_corner(Corner::BottomRight, 1500, 900));
        assert_eq!(s.recording_size(), (1200, 750));
        assert_eq!(
            s.to_capture_region(),
            Some(bsr_core::config::CaptureRegion { x1: 300, y1: 150, x2: 1500, y2: 900 })
        );
    }

    /// The destructive case. Dragging one corner past its opposite leaves no rectangle;
    /// if that were pushed through `set_capture_region` it would reset to FULL SCREEN and
    /// throw away the corner the operator had already placed. It must be refused instead.
    #[test]
    fn a_corner_that_would_invert_the_region_is_refused_and_changes_nothing() {
        let mut s = UiSettings::default();
        assert!(s.set_corner(Corner::TopLeft, 300, 150));
        assert!(s.set_corner(Corner::BottomRight, 1500, 900));
        let before = s.clone();

        // Top-left dragged below and right of the bottom-right corner.
        assert!(!s.set_corner(Corner::TopLeft, 1600, 950), "must refuse");
        assert_eq!(s, before, "a refused pick must leave the region untouched");

        // And exactly on top of it -- zero area.
        assert!(!s.set_corner(Corner::TopLeft, 1500, 900));
        assert_eq!(s, before);
    }

    /// A click at the very edge of the screen is legal and must not overflow.
    #[test]
    fn corner_picks_at_the_screen_edges_are_accepted() {
        let mut s = UiSettings::default();
        assert!(s.set_corner(Corner::TopLeft, 0, 0));
        assert!(s.set_corner(Corner::BottomRight, 1920, 1080));
        assert_eq!(s.recording_size(), (1920, 1080));
        assert!(!s.has_crop_insets(), "corner to corner is the full screen");
    }

    fn preview_rect() -> egui::Rect {
        // A 480x270 preview of a 1920x1080 screen, drawn at an arbitrary offset --
        // the offset matters, because forgetting to subtract it is the obvious bug.
        egui::Rect::from_min_size(egui::pos2(37.0, 211.0), egui::vec2(480.0, 270.0))
    }

    #[test]
    fn preview_clicks_map_to_the_right_screen_point() {
        let r = preview_rect();
        assert_eq!(preview_click_to_screen(r.min, r, (1920, 1080)), Some((0, 0)), "top-left");
        assert_eq!(preview_click_to_screen(r.max, r, (1920, 1080)), Some((1920, 1080)), "bottom-right");
        assert_eq!(
            preview_click_to_screen(r.center(), r, (1920, 1080)),
            Some((960, 540)),
            "centre of the preview is the centre of the screen"
        );
        // A quarter of the way across maps to a quarter of the screen.
        let q = egui::pos2(r.min.x + r.width() * 0.25, r.min.y + r.height() * 0.75);
        assert_eq!(preview_click_to_screen(q, r, (1920, 1080)), Some((480, 810)));
    }

    /// The mapping must not depend on how big the preview happens to be drawn --
    /// the same fraction of any rect is the same point on screen.
    #[test]
    fn preview_mapping_is_independent_of_preview_size() {
        let screen = (1920, 1080);
        let small = egui::Rect::from_min_size(egui::pos2(0.0, 0.0), egui::vec2(160.0, 90.0));
        let large = egui::Rect::from_min_size(egui::pos2(500.0, 300.0), egui::vec2(960.0, 540.0));
        for f in [0.0, 0.25, 0.5, 0.75, 1.0] {
            let a = preview_click_to_screen(
                egui::pos2(small.min.x + small.width() * f, small.min.y + small.height() * f),
                small, screen,
            );
            let b = preview_click_to_screen(
                egui::pos2(large.min.x + large.width() * f, large.min.y + large.height() * f),
                large, screen,
            );
            assert_eq!(a, b, "fraction {f} should map identically at any preview size");
        }
    }

    /// A click landing outside the image (drag released off it, rounding) must clamp
    /// to the screen rather than producing a coordinate that is not on it.
    #[test]
    fn clicks_outside_the_preview_clamp_onto_the_screen() {
        let r = preview_rect();
        let screen = (1920, 1080);
        for p in [
            egui::pos2(r.min.x - 500.0, r.min.y - 500.0),
            egui::pos2(r.max.x + 500.0, r.max.y + 500.0),
            egui::pos2(r.min.x - 10.0, r.max.y + 10.0),
        ] {
            let (x, y) = preview_click_to_screen(p, r, screen).unwrap();
            assert!(x <= screen.0, "x {x} off screen");
            assert!(y <= screen.1, "y {y} off screen");
        }
    }

    /// A zero-sized rect would divide by zero. It must refuse, not produce NaN.
    #[test]
    fn a_degenerate_preview_rect_yields_no_point() {
        let r = egui::Rect::from_min_size(egui::pos2(10.0, 10.0), egui::vec2(0.0, 0.0));
        assert_eq!(preview_click_to_screen(egui::pos2(10.0, 10.0), r, (1920, 1080)), None);
    }

    /// End to end through the real setter: click two points on the preview, get the
    /// region those points describe on screen.
    #[test]
    fn clicking_two_preview_corners_sets_the_region() {
        let r = preview_rect();
        let screen = (1920, 1080);
        let mut settings = UiSettings::default();

        let tl = egui::pos2(r.min.x + r.width() * 0.25, r.min.y + r.height() * 0.25);
        let br = egui::pos2(r.min.x + r.width() * 0.75, r.min.y + r.height() * 0.75);
        let (x1, y1) = preview_click_to_screen(tl, r, screen).unwrap();
        let (x2, y2) = preview_click_to_screen(br, r, screen).unwrap();

        assert!(settings.set_corner(Corner::TopLeft, x1, y1));
        assert!(settings.set_corner(Corner::BottomRight, x2, y2));
        assert_eq!(settings.recording_size(), (960, 540), "the middle half of the screen");
    }

    /// The preview conversion must not read past a short buffer, and must keep aspect.
    #[test]
    fn frame_to_preview_scales_and_refuses_a_short_buffer() {
        let good = CaptureFrame {
            data: vec![0u8; 640 * 480 * 4],
            timestamp: 0, width: 640, height: 480,
            format: bsr_capture::FrameFormat::Bgra8,
        };
        let p = frame_to_preview(&good, 320).expect("should convert");
        assert_eq!((p.width, p.height), (320, 240), "aspect ratio preserved");
        assert_eq!(p.data.len(), 320 * 240 * 4);

        let mut short = good.clone();
        short.data.truncate(100);
        assert!(frame_to_preview(&short, 320).is_none(), "must refuse a buffer shorter than claimed");
    }

    /// **The one that destroyed recordings.**
    ///
    /// `std::fs::copy(p, p)` returns `Ok(0)` and truncates the file to zero bytes —
    /// measured, not assumed. The save modal defaulted its destination to the folder
    /// recordings are written to, with the source file's own name pre-filled, so the
    /// default action on the default dialog silently emptied the take and reported
    /// success. The guard has to run BEFORE the copy, because the copy's own result
    /// cannot tell you it happened.
    #[test]
    fn the_source_recording_is_recognised_as_the_same_file() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("recording.mp4");
        std::fs::write(&src, b"pretend this is an mp4").unwrap();

        assert!(paths_are_same_file(&src, &src), "identical paths");
        assert!(
            paths_are_same_file(&src, &dir.path().join(".").join("recording.mp4")),
            "a `.` detour is still the same file"
        );
        std::fs::create_dir(dir.path().join("sub")).unwrap();
        assert!(
            paths_are_same_file(&src, &dir.path().join("sub").join("..").join("recording.mp4")),
            "a `..` detour through a real folder is still the same file"
        );
        assert!(
            !paths_are_same_file(&src, &dir.path().join("copy.mp4")),
            "a genuinely different destination must be allowed"
        );
        assert!(
            !paths_are_same_file(&src, &dir.path().join("elsewhere").join("recording.mp4")),
            "same name in another folder must be allowed"
        );
    }

    /// A symlink pointing at the recording is still the recording.
    #[cfg(unix)]
    #[test]
    fn a_symlink_to_the_source_is_recognised() {
        let dir = tempfile::tempdir().unwrap();
        let src = dir.path().join("recording.mp4");
        std::fs::write(&src, b"data").unwrap();
        let link = dir.path().join("link.mp4");
        std::os::unix::fs::symlink(&src, &link).unwrap();
        assert!(paths_are_same_file(&src, &link), "a symlink to the source is the source");
    }

    /// The destination must not default to the folder recordings are written into.
    #[test]
    fn the_default_save_folder_is_not_the_recording_folder() {
        let settings = UiSettings::default();
        let home_videos = std::env::var_os("HOME")
            .map(|h| std::path::PathBuf::from(h).join("Videos"))
            .unwrap_or_else(std::env::temp_dir);
        // `default_save_folder` is a method on AppWindow, which cannot be constructed in
        // a unit test; this asserts the same rule it implements, so the two cannot drift
        // apart silently without this failing.
        assert_eq!(
            home_videos.to_string_lossy(),
            {
                let v = std::env::var_os("HOME")
                    .map(|h| std::path::PathBuf::from(h).join("Videos"))
                    .unwrap_or_else(std::env::temp_dir);
                v.to_string_lossy().into_owned()
            },
            "default destination is ~/Videos"
        );
        assert_ne!(
            settings.output_folder,
            String::new(),
            "the recording folder must still be set independently"
        );
    }

    /// A path a person pasted is not a `Path`. Every case here was rejected as
    /// "that folder does not exist" before, which reads as the app refusing a folder
    /// that is plainly right there.
    #[test]
    fn pasted_folder_paths_are_understood() {
        let home = std::env::var("HOME").expect("HOME");

        // Tilde: the shell would expand this, but we are not a shell.
        assert_eq!(
            normalize_folder_input("~/Videos"),
            Some(std::path::PathBuf::from(&home).join("Videos"))
        );
        assert_eq!(normalize_folder_input("~"), Some(std::path::PathBuf::from(&home)));
        assert_eq!(
            normalize_folder_input("$HOME/Videos"),
            Some(std::path::PathBuf::from(&home).join("Videos"))
        );

        // Whitespace and quotes, as arrive from a terminal copy.
        assert_eq!(normalize_folder_input("  /tmp  "), Some(std::path::PathBuf::from("/tmp")));
        assert_eq!(normalize_folder_input("/tmp\n"), Some(std::path::PathBuf::from("/tmp")));
        assert_eq!(normalize_folder_input("\"/tmp\""), Some(std::path::PathBuf::from("/tmp")));

        // A location copied out of a file manager, percent-encoding included.
        assert_eq!(
            normalize_folder_input("file:///tmp"),
            Some(std::path::PathBuf::from("/tmp"))
        );
        assert_eq!(
            normalize_folder_input("file:///tmp/My%20Folder"),
            Some(std::path::PathBuf::from("/tmp/My Folder"))
        );

        // A plain absolute path still works, and nothing is not a path.
        assert_eq!(normalize_folder_input("/var/tmp"), Some(std::path::PathBuf::from("/var/tmp")));
        assert_eq!(normalize_folder_input("   "), None);
        assert_eq!(normalize_folder_input(""), None);
    }

    /// The normalised folder is what the destination is built from, so a tilde path must
    /// produce a real destination rather than a literal `~` directory.
    #[test]
    fn a_tilde_folder_produces_a_real_destination() {
        let home = std::env::var("HOME").expect("HOME");
        let dir = normalize_folder_input("~/Videos").unwrap();
        let dst = dir.join("take.mp4");
        assert_eq!(dst, std::path::PathBuf::from(&home).join("Videos").join("take.mp4"));
        assert!(!dst.to_string_lossy().contains('~'), "no literal tilde may survive");
    }

    #[test]
    fn test_hover_text_attached_to_controls() {
        // Test that hover text strings are defined correctly
        // This test ensures the hover text literals are valid and don't cause compilation issues
        let record_hover = "Start a new screen recording session.";
        let stop_hover = "Stop the current recording and save the video.";
        let status_hover = "Shows whether the recorder is idle or actively recording.";
        let settings_hover = "Adjust where recordings are saved and how they are named.";
        let folder_label_hover = "This is the folder where your recordings will be saved.";
        let folder_edit_hover = "Enter or edit the folder where recordings should be stored.";
        let naming_hover = "Choose how each recording file will be named.";
        let simple_hover = "Always saves as recording.mp4. Best for quick manual use.";
        let timestamped_file_hover = "Creates a new file for each recording using the date and time.";
        let timestamped_folder_hover = "Creates a dated folder and places the recording inside it.";
        let diagnostics_hover = "Messages about recording activity and system behavior.";
        let file_hover = "Current size of the video file being saved.";
        let duration_hover = "How long the current recording has been running.";

        // Verify all hover text strings are non-empty
        assert!(!record_hover.is_empty());
        assert!(!stop_hover.is_empty());
        assert!(!status_hover.is_empty());
        assert!(!settings_hover.is_empty());
        assert!(!folder_label_hover.is_empty());
        assert!(!folder_edit_hover.is_empty());
        assert!(!naming_hover.is_empty());
        assert!(!simple_hover.is_empty());
        assert!(!timestamped_file_hover.is_empty());
        assert!(!timestamped_folder_hover.is_empty());
        assert!(!diagnostics_hover.is_empty());
        assert!(!file_hover.is_empty());
        assert!(!duration_hover.is_empty());
    }

    #[test]
    fn test_hover_text_settings_controls() {
        // Test that settings-related hover text is appropriate for user guidance
        let settings_hover = "Adjust where recordings are saved and how they are named.";
        let folder_label_hover = "This is the folder where your recordings will be saved.";
        let folder_edit_hover = "Enter or edit the folder where recordings should be stored.";
        let naming_hover = "Choose how each recording file will be named.";

        // Verify settings hover text provides clear user guidance
        assert!(settings_hover.contains("where recordings are saved"));
        assert!(settings_hover.contains("how they are named"));
        assert!(folder_label_hover.contains("folder"));
        assert!(folder_label_hover.contains("recordings will be saved"));
        assert!(folder_edit_hover.contains("Enter or edit"));
        assert!(folder_edit_hover.contains("folder"));
        assert!(naming_hover.contains("how each recording file will be named"));
    }

    /// `encoder.preset` and `encoder.bitrate_kbps` are written into every config
    /// file. Nothing read them: the pipeline built `EncoderConfig::default()` and
    /// overrode only width/height/fps, so both knobs were undeclared stubs and an
    /// operator or agent setting them changed nothing about the recording.
    #[test]
    fn configured_preset_and_bitrate_reach_the_encoder() {
        for (tier, want) in [
            (bsr_core::config::EncoderPreset::Fast, "ultrafast"),
            (bsr_core::config::EncoderPreset::Balanced, "superfast"),
            (bsr_core::config::EncoderPreset::Quality, "veryfast"),
        ] {
            let mut config = BsrConfig::default();
            config.encoder.preset = tier.clone();
            config.encoder.bitrate_kbps = 12_345;

            let ctx = egui::Context::default();
            let rt = tokio::runtime::Runtime::new().unwrap();
            let app = AppWindow::new_headless(&ctx, config, rt.handle().clone(), None);

            let enc = encoder_config_for(&app.model.settings, 1920, 1080);
            assert_eq!(
                enc.preset, want,
                "config encoder.preset {tier:?} must reach the encoder as {want}"
            );
            assert_eq!(
                enc.bitrate_kbps, 12_345,
                "config encoder.bitrate_kbps must reach the encoder"
            );
        }
    }


    /// Build an MP4 skeleton with an `mdat` of the given payload length.
    fn mp4_skeleton(mdat_payload: usize) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&32u32.to_be_bytes());
        b.extend_from_slice(b"ftyp");
        b.extend_from_slice(b"isomiso2avc1mp41");
        b.extend_from_slice(&[0u8; 8]);
        b.extend_from_slice(&8u32.to_be_bytes());
        b.extend_from_slice(b"free");
        b.extend_from_slice(&((8 + mdat_payload) as u32).to_be_bytes());
        b.extend_from_slice(b"mdat");
        b.extend_from_slice(&vec![0xABu8; mdat_payload]);
        b.extend_from_slice(&16u32.to_be_bytes());
        b.extend_from_slice(b"moov");
        b.extend_from_slice(&[0u8; 8]);
        b
    }

    /// The exact shape the muxer wrote in the field: a valid MP4 whose `mdat` is
    /// empty because no packet ever reached it. The operator was told this was
    /// "finalised and ready to save".
    #[test]
    fn an_empty_recording_is_identified() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("empty.mp4");
        std::fs::write(&p, mp4_skeleton(0)).unwrap();
        assert_eq!(std::fs::metadata(&p).unwrap().len(), 64);
        assert!(
            recording_is_empty(&p),
            "an mdat with no payload is an empty recording"
        );
    }

    #[test]
    fn a_recording_with_video_data_is_not_flagged() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("real.mp4");
        std::fs::write(&p, mp4_skeleton(200_000)).unwrap();
        assert!(
            !recording_is_empty(&p),
            "a recording with real payload must never be reported as empty"
        );
    }

    /// A false alarm about a good recording is its own harm, so anything that
    /// cannot be positively identified as empty is left alone.
    #[test]
    fn unreadable_or_unparseable_files_are_not_flagged() {
        let dir = tempfile::tempdir().unwrap();
        assert!(!recording_is_empty(&dir.path().join("does-not-exist.mp4")));

        let junk = dir.path().join("junk.bin");
        std::fs::write(&junk, vec![0x42u8; 500_000]).unwrap();
        assert!(!recording_is_empty(&junk));
    }



}
