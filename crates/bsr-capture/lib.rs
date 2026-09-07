// SPDX-License-Identifier: MIT
// Baxter's Screen Record — Screen Capture Layer
// Raw frame stream for the recorder.
//
// One `CaptureBackend` trait, one real implementation per platform:
//   * Windows — `dxgi_backend`, DXGI Desktop Duplication.
//   * Linux   — `portal_backend`, xdg-desktop-portal ScreenCast -> PipeWire.
// `platform_backend()` selects between them. There is no synthetic backend in a
// product build; see the `compile_error!` immediately below.

// ---------------------------------------------------------------------------
// Article VII, enforced at compile time.
//
// This crate previously declared `mod mock_backend` under `#[cfg(not(windows))]`,
// so on Linux the *shipping* capture backend invented its frames: it logged
// "Mock backend initialized", reported success, passed every test, and produced a
// recording containing nothing that was ever on screen. A screen recorder that
// cannot see the screen must refuse to exist, not quietly make something up.
//
// The mock is now `#[cfg(test)]` and cannot be linked into a product build at all.
// This guard closes the other half: a target with no real backend compiled in fails
// to BUILD rather than silently shipping without one.
// ---------------------------------------------------------------------------
#[cfg(all(not(windows), not(all(target_os = "linux", feature = "linux-capture"))))]
compile_error!(
    "bsr-capture has no real capture backend for this target. On Linux, build with the \
     `linux-capture` feature (enabled by default) and the GStreamer development headers \
     installed: libgstreamer1.0-dev, libgstreamer-plugins-base1.0-dev. There is \
     deliberately no synthetic fallback — a screen recorder that invents frames is worse \
     than one that refuses to start."
);

use bsr_core::buffer::DropOldestBuffer;
use serde::{Deserialize, Serialize};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{broadcast, mpsc, watch, Notify};
use tracing::{info, warn};
use bsr_ipc::TelemetryEvent;

/// Capture error types
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum CaptureError {
    InitializationFailed(String),
    FrameAcquisitionFailed(String),
    UnsupportedFormat(String),
    Shutdown(String),
}

impl std::fmt::Display for CaptureError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            CaptureError::InitializationFailed(msg) => write!(f, "Init failed: {}", msg),
            CaptureError::FrameAcquisitionFailed(msg) => write!(f, "Frame acquisition failed: {}", msg),
            CaptureError::UnsupportedFormat(msg) => write!(f, "Unsupported format: {}", msg),
            CaptureError::Shutdown(msg) => write!(f, "Shutdown: {}", msg),
        }
    }
}

impl std::error::Error for CaptureError {}

/// Captured frame data
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CaptureFrame {
    pub data: Vec<u8>,
    pub timestamp: u64, // nanoseconds since epoch
    pub width: u32,
    pub height: u32,
    pub format: FrameFormat,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum FrameFormat {
    Bgra8,
    Rgba8,
}

/// Capture backend trait
#[async_trait::async_trait]
pub trait CaptureBackend: Send + Sync + 'static {
    async fn initialize(&mut self) -> Result<(), CaptureError>;
    async fn capture_frame(&mut self) -> Result<CaptureFrame, CaptureError>;
    async fn shutdown(&mut self) -> Result<(), CaptureError>;
}

/// Capture configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureConfig {
    pub width: u32,
    pub height: u32,
    pub fps: u32,
    pub drop_policy: DropPolicy,
    /// Seed-BSR-G2-04-11: capacity of the DropOldest frame ring buffer.
    #[serde(default = "CaptureConfig::default_buffer_capacity")]
    pub buffer_capacity: usize,
    /// Record only part of the screen. `None` records all of it.
    ///
    /// Applied here in the platform-independent service rather than inside a backend,
    /// so DXGI and the portal backend crop identically and neither has to know about it.
    /// `bsr_core`'s `CaptureRegion` was declared, serialized into every config file, and
    /// given `width()`/`height()`/`is_valid()` helpers, but **no code anywhere read it** —
    /// a dead knob. This is where it finally does something.
    #[serde(default)]
    pub region: Option<bsr_core::config::CaptureRegion>,
}

impl Default for CaptureConfig {
    fn default() -> Self {
        Self {
            width: 1920,
            height: 1080,
            fps: 30,
            drop_policy: DropPolicy::DropNewest,
            buffer_capacity: Self::default_buffer_capacity(),
            region: None,
        }
    }
}

impl CaptureConfig {
    fn default_buffer_capacity() -> usize { 8 }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub enum DropPolicy {
    DropNewest,
    DropOldest,
}

/// Capture telemetry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CaptureTelemetry {
    pub frames_captured: u64,
    pub frames_dropped: u64,
    pub avg_latency_ms: f64,
}

impl Default for CaptureTelemetry {
    fn default() -> Self {
        Self {
            frames_captured: 0,
            frames_dropped: 0,
            avg_latency_ms: 0.0,
        }
    }
}

/// Copy a sub-rectangle out of a frame.
///
/// The region is resolved against this frame's real dimensions first, so an inverted,
/// off-screen, oversized or odd-sized rectangle can never produce an out-of-bounds read
/// — see `CaptureRegion::resolve`. A region that resolves to nothing returns the frame
/// untouched, which is the safe direction: recording too much beats recording nothing.
///
/// Rows are copied individually because the destination is tightly packed while the
/// source stride is the full frame width.
pub fn crop_frame(frame: &CaptureFrame, region: &bsr_core::config::CaptureRegion) -> CaptureFrame {
    let Some((x, y, w, h)) = region.resolve(frame.width, frame.height) else {
        return frame.clone();
    };
    if (x, y, w, h) == (0, 0, frame.width, frame.height) {
        return frame.clone();
    }

    let src_stride = frame.width as usize * 4;
    let dst_stride = w as usize * 4;
    let needed = src_stride * frame.height as usize;
    if frame.data.len() < needed {
        // The frame is not the size it claims. Cropping it would read past the end.
        warn!(
            "frame claims {}x{} ({} bytes) but carries {} — skipping crop",
            frame.width, frame.height, needed, frame.data.len()
        );
        return frame.clone();
    }

    let mut data = vec![0u8; dst_stride * h as usize];
    for row in 0..h as usize {
        let src = (y as usize + row) * src_stride + x as usize * 4;
        data[row * dst_stride..(row + 1) * dst_stride]
            .copy_from_slice(&frame.data[src..src + dst_stride]);
    }

    CaptureFrame { data, timestamp: frame.timestamp, width: w, height: h, format: frame.format.clone() }
}

/// Capture service
///
/// Seed-BSR-G1-02-11: Frames are pushed into the shared `frame_buf`
/// (DropOldest policy) and the encoder is woken via `frame_notify`.
pub struct CaptureService<B: CaptureBackend> {
    backend: B,
    config: CaptureConfig,
    telemetry: CaptureTelemetry,
    telemetry_tx: broadcast::Sender<TelemetryEvent>,
    shutdown_rx: mpsc::Receiver<()>,
    frame_buf: Arc<Mutex<DropOldestBuffer<CaptureFrame>>>,
    frame_notify: Arc<Notify>,
    /// Optional live-preview sink and the minimum interval between sends.
    ///
    /// The preview gets its **own** copy of each frame. It must never pop from
    /// `frame_buf`: that buffer belongs to the encoder, and a second consumer on
    /// it removes frames from the recording itself.
    preview: Option<(watch::Sender<Option<CaptureFrame>>, Duration)>,
}

impl<B: CaptureBackend> CaptureService<B> {
    /// Create a new CaptureService.
    ///
    /// `frame_buf` and `frame_notify` must be shared with the EncoderService
    /// so captured frames flow through the DropOldest ring buffer.
    pub fn new(
        backend: B,
        config: CaptureConfig,
        telemetry_tx: broadcast::Sender<TelemetryEvent>,
        shutdown_rx: mpsc::Receiver<()>,
        frame_buf: Arc<Mutex<DropOldestBuffer<CaptureFrame>>>,
        frame_notify: Arc<Notify>,
    ) -> Self {
        Self {
            backend,
            config,
            telemetry: CaptureTelemetry::default(),
            telemetry_tx,
            shutdown_rx,
            frame_buf,
            frame_notify,
            preview: None,
        }
    }

    /// Send a copy of each captured frame to a live preview, at most once per
    /// `min_interval`.
    ///
    /// A preview must never share the encoder's ring buffer. When it did, the
    /// preview task and the encoder service were two consumers woken by one
    /// `notify_one()` — which wakes exactly one waiter — and every frame the
    /// preview won was discarded after being scaled for display. The recording
    /// lost those frames outright.
    pub fn with_preview(
        mut self,
        tx: watch::Sender<Option<CaptureFrame>>,
        min_interval: Duration,
    ) -> Self {
        self.preview = Some((tx, min_interval));
        self
    }

    pub async fn run(mut self) -> Result<(), CaptureError> {
        info!("Capture service starting");
        self.backend.initialize().await?;

        let mut last_preview: Option<std::time::Instant> = None;
        let frame_interval = Duration::from_secs_f64(1.0 / self.config.fps as f64);
        let mut interval = tokio::time::interval(frame_interval);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                _ = interval.tick() => {
                    match self.backend.capture_frame().await {
                        Ok(frame) => {
                            let frame = match &self.config.region {
                                Some(region) => crop_frame(&frame, region),
                                None => frame,
                            };
                            self.telemetry.frames_captured += 1;
                            // The preview's own copy, taken before the frame moves
                            // into the encoder's buffer. Throttled: a full-size clone
                            // at capture rate is copying the recording never needs.
                            if let Some((tx, min_interval)) = &self.preview {
                                let due = last_preview
                                    .map_or(true, |t: std::time::Instant| t.elapsed() >= *min_interval);
                                if due {
                                    let _ = tx.send(Some(frame.clone()));
                                    last_preview = Some(std::time::Instant::now());
                                }
                            }
                            // Seed-BSR-G1-02-11 / G2-04-11: push to DropOldest
                            // ring buffer; wake the encoder thread.
                            let dropped = {
                                let mut buf = self.frame_buf.lock().unwrap();
                                buf.push(frame)
                            };
                            if dropped {
                                self.telemetry.frames_dropped += 1;
                                let total = {
                                    self.frame_buf.lock().unwrap().dropped_count()
                                };
                                warn!("Ring buffer full — dropped oldest frame; total dropped: {}", total);
                                // Seed-BSR-G1-04-11: emit telemetry, do not crash.
                                let _ = self.telemetry_tx.send(
                                    TelemetryEvent::ErrorOccurred {
                                        message: format!("Frame dropped by ring buffer (total: {})", total),
                                    },
                                );
                            }
                            self.frame_notify.notify_one();
                            info!("Captured frame: {}x{}", {
                                self.frame_buf.lock().unwrap().len()
                            }, 1);
                        }
                        Err(e) => {
                            // Seed-BSR-G1-04-11: log capture errors, never crash.
                            warn!("Frame capture failed: {}", e);
                            self.telemetry.frames_dropped += 1;
                        }
                    }
                }
                _ = self.shutdown_rx.recv() => {
                    info!("Capture service shutting down");
                    self.backend.shutdown().await?;
                    break;
                }
            }
        }

        Ok(())
    }
}

#[cfg(windows)]
pub mod dxgi_backend {
    use super::*;
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory1, IDXGIFactory1, IDXGIOutput1, IDXGIOutputDuplication,
        IDXGIResource, DXGI_OUTDUPL_FRAME_INFO, DXGI_OUTPUT_DESC,
    };
    use windows::Win32::Graphics::Direct3D11::{
        D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11Texture2D,
        D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_CPU_ACCESS_READ,
        D3D11_MAPPED_SUBRESOURCE, D3D11_MAP_READ, D3D11_SDK_VERSION,
        D3D11_TEXTURE2D_DESC, D3D11_USAGE_STAGING,
    };
    use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
    use windows::Win32::Graphics::Dxgi::Common::{
        DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC,
    };
    use windows::core::Interface;

    pub struct DxgiCaptureBackend {
        device: Option<ID3D11Device>,
        context: Option<ID3D11DeviceContext>,
        duplication: Option<IDXGIOutputDuplication>,
        staging_texture: Option<ID3D11Texture2D>,
        width: u32,
        height: u32,
    }

    impl DxgiCaptureBackend {
        pub fn new() -> Self {
            Self {
                device: None,
                context: None,
                duplication: None,
                staging_texture: None,
                width: 0,
                height: 0,
            }
        }
    }

    #[async_trait::async_trait]
    impl CaptureBackend for DxgiCaptureBackend {
        async fn initialize(&mut self) -> Result<(), CaptureError> {
            unsafe {
                // Create DXGI Factory
                let factory: IDXGIFactory1 = CreateDXGIFactory1()
                    .map_err(|e| CaptureError::InitializationFailed(format!("CreateDXGIFactory1: {}", e)))?;

                // Get first adapter (primary GPU)
                let adapter = factory.EnumAdapters1(0)
                    .map_err(|e| CaptureError::InitializationFailed(format!("EnumAdapters1: {}", e)))?;

                // Create D3D11 device — use HARDWARE with None adapter to let
                // Windows pick the adapter automatically (avoids IDXGIAdapter1 → IDXGIAdapter cast issues)
                let mut device = None;
                let mut context = None;
                D3D11CreateDevice(
                    None,
                    D3D_DRIVER_TYPE_HARDWARE,
                    None,
                    D3D11_CREATE_DEVICE_BGRA_SUPPORT,
                    None,
                    D3D11_SDK_VERSION,
                    Some(&mut device),
                    None,
                    Some(&mut context),
                ).map_err(|e| CaptureError::InitializationFailed(format!("D3D11CreateDevice: {}", e)))?;

                let device = device
                    .ok_or_else(|| CaptureError::InitializationFailed("D3D11 device is None".into()))?;
                let context = context
                    .ok_or_else(|| CaptureError::InitializationFailed("D3D11 context is None".into()))?;

                // Get first output (primary monitor) from adapter
                let output = adapter.EnumOutputs(0)
                    .map_err(|e| CaptureError::InitializationFailed(format!("EnumOutputs: {}", e)))?;

                // Get output resolution
                let mut desc = DXGI_OUTPUT_DESC::default();
                output.GetDesc(&mut desc)
                    .map_err(|e| CaptureError::InitializationFailed(format!("GetDesc: {}", e)))?;
                let width = (desc.DesktopCoordinates.right - desc.DesktopCoordinates.left) as u32;
                let height = (desc.DesktopCoordinates.bottom - desc.DesktopCoordinates.top) as u32;

                // Cast to IDXGIOutput1 for Desktop Duplication
                let output1: IDXGIOutput1 = output.cast()
                    .map_err(|e| CaptureError::InitializationFailed(format!("Cast IDXGIOutput1: {}", e)))?;

                // Create output duplication
                let duplication = output1.DuplicateOutput(&device)
                    .map_err(|e| CaptureError::InitializationFailed(format!("DuplicateOutput: {}", e)))?;

                // Create CPU-readable staging texture
                let staging_desc = D3D11_TEXTURE2D_DESC {
                    Width: width,
                    Height: height,
                    MipLevels: 1,
                    ArraySize: 1,
                    Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Usage: D3D11_USAGE_STAGING,
                    BindFlags: 0,
                    CPUAccessFlags: D3D11_CPU_ACCESS_READ.0 as u32,
                    MiscFlags: 0,
                };

                let mut staging: Option<ID3D11Texture2D> = None;
                device.CreateTexture2D(&staging_desc, None, Some(&mut staging))
                    .map_err(|e| CaptureError::InitializationFailed(format!("CreateTexture2D: {}", e)))?;
                let staging = staging
                    .ok_or_else(|| CaptureError::InitializationFailed("Staging texture is None".into()))?;

                self.device = Some(device);
                self.context = Some(context);
                self.duplication = Some(duplication);
                self.staging_texture = Some(staging);
                self.width = width;
                self.height = height;

                info!("DXGI backend initialized: {}x{}", width, height);
                Ok(())
            }
        }

        async fn capture_frame(&mut self) -> Result<CaptureFrame, CaptureError> {
            let duplication = self.duplication.as_ref()
                .ok_or_else(|| CaptureError::FrameAcquisitionFailed("Not initialized".into()))?;
            let context = self.context.as_ref()
                .ok_or_else(|| CaptureError::FrameAcquisitionFailed("No device context".into()))?;
            let staging = self.staging_texture.as_ref()
                .ok_or_else(|| CaptureError::FrameAcquisitionFailed("No staging texture".into()))?;

            unsafe {
                // Acquire the next desktop frame (100ms timeout)
                let mut frame_info = DXGI_OUTDUPL_FRAME_INFO::default();
                let mut resource: Option<IDXGIResource> = None;

                duplication.AcquireNextFrame(100, &mut frame_info, &mut resource)
                    .map_err(|e| CaptureError::FrameAcquisitionFailed(format!("AcquireNextFrame: {}", e)))?;

                let resource = resource
                    .ok_or_else(|| CaptureError::FrameAcquisitionFailed("No desktop resource".into()))?;

                // Get the GPU texture from the resource
                let texture: ID3D11Texture2D = resource.cast()
                    .map_err(|e| CaptureError::FrameAcquisitionFailed(format!("Cast Texture2D: {}", e)))?;

                // Copy GPU texture → CPU staging texture
                context.CopyResource(staging, &texture);

                // Map staging texture for CPU read
                let mut mapped = D3D11_MAPPED_SUBRESOURCE::default();
                context.Map(staging, 0, D3D11_MAP_READ, 0, Some(&mut mapped))
                    .map_err(|e| CaptureError::FrameAcquisitionFailed(format!("Map: {}", e)))?;

                // Copy BGRA pixel rows (accounting for row pitch padding)
                let row_pitch = mapped.RowPitch as usize;
                let w = self.width as usize;
                let h = self.height as usize;
                let mut data = vec![0u8; w * h * 4];

                let src = std::slice::from_raw_parts(mapped.pData as *const u8, row_pitch * h);
                for y in 0..h {
                    let src_row = &src[y * row_pitch..y * row_pitch + w * 4];
                    let dst_row = &mut data[y * w * 4..(y + 1) * w * 4];
                    dst_row.copy_from_slice(src_row);
                }

                context.Unmap(staging, 0);

                // Release the duplicated frame
                duplication.ReleaseFrame()
                    .map_err(|e| CaptureError::FrameAcquisitionFailed(format!("ReleaseFrame: {}", e)))?;

                let timestamp = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos() as u64;

                Ok(CaptureFrame {
                    data,
                    timestamp,
                    width: self.width,
                    height: self.height,
                    format: FrameFormat::Bgra8,
                })
            }
        }

        async fn shutdown(&mut self) -> Result<(), CaptureError> {
            self.duplication = None;
            self.staging_texture = None;
            self.context = None;
            self.device = None;
            info!("DXGI backend shutdown");
            Ok(())
        }
    }
}

#[cfg(all(target_os = "linux", feature = "linux-capture"))]
pub mod portal_backend;

/// The real capture backend for the platform this binary was built for.
///
/// Every call site uses this rather than naming a backend directly. `bsr-ui` named
/// `dxgi_backend::DxgiCaptureBackend` with no `cfg` at all, which is why it could never
/// build on Linux; `bsr-smoke` and `bsr-real-smoke` each repeated their own `cfg` pair
/// and each picked the mock. One alias means a new platform is added in one place and
/// no call site can pick a backend that is not real.
#[cfg(windows)]
pub type PlatformCaptureBackend = dxgi_backend::DxgiCaptureBackend;

/// The real capture backend for the platform this binary was built for.
#[cfg(all(target_os = "linux", feature = "linux-capture"))]
pub type PlatformCaptureBackend = portal_backend::PortalCaptureBackend;

/// Construct the platform's real capture backend.
///
/// Infallible by design, matching `DxgiCaptureBackend::new()`: acquiring the screen is
/// `CaptureBackend::initialize`'s job, and that is where failure is reported. It never
/// returns a synthetic source — on a box with no portal, `initialize` fails and the
/// recording does not start.
pub fn platform_backend() -> PlatformCaptureBackend {
    PlatformCaptureBackend::new()
}

/// Synthetic frame source. **Test-only, and it must stay that way.**
///
/// It is gated on `cfg(test)`, not on a platform, so it cannot be compiled into a
/// product binary on any target. It exists to drive `CaptureService` deterministically
/// without a display; it is not a fallback, and nothing outside this crate's tests may
/// construct it. See the `compile_error!` at the top of this file for the reasoning.
#[cfg(test)]
mod mock_backend {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    pub struct MockCaptureBackend {
        frame_count: u64,
    }

    impl MockCaptureBackend {
        pub fn new() -> Self {
            Self { frame_count: 0 }
        }
    }

    #[async_trait::async_trait]
    impl CaptureBackend for MockCaptureBackend {
        async fn initialize(&mut self) -> Result<(), CaptureError> {
            info!("Mock backend initialized");
            Ok(())
        }

        async fn capture_frame(&mut self) -> Result<CaptureFrame, CaptureError> {
            self.frame_count += 1;
            let timestamp = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_nanos() as u64;

            // Generate synthetic frame: solid color with frame number
            let width = 1920;
            let height = 1080;
            let mut data = vec![0u8; (width * height * 4) as usize];
            for i in 0..data.len() / 4 {
                data[i * 4] = (self.frame_count % 256) as u8; // B
                data[i * 4 + 1] = 128; // G
                data[i * 4 + 2] = 128; // R
                data[i * 4 + 3] = 255; // A
            }

            Ok(CaptureFrame {
                data,
                timestamp,
                width,
                height,
                format: FrameFormat::Bgra8,
            })
        }

        async fn shutdown(&mut self) -> Result<(), CaptureError> {
            info!("Mock backend shutdown");
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(not(windows))]
    #[tokio::test]
    async fn test_mock_backend() {
        let mut backend = mock_backend::MockCaptureBackend::new();
        backend.initialize().await.unwrap();

        let frame = backend.capture_frame().await.unwrap();
        assert_eq!(frame.width, 1920);
        assert_eq!(frame.height, 1080);
        assert_eq!(frame.format, FrameFormat::Bgra8);
        assert!(!frame.data.is_empty());

        backend.shutdown().await.unwrap();
    }

    #[test]
    fn test_capture_frame_serialization() {
        let frame = CaptureFrame {
            data: vec![1, 2, 3, 4],
            timestamp: 123456789,
            width: 100,
            height: 100,
            format: FrameFormat::Bgra8,
        };
        let json = serde_json::to_string(&frame).unwrap();
        let deserialized: CaptureFrame = serde_json::from_str(&json).unwrap();
        assert_eq!(frame, deserialized);
    }

    #[cfg(not(windows))]
    #[tokio::test]
    async fn test_capture_service() {
        use std::sync::{Arc, Mutex};
        use tokio::sync::{broadcast, mpsc, Notify};
        use bsr_core::buffer::DropOldestBuffer;

        let backend = mock_backend::MockCaptureBackend::new();
        let config = CaptureConfig::default();
        let (telemetry_tx, _) = broadcast::channel(32);
        let (shutdown_tx, shutdown_rx) = mpsc::channel(1);
        let frame_buf = Arc::new(Mutex::new(DropOldestBuffer::new(config.buffer_capacity)));
        let frame_notify = Arc::new(Notify::new());

        let service = CaptureService::new(backend, config, telemetry_tx, shutdown_rx, frame_buf, frame_notify);

        // Spawn service
        let handle = tokio::spawn(async move {
            service.run().await.unwrap();
        });

        // Let it capture a few frames
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Shutdown
        shutdown_tx.send(()).await.unwrap();

        handle.await.unwrap();
    }
}
