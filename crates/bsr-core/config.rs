// SPDX-License-Identifier: MIT
// Baxter's Screen Record — Config system
// Block A-2: config.rs

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use thiserror::Error;
use std::env;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BsrConfig {
    pub capture: CaptureConfig,
    pub encoder: EncoderConfig,
    pub output: OutputConfig,
    pub hotkeys: HotkeyConfig,
    pub stealth: StealthConfig,
    pub hrt: HrtConfig,
    pub ui: UiConfig,
    pub logging: LoggingConfig,
    pub advanced: AdvancedConfig,
}

impl Default for BsrConfig {
    fn default() -> Self {
        Self {
            capture: CaptureConfig::default(),
            encoder: EncoderConfig::default(),
            output: OutputConfig::default(),
            hotkeys: HotkeyConfig::default(),
            stealth: StealthConfig::default(),
            hrt: HrtConfig::default(),
            ui: UiConfig::default(),
            logging: LoggingConfig::default(),
            advanced: AdvancedConfig::default(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureConfig {
    pub fps: u32,
    pub region: Option<CaptureRegion>,
    pub monitor: Option<String>,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            fps: 30,
            region: None,
            monitor: None,
        }
    }
}

// PartialEq/Eq so it can travel inside IpcCommand (which is compared in tests);
// Copy because it is four i32s and passing it by value reads better everywhere.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct CaptureRegion {
    pub x1: i32,
    pub y1: i32,
    pub x2: i32,
    pub y2: i32,
}

impl CaptureRegion {
    pub fn width(&self) -> i32 { self.x2 - self.x1 }
    pub fn height(&self) -> i32 { self.y2 - self.y1 }
    pub fn is_valid(&self) -> bool { self.width() > 0 && self.height() > 0 }

    /// Resolve this region against a real frame, returning `(x, y, width, height)`
    /// in pixels, or `None` if nothing usable is left.
    ///
    /// This is the whole reason the type needs more than `width()`/`height()`: a region
    /// typed in by hand is not trustworthy. It can be inverted, negative, partly or
    /// wholly off-screen, or an odd number of pixels wide.
    ///
    /// * corners are **normalised**, so dragging from bottom-right to top-left works;
    /// * the rect is **clamped** to the frame, so a region larger than the screen
    ///   records the screen rather than reading past the end of the buffer;
    /// * width and height are **snapped down to even numbers**, because H.264's 4:2:0
    ///   chroma subsampling has no way to represent an odd dimension — an odd crop is
    ///   rejected by the encoder, and the recording would fail at Record time rather
    ///   than when the value was entered;
    /// * anything smaller than 2x2 after all that returns `None`, meaning "no crop".
    pub fn resolve(&self, frame_width: u32, frame_height: u32) -> Option<(u32, u32, u32, u32)> {
        if frame_width < 2 || frame_height < 2 {
            return None;
        }
        let (fw, fh) = (frame_width as i64, frame_height as i64);

        // Normalise: accept the corners in any order.
        let (left, right) = (self.x1.min(self.x2) as i64, self.x1.max(self.x2) as i64);
        let (top, bottom) = (self.y1.min(self.y2) as i64, self.y1.max(self.y2) as i64);

        // Clamp into the frame.
        let left = left.clamp(0, fw);
        let top = top.clamp(0, fh);
        let right = right.clamp(0, fw);
        let bottom = bottom.clamp(0, fh);

        // Snap the SIZE down to even, keeping the origin where the operator put it.
        let w = (right - left) & !1;
        let h = (bottom - top) & !1;
        if w < 2 || h < 2 {
            return None;
        }
        Some((left as u32, top as u32, w as u32, h as u32))
    }
}

#[cfg(test)]
mod capture_region_tests {
    use super::CaptureRegion;

    fn r(x1: i32, y1: i32, x2: i32, y2: i32) -> CaptureRegion {
        CaptureRegion { x1, y1, x2, y2 }
    }

    #[test]
    fn plain_region_resolves_unchanged() {
        assert_eq!(r(100, 50, 900, 650).resolve(1920, 1080), Some((100, 50, 800, 600)));
    }

    #[test]
    fn corners_may_be_given_in_any_order() {
        // Dragging bottom-right to top-left must mean the same rectangle.
        assert_eq!(
            r(900, 650, 100, 50).resolve(1920, 1080),
            r(100, 50, 900, 650).resolve(1920, 1080)
        );
    }

    #[test]
    fn odd_sizes_snap_down_to_even_for_h264() {
        // 801x601 would be rejected by a 4:2:0 encoder at Record time.
        let (x, y, w, h) = r(0, 0, 801, 601).resolve(1920, 1080).unwrap();
        assert_eq!((x, y), (0, 0));
        assert_eq!((w, h), (800, 600));
        assert_eq!(w % 2, 0);
        assert_eq!(h % 2, 0);
    }

    #[test]
    fn region_larger_than_the_screen_is_clamped_not_read_past() {
        assert_eq!(r(-500, -500, 5000, 5000).resolve(1920, 1080), Some((0, 0, 1920, 1080)));
    }

    #[test]
    fn region_entirely_off_screen_is_no_crop_rather_than_a_panic() {
        assert_eq!(r(4000, 4000, 5000, 5000).resolve(1920, 1080), None);
    }

    #[test]
    fn degenerate_and_sub_pixel_regions_are_rejected() {
        assert_eq!(r(10, 10, 10, 10).resolve(1920, 1080), None, "zero area");
        assert_eq!(r(10, 10, 11, 11).resolve(1920, 1080), None, "1x1 snaps to 0x0");
        assert_eq!(r(0, 0, 1920, 1).resolve(1920, 1080), None, "one pixel tall");
    }

    #[test]
    fn a_crop_never_escapes_the_frame() {
        // The property the buffer copy depends on, over a spread of awkward inputs.
        for &(x1, y1, x2, y2) in &[
            (-9, -9, 33, 33), (0, 0, 1921, 1081), (1919, 1079, -5, -5), (7, 3, 1913, 1077),
        ] {
            if let Some((x, y, w, h)) = r(x1, y1, x2, y2).resolve(1920, 1080) {
                assert!(x + w <= 1920, "{x}+{w} overruns width");
                assert!(y + h <= 1080, "{y}+{h} overruns height");
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncoderConfig {
    pub encoder_name: Option<String>,
    pub codec: VideoCodec,
    pub preset: EncoderPreset,
    pub bitrate_kbps: u32,
}

impl Default for EncoderConfig {
    fn default() -> Self {
        Self {
            encoder_name: None,
            codec: VideoCodec::H264,
            preset: EncoderPreset::Balanced,
            bitrate_kbps: 8000,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum VideoCodec {
    H264,
    H265,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum EncoderPreset {
    Fast,
    Balanced,
    Quality,
}

impl EncoderPreset {
    /// The x264 preset name this tier maps to.
    ///
    /// Chosen against a measurement on the reference box (1920x1080, release
    /// build, BGRA->YUV420P scale plus encode, per frame):
    ///
    /// | x264 preset | ms/frame | sustainable fps |
    /// |-------------|----------|-----------------|
    /// | fast        | 46.4     | 21.6            |
    /// | veryfast    | 36.8     | 27.2            |
    /// | superfast   | 26.9     | 37.1            |
    /// | ultrafast   | 22.1     | 45.2            |
    ///
    /// A screen recorder that cannot sustain its target frame rate sheds
    /// frames in the capture loop, so `Balanced` is the fastest tier with real
    /// headroom over 30 fps rather than the nominally "balanced" x264 name.
    /// `Quality` deliberately trades frame rate for picture and may not hold
    /// 30 fps at 1080p.
    pub fn x264_name(&self) -> &'static str {
        match self {
            EncoderPreset::Fast => "ultrafast",
            EncoderPreset::Balanced => "superfast",
            EncoderPreset::Quality => "veryfast",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutputConfig {
    pub output_folder: String,
    pub container: ContainerFormat,
    pub auto_remux_to_mp4: bool,
    pub file_name_pattern: String,
    pub max_duration_hours: u32,
}

/// The user's home directory, on either platform.
fn home_dir() -> Option<PathBuf> {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

/// Is this path unusable on the platform we are actually running on?
///
/// A configuration file written on Windows — or a stale default like
/// `C:\Users\Default\Videos\BSR Recordings` — is not merely wrong on Linux, it is
/// actively dangerous: FFmpeg reads the leading `C:` as a protocol scheme and fails with
/// "Protocol not found", after the recording has already been made.
pub fn is_foreign_path(path: &str) -> bool {
    if path.trim().is_empty() {
        return true;
    }
    #[cfg(not(windows))]
    {
        let b = path.as_bytes();
        // A drive letter, e.g. `C:\...` or `C:/...`
        if b.len() >= 2 && b[0].is_ascii_alphabetic() && b[1] == b':' {
            return true;
        }
        // A UNC path, or any path using backslash separators.
        if path.starts_with("\\\\") || path.contains('\\') {
            return true;
        }
    }
    #[cfg(windows)]
    {
        // A POSIX absolute path is not usable on Windows.
        if path.starts_with('/') {
            return true;
        }
    }
    false
}

impl Default for OutputConfig {
    fn default() -> Self {
        // The comment here used to claim "cross-platform compatibility" while doing the
        // opposite: it read USERPROFILE (Windows-only, unset on Linux) and fell back to
        // the literal `C:\Users\Default`, joined with backslashes. On Linux that produced
        // `C:\Users\Default\Videos\BSR Recordings`, and FFmpeg parses the leading `C:` as
        // a URL scheme — a recording died with "Protocol not found" on a clean install.
        // Article XI: derive from one root and join portably, never a drive letter.
        let output_folder = home_dir()
            .map(|h| h.join("Videos").join("BSR Recordings"))
            .unwrap_or_else(|| env::temp_dir().join("BSR Recordings"))
            .to_string_lossy()
            .into_owned();

        Self {
            output_folder,
            container: ContainerFormat::Mp4,
            auto_remux_to_mp4: false,
            file_name_pattern: "BSR_{date}_{time}".to_string(),
            max_duration_hours: 5,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum ContainerFormat {
    Mkv,
    Mp4,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HotkeyConfig {
    pub start_recording: String,
    pub stop_recording: String,
    pub toggle_stealth: String,
    pub rescue: String,
}

impl Default for HotkeyConfig {
    fn default() -> Self {
        Self {
            start_recording: "Ctrl+Shift+R".to_string(),
            stop_recording: "Ctrl+Shift+S".to_string(),
            toggle_stealth: "Ctrl+Shift+T".to_string(),
            rescue: "Ctrl+Shift+Escape".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StealthConfig {
    pub enabled: bool,
    pub full_screen: bool,
    pub region: Option<CaptureRegion>,
}

impl Default for StealthConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            full_screen: false,
            region: None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HrtConfig {
    pub enabled: bool,
    pub pipe_path: String,
    pub throttle_temp_c: f64,
    pub auto_stop_temp_c: f64,
    pub e_stop_enabled: bool,
}

impl Default for HrtConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            pipe_path: r"\\.\pipe\hot-rod-tuner".to_string(),
            throttle_temp_c: 85.0,
            auto_stop_temp_c: 95.0,
            e_stop_enabled: true,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiConfig {
    pub theme: String,
}

impl Default for UiConfig {
    fn default() -> Self {
        Self {
            theme: "charcoal".to_string(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LoggingConfig {
    pub level: String,
    pub log_to_file: bool,
    pub max_files: usize,
    pub max_file_size_mb: usize,
}

impl Default for LoggingConfig {
    fn default() -> Self {
        Self {
            level: "info".to_string(),
            log_to_file: true,
            max_files: 5,
            max_file_size_mb: 10,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AdvancedConfig {
    pub ffmpeg_path: String,
    pub temp_dir: String,
    pub worker_threads: usize,
    pub buffer_size_mb: usize,
    pub enable_telemetry: bool,
    pub enable_diagnostics: bool,
}

impl Default for AdvancedConfig {
    fn default() -> Self {
        let temp_dir = env::var("TEMP").unwrap_or_else(|_| "C:\\Temp".to_string());
        let temp_dir = format!("{}\\bsr", temp_dir);

        Self {
            ffmpeg_path: "ffmpeg".to_string(),
            temp_dir,
            worker_threads: 0, // 0 = auto-detect
            buffer_size_mb: 64,
            enable_telemetry: true,
            enable_diagnostics: true,
        }
    }
}

#[derive(Debug, Error)]
pub enum ConfigError {
    #[error("Failed to load config: {0}")]
    Load(String),
    #[error("Failed to save config: {0}")]
    Save(String),
    #[error("Validation error: {0}")]
    Validation(String),
}

pub fn load_config(path: &PathBuf) -> Result<BsrConfig, ConfigError> {
    let data = std::fs::read_to_string(path)
        .map_err(|e| ConfigError::Load(e.to_string()))?;
    let mut config: BsrConfig =
        toml::from_str(&data).map_err(|e| ConfigError::Load(e.to_string()))?;
    config.repair_for_this_platform();
    Ok(config)
}

impl BsrConfig {
    /// Replace settings a config file carries that cannot work on this platform.
    ///
    /// Config files travel: this project was ported from Windows and its committed
    /// `bsr-config.toml` still holds `C:\Users\Default\Videos\BSR Recordings`. Reading
    /// that on Linux does not fail at load — it fails *after* a recording has been made,
    /// deep inside FFmpeg, as "Protocol not found", because `C:` parses as a URL scheme.
    /// Repairing at load turns a lost take into a log line.
    pub fn repair_for_this_platform(&mut self) -> Vec<String> {
        let mut repaired = Vec::new();
        if is_foreign_path(&self.output.output_folder) {
            let fallback = OutputConfig::default().output_folder;
            repaired.push(format!(
                "output folder {:?} is not usable on this platform; using {:?}",
                self.output.output_folder, fallback
            ));
            self.output.output_folder = fallback;
        }
        repaired
    }
}

pub fn load_config_from_default_path() -> Result<BsrConfig, ConfigError> {
    let config_path = get_config_path()?;
    load_config(&config_path)
}

pub fn get_config_path() -> Result<PathBuf, ConfigError> {
    // First try the executable directory (packaged app)
    if let Ok(exe_path) = env::current_exe() {
        if let Some(exe_dir) = exe_path.parent() {
            let config_dir = exe_dir.join("config");
            let config_path = config_dir.join("bsr-config.toml");

            // If config exists in exe directory, use it
            if config_path.exists() {
                return Ok(config_path);
            }

            // If config directory exists but no config file, we'll create it
            if config_dir.exists() {
                return Ok(config_path);
            }
        }
    }

    // Fallback to current directory
    let config_path = PathBuf::from("bsr-config.toml");
    if config_path.exists() {
        return Ok(config_path);
    }

    // Final fallback: create in current directory
    Ok(config_path)
}

pub fn ensure_config_exists() -> Result<PathBuf, ConfigError> {
    let config_path = get_config_path()?;

    if !config_path.exists() {
        // Create config directory if it doesn't exist
        if let Some(parent) = config_path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| ConfigError::Save(format!("Failed to create config directory: {}", e)))?;
        }

        // Save default config
        let default_config = BsrConfig::default();
        save_config_to_path(&default_config, &config_path)?;
    }

    Ok(config_path)
}

pub fn save_config_to_path(config: &BsrConfig, path: &PathBuf) -> Result<(), ConfigError> {
    let toml = toml::to_string(config)
        .map_err(|e| ConfigError::Save(e.to_string()))?;
    std::fs::write(path, toml)
        .map_err(|e| ConfigError::Save(e.to_string()))
}

pub fn validate_config(config: &BsrConfig) -> Vec<ConfigError> {
    let mut errors = Vec::new();
    if config.capture.fps < 10 || config.capture.fps > 120 {
        errors.push(ConfigError::Validation("FPS must be between 10 and 120".to_string()));
    }
    if config.encoder.bitrate_kbps < 1000 || config.encoder.bitrate_kbps > 100_000 {
        errors.push(ConfigError::Validation("Bitrate must be between 1000 and 100000 kbps".to_string()));
    }
    if !config.output.output_folder.starts_with("C:/") && !config.output.output_folder.starts_with("D:/") {
        errors.push(ConfigError::Validation("Output folder must be on C:/ or D:/".to_string()));
    }
    errors
}

#[cfg(test)]
#[path = "config_test.rs"]
mod config_test;

#[cfg(test)]
mod platform_path_tests {
    use super::*;

    /// **The bug a clean package install found.** `OutputConfig::default()` read
    /// `USERPROFILE` — Windows-only — and fell back to the literal `C:\Users\Default`,
    /// joined with backslashes. On Linux that is not a path at all, and FFmpeg parses the
    /// leading `C:` as a URL scheme: a recording completed and then died in the muxer with
    /// "Protocol not found", losing the take.
    #[test]
    fn the_default_output_folder_is_usable_on_this_platform() {
        let folder = OutputConfig::default().output_folder;
        assert!(!folder.is_empty());
        assert!(
            !is_foreign_path(&folder),
            "the default output folder is not usable here: {folder:?}"
        );
        assert!(
            std::path::Path::new(&folder).is_absolute(),
            "must be absolute, got {folder:?}"
        );
        #[cfg(not(windows))]
        {
            assert!(!folder.contains('\\'), "no backslash separators on unix: {folder:?}");
            assert!(!folder.contains(':'), "no drive letter on unix: {folder:?}");
        }
    }

    #[test]
    fn windows_paths_are_recognised_as_foreign_on_unix() {
        #[cfg(not(windows))]
        {
            for p in [
                "C:\\Users\\Default\\Videos\\BSR Recordings",
                "D:/Videos",
                "\\\\server\\share\\clips",
                "Videos\\BSR",
            ] {
                assert!(is_foreign_path(p), "{p:?} should be foreign here");
            }
            for p in ["/home/someone/Videos", "/tmp/x", "/var/tmp/BSR Recordings"] {
                assert!(!is_foreign_path(p), "{p:?} is a perfectly good unix path");
            }
        }
        assert!(is_foreign_path(""), "an empty path is unusable everywhere");
        assert!(is_foreign_path("   "), "and so is whitespace");
    }

    /// A config file carrying the Windows path must be repaired at load, not honoured.
    /// This project's own committed `bsr-config.toml` still contains exactly that value.
    #[test]
    fn a_config_with_a_windows_path_is_repaired_on_load() {
        let mut config = BsrConfig::default();
        config.output.output_folder = "C:\\Users\\Default\\Videos\\BSR Recordings".to_string();

        let notes = config.repair_for_this_platform();

        #[cfg(not(windows))]
        {
            assert_eq!(notes.len(), 1, "the repair must be reported, not silent");
            assert!(notes[0].contains("not usable"), "got: {}", notes[0]);
            assert!(
                !is_foreign_path(&config.output.output_folder),
                "still unusable after repair: {:?}",
                config.output.output_folder
            );
        }
    }

    /// A good config must be left completely alone.
    #[test]
    fn a_usable_config_is_not_touched() {
        let mut config = BsrConfig::default();
        let before = config.output.output_folder.clone();
        assert!(config.repair_for_this_platform().is_empty(), "nothing to repair");
        assert_eq!(config.output.output_folder, before);
    }
}
