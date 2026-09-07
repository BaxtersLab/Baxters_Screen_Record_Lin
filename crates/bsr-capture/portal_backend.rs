// SPDX-License-Identifier: MIT
//! Linux screen capture: xdg-desktop-portal ScreenCast -> PipeWire -> GStreamer.
//!
//! The Wayland-native sibling of the Windows DXGI backend in [`crate::dxgi_backend`].
//! It implements the same [`CaptureBackend`] trait and is selected by
//! [`crate::platform_backend`]; the trait itself is untouched.
//!
//! **Why not X11 / XGetImage / XShm.** Under a native Wayland session the X11 root
//! window contains nothing — a capture off it comes back solid black with no error.
//! This is measured, not theoretical: SOC Ultralight on this same box captured solid
//! black under X11 and logged nothing at all. There is no Xorg fallback to reach for
//! either; the compositor never draws the desktop into an X drawable. The portal is
//! not a workaround, it is the only sanctioned path. **Never set `GDK_BACKEND=x11`
//! for this application.**
//!
//! **Pixel format is BGRA, deliberately.** `bsr-encode`'s H.264 backend builds its
//! swscale context with `Pixel::BGRA` (`crates/bsr-encode/backends/h264.rs`), which is
//! what DXGI hands it on Windows. Asking GStreamer for BGRA here means the Linux
//! frames enter the encoder in exactly the layout the Windows frames do — no encoder
//! change, and no swapped red and blue channels.
//!
//! **Session lifetime.** A portal session dies with the D-Bus connection that created
//! it, so the connection is owned by a dedicated thread that outlives the handshake
//! and is torn down only on shutdown or drop.
//!
//! **The first run shows GNOME's screen-share picker.** `PersistMode::ExplicitlyRevoked`
//! makes the portal hand back a restore token, cached at
//! `$XDG_CACHE_HOME/bsr/screencast_restore_token`; later runs reconnect silently.

use std::io::Write;
use std::os::fd::{AsRawFd, OwnedFd};
use std::path::PathBuf;
use std::sync::mpsc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use gstreamer as gst;
use gstreamer::prelude::*;
use gstreamer_app as gst_app;
use gstreamer_video as gst_video;
use gstreamer_video::prelude::*;

use super::{CaptureBackend, CaptureError, CaptureFrame, FrameFormat};
use tracing::{info, warn};

/// How long to wait for the very first frame before declaring the backend unusable.
/// Generous: it covers the portal dialog being approved on a first run, format
/// negotiation, and a desktop that is not repainting.
/// How long `initialize` waits for the stream to prove itself with a first frame.
///
/// A live ScreenCast stream delivers its first buffer in well under a second. This was
/// 30 s, which is far too long to sit silent after the operator presses Record: in the
/// field a recording whose stream never produced anything simply looked like it was
/// recording for 13 s and then wrote an empty file, because the operator stopped long
/// before the timeout could fire and say what was wrong.
const FIRST_FRAME_TIMEOUT: Duration = Duration::from_secs(8);

/// Bound on how many queued buffers one `capture_frame` will drain to reach the newest.
const MAX_DRAIN: usize = 30;

/// Set `BSR_NO_PORTAL=1` to make the backend refuse to initialize. This exists so the
/// fail-closed path can be exercised by the test gate on a box where the portal *is*
/// available; it is not a fallback and enables no alternative frame source.
const ENV_NO_PORTAL: &str = "BSR_NO_PORTAL";

/// `embedded` (default), `hidden`, or `metadata`. A screen *recorder* wants the pointer
/// in the picture, which is the opposite of the sibling OCR automation in SOC Ultralight.
const ENV_CURSOR: &str = "BSR_CAPTURE_CURSOR";

/// Portal-side handshake results, handed back from the connection thread.
struct PortalStream {
    node_id: u32,
    fd: OwnedFd,
    size: Option<(i32, i32)>,
}

/// Owns the D-Bus connection thread. Dropping it closes the portal session.
struct PortalConnection {
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    /// Signalled by the session task just before it returns, so `drop` can wait a
    /// bounded time instead of blocking forever.
    exited: std::sync::Mutex<mpsc::Receiver<()>>,
}

impl Drop for PortalConnection {
    fn drop(&mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        // Bounded: the session task closes the portal session and signals. If it does
        // not, carry on rather than blocking the caller — a leaked session is released
        // when the process exits, whereas a blocked Stop strands the operator.
        let waited = self
            .exited
            .get_mut()
            .map(|rx| rx.recv_timeout(Duration::from_secs(3)))
            .unwrap_or(Ok(()));
        if waited.is_err() {
            warn!("portal session did not confirm close within 3s; continuing");
        }
    }
}

/// A live portal session plus the GStreamer pipeline reading it.
struct Live {
    // Field order is drop order, and it matters:
    //   pipeline (stops reading the fd) -> fd -> portal connection.
    // Closing the fd or the session while pipewiresrc still holds it is a use after close.
    pipeline: gst::Pipeline,
    appsink: gst_app::AppSink,
    _fd: OwnedFd,
    _connection: PortalConnection,
    node_id: u32,
    width: u32,
    height: u32,
    /// The most recent real frame. A PipeWire screencast stream is **damage-driven**:
    /// a desktop that has not repainted produces no buffers while still looking exactly
    /// like its last frame. Re-emitting it is what keeps a recording at a constant frame
    /// rate. This is the real screen, held, not a synthesised picture.
    last: Option<CaptureFrame>,
}

/// Persistent portal + PipeWire capturer for one monitor.
pub struct PortalCaptureBackend {
    live: Option<Live>,
    node_id: Option<u32>,
    /// Frames served by repeating `last` because the desktop did not repaint.
    repeated_frames: u64,
}

impl Default for PortalCaptureBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl PortalCaptureBackend {
    pub fn new() -> Self {
        Self { live: None, node_id: None, repeated_frames: 0 }
    }

    /// Frame size negotiated with the stream, available after `initialize`.
    pub fn dimensions(&self) -> Option<(u32, u32)> {
        self.live.as_ref().map(|l| (l.width, l.height))
    }

    /// PipeWire node backing the stream, available after `initialize`.
    pub fn node_id(&self) -> Option<u32> {
        self.node_id
    }

    /// How many frames have been served by repeating the last real frame because the
    /// desktop did not repaint. Distinguishes "static desktop" from "stream stalled"
    /// in telemetry, which a caller otherwise cannot tell apart.
    pub fn repeated_frames(&self) -> u64 {
        self.repeated_frames
    }
}

#[async_trait::async_trait]
impl CaptureBackend for PortalCaptureBackend {
    async fn initialize(&mut self) -> Result<(), CaptureError> {
        if self.live.is_some() {
            return Ok(());
        }
        // The handshake blocks on D-Bus and then on the first frame (up to
        // FIRST_FRAME_TIMEOUT). Doing that on a tokio worker thread would stall
        // every other task on the runtime, so it goes to the blocking pool.
        let live = tokio::task::spawn_blocking(start_live)
            .await
            .map_err(|e| CaptureError::InitializationFailed(format!("portal init task: {e}")))??;

        info!(
            node = live.node_id,
            width = live.width,
            height = live.height,
            "Portal/PipeWire capture live"
        );
        self.node_id = Some(live.node_id);
        self.live = Some(live);
        Ok(())
    }

    async fn capture_frame(&mut self) -> Result<CaptureFrame, CaptureError> {
        let live = self.live.as_mut().ok_or_else(|| {
            CaptureError::FrameAcquisitionFailed(
                "capture_frame called before initialize — no portal session".into(),
            )
        })?;

        // Drain whatever is queued so we act on the NEWEST frame, not the oldest.
        // A zero timeout only reports what is already waiting, so this does not block.
        let mut newest = None;
        for _ in 0..MAX_DRAIN {
            match pull_frame(&live.appsink, Duration::ZERO) {
                Ok(Some(f)) => newest = Some(f),
                Ok(None) => break,
                Err(e) => {
                    // A malformed sample is worth saying out loud rather than silently
                    // reading as a static desktop, which would hide a real fault.
                    warn!("dropping unusable PipeWire sample: {e}");
                    break;
                }
            }
        }

        if let Some(frame) = newest {
            live.width = frame.width;
            live.height = frame.height;
            live.last = Some(frame.clone());
            return Ok(frame);
        }

        // Nothing new: the desktop has not repainted. Re-emit the last real frame with
        // a current timestamp so the recording keeps its frame rate.
        match &live.last {
            Some(prev) => {
                self.repeated_frames += 1;
                Ok(CaptureFrame { timestamp: now_nanos(), ..prev.clone() })
            }
            None => Err(CaptureError::FrameAcquisitionFailed(
                "no frame available and no previous frame to repeat".into(),
            )),
        }
    }

    async fn shutdown(&mut self) -> Result<(), CaptureError> {
        if let Some(live) = self.live.take() {
            // Stop the pipeline before the fd and the session go away.
            let _ = live.pipeline.set_state(gst::State::Null);
            drop(live);
        }
        self.node_id = None;
        info!("Portal/PipeWire backend shutdown");
        Ok(())
    }
}

fn now_nanos() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos() as u64)
        .unwrap_or(0)
}

/// Build a live portal session and prove it produces a frame.
///
/// Returns `Err` rather than a silently-empty capturer. A backend that reports success
/// and then delivers nothing — or worse, delivers something it made up — is the exact
/// failure this module exists to prevent.
fn start_live() -> Result<Live, CaptureError> {
    let init = |m: String| CaptureError::InitializationFailed(m);

    if std::env::var(ENV_NO_PORTAL).as_deref() == Ok("1") {
        return Err(init(format!("{ENV_NO_PORTAL}=1 set — refusing to capture")));
    }
    if std::env::var_os("WAYLAND_DISPLAY").is_none() && std::env::var_os("DISPLAY").is_none() {
        return Err(init(
            "no graphical session (WAYLAND_DISPLAY and DISPLAY are both unset)".into(),
        ));
    }
    // Refuse an Xorg session outright rather than recording a black rectangle.
    //
    // Under X11 there is no ScreenCast portal path that gives us the desktop; an X11 grab
    // of the root window comes back solid black with no error at all — measured on this
    // estate, and the reason `GDK_BACKEND=x11` must never be set for this app. Failing
    // here is strictly better than producing a file full of nothing and reporting success.
    if std::env::var("XDG_SESSION_TYPE").as_deref() == Ok("x11") {
        return Err(init(
            "this is an Xorg (X11) session — BSR needs Wayland. Under X11 the desktop \
             cannot be captured through the portal and a root-window grab returns solid \
             black, so recording is refused rather than producing an empty file. \
             Log in with a Wayland session (Ubuntu 26.04 and later default to it)."
                .into(),
        ));
    }

    info!("capture start: environment ok, opening portal session");
    let (stream, connection) = start_portal_session().map_err(init)?;
    info!(node = stream.node_id, "capture start: portal session granted");

    info!("capture start: initialising GStreamer");
    gst::init().map_err(|e| init(format!("gst::init: {e}")))?;
    info!("capture start: GStreamer ready, building pipeline");

    // BGRA to match what the H.264 backend's swscale context expects (see module docs).
    //
    // drop=true is not a tuning knob. With drop=false the appsink queue fills,
    // back-pressures, and PipeWire STOPS PRODUCING: the stream delivers one burst and
    // then freezes forever, which reads as a frozen desktop rather than as an error.
    let desc = format!(
        "pipewiresrc name=src fd={fd} path={node} ! videoconvert ! video/x-raw,format=BGRA ! \
         appsink name=sink max-buffers=4 drop=true sync=false",
        fd = stream.fd.as_raw_fd(),
        node = stream.node_id,
    );

    let element = gst::parse::launch(&desc).map_err(|e| init(format!("gst parse ({desc}): {e}")))?;
    let pipeline = element
        .downcast::<gst::Pipeline>()
        .map_err(|_| init("parsed element is not a pipeline".into()))?;
    let appsink = pipeline
        .by_name("sink")
        .ok_or_else(|| init("pipeline has no element named 'sink'".into()))?
        .downcast::<gst_app::AppSink>()
        .map_err(|_| init("'sink' is not an appsink".into()))?;

    pipeline
        .set_state(gst::State::Playing)
        .map_err(|e| init(format!("set pipeline PLAYING: {e}")))?;

    // The first frame is mandatory — it is what proves the stream is live, and its caps
    // are the authoritative frame size (the portal's advertised size can be the logical,
    // pre-scaling one).
    let first = match pull_frame(&appsink, FIRST_FRAME_TIMEOUT) {
        Ok(Some(f)) => f,
        Ok(None) => {
            let _ = pipeline.set_state(gst::State::Null);
            return Err(init(format!(
                "portal session started (node {}) but no frame arrived within {:?} — the \
                 grant may have been cancelled or the stream revoked",
                stream.node_id, FIRST_FRAME_TIMEOUT
            )));
        }
        Err(e) => {
            let _ = pipeline.set_state(gst::State::Null);
            return Err(init(format!("first frame unusable: {e}")));
        }
    };

    let (width, height) = (first.width, first.height);
    let fallback = stream.size.map(|(w, h)| (w.max(0) as u32, h.max(0) as u32));
    if let Some((fw, fh)) = fallback {
        if (fw, fh) != (width, height) && fw > 0 && fh > 0 {
            // Not an error — the portal advertises logical size, the caps carry physical.
            info!(
                portal_w = fw, portal_h = fh, caps_w = width, caps_h = height,
                "portal advertised a different size than the negotiated caps; using caps"
            );
        }
    }

    Ok(Live {
        pipeline,
        appsink,
        _fd: stream.fd,
        _connection: connection,
        node_id: stream.node_id,
        width,
        height,
        last: Some(first),
    })
}

/// Pull one sample and convert it. `Ok(None)` means "nothing queued within `timeout`".
fn pull_frame(
    appsink: &gst_app::AppSink,
    timeout: Duration,
) -> Result<Option<CaptureFrame>, String> {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let clock = gst::ClockTime::from_nseconds(remaining.as_nanos() as u64);
        let Some(sample) = appsink.try_pull_sample(clock) else {
            return Ok(None);
        };
        match sample_to_frame(&sample) {
            Ok(frame) => return Ok(Some(frame)),
            Err(e) if Instant::now() >= deadline => return Err(e),
            Err(e) => warn!("skipping unusable sample: {e}"),
        }
    }
}

/// Copy one GStreamer sample into a tightly packed BGRA [`CaptureFrame`].
///
/// The copy is per row because GStreamer pads rows to its own stride, which is not
/// necessarily `width * 4`. Reading `width * height * 4` bytes straight out of the
/// mapped buffer produces a picture that shears progressively down the frame.
fn sample_to_frame(sample: &gst::Sample) -> Result<CaptureFrame, String> {
    let caps = sample.caps().ok_or("sample carries no caps")?;
    let info =
        gst_video::VideoInfo::from_caps(caps).map_err(|e| format!("VideoInfo::from_caps: {e}"))?;
    let buffer = sample.buffer().ok_or("sample carries no buffer")?;
    let frame = gst_video::VideoFrameRef::from_buffer_ref_readable(buffer, &info)
        .map_err(|e| format!("map video frame: {e}"))?;

    let w = frame.width() as usize;
    let h = frame.height() as usize;
    if w == 0 || h == 0 {
        return Err(format!("degenerate frame size {w}x{h}"));
    }
    let stride = frame.plane_stride()[0] as usize;
    let src = frame.plane_data(0).map_err(|e| format!("plane_data(0): {e}"))?;
    let row = w * 4;
    if stride < row {
        return Err(format!("stride {stride} shorter than a {row}-byte row"));
    }

    let mut data = vec![0u8; row * h];
    for y in 0..h {
        let s = y * stride;
        if s + row > src.len() {
            return Err(format!(
                "buffer is {} bytes, short of row {y} at offset {}",
                src.len(),
                s + row
            ));
        }
        data[y * row..(y + 1) * row].copy_from_slice(&src[s..s + row]);
    }

    Ok(CaptureFrame {
        data,
        timestamp: now_nanos(),
        width: w as u32,
        height: h as u32,
        format: FrameFormat::Bgra8,
    })
}

/// Where the ScreenCast restore token is cached. Its presence is what makes every run
/// after the first one silent.
fn restore_token_path() -> PathBuf {
    let base = std::env::var_os("XDG_CACHE_HOME")
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))
        .unwrap_or_else(|| PathBuf::from("/tmp"));
    base.join("bsr").join("screencast_restore_token")
}

fn read_restore_token() -> Option<String> {
    let s = std::fs::read_to_string(restore_token_path()).ok()?;
    let s = s.trim().to_string();
    if s.is_empty() { None } else { Some(s) }
}

fn write_restore_token(token: &str) {
    let path = restore_token_path();
    let Some(dir) = path.parent() else { return };
    if std::fs::create_dir_all(dir).is_err() {
        return;
    }
    // 0600: the token is a standing grant to capture this desktop.
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create(true).truncate(true);
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    if let Ok(mut f) = opts.open(&path) {
        let _ = f.write_all(token.as_bytes());
    }
}

fn cursor_mode() -> ashpd::desktop::screencast::CursorMode {
    use ashpd::desktop::screencast::CursorMode;
    match std::env::var(ENV_CURSOR).as_deref() {
        Ok("hidden") => CursorMode::Hidden,
        Ok("metadata") => CursorMode::Metadata,
        // A screen recording that loses the pointer loses half of what it was recording.
        _ => CursorMode::Embedded,
    }
}

/// Run the ScreenCast handshake on a dedicated thread and keep that thread — and
/// therefore the D-Bus connection and the portal session — alive until the returned
/// [`PortalConnection`] is dropped.
/// The one portal runtime for this process.
///
/// zbus caches the session-bus connection **process-wide**, and that connection is
/// driven by whichever executor first created it. The previous code built a fresh
/// `current_thread` runtime on a per-session thread and let that thread exit when the
/// session was shut down, which left the cached connection with no executor to drive
/// it: the *next* `Screencast::new()` blocked forever and BSR recorded nothing, with
/// no error anywhere.
///
/// Measured with `examples/two_sessions.rs`: session A reaches "proxy ready" in 5.8 ms;
/// session B, opened after A was closed, never returns from `Screencast::new()`. This
/// is why the recording produced an empty file whenever the idle live view had been
/// used first, and why it worked when it had not.
///
/// Never dropped, so the connection always has somewhere to run.
fn portal_runtime() -> &'static tokio::runtime::Runtime {
    static RT: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RT.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .thread_name("bsr-portal")
            .enable_all()
            .build()
            .expect("build the portal runtime")
    })
}

/// How long to wait for the portal to answer a handshake before giving up. The portal
/// may show a picker dialog, so this has to allow for a person reading it.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(120);

/// Run the ScreenCast handshake on the shared portal runtime and keep the session
/// alive until the returned [`PortalConnection`] is dropped.
fn start_portal_session() -> Result<(PortalStream, PortalConnection), String> {
    let (ready_tx, ready_rx) = mpsc::channel::<Result<PortalStream, String>>();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let (exited_tx, exited_rx) = mpsc::channel::<()>();

    info!("portal: dispatching handshake to the shared portal runtime");
    portal_runtime().spawn(async move {
        // Fires on every exit path, so a dropped PortalConnection never waits on a
        // task that has already gone.
        struct ExitSignal(mpsc::Sender<()>);
        impl Drop for ExitSignal {
            fn drop(&mut self) {
                let _ = self.0.send(());
            }
        }
        let _exit = ExitSignal(exited_tx);

        match handshake().await {
            Ok((session, stream)) => {
                if ready_tx.send(Ok(stream)).is_err() {
                    let _ = session.close().await;
                    return;
                }
                // Hold the session open until the owner drops.
                let _ = shutdown_rx.await;
                let _ = tokio::time::timeout(Duration::from_secs(5), session.close()).await;
            }
            Err(e) => {
                let _ = ready_tx.send(Err(e));
            }
        }
    });

    info!("portal: waiting for handshake result");
    let stream = match ready_rx.recv_timeout(HANDSHAKE_TIMEOUT) {
        Ok(Ok(s)) => s,
        Ok(Err(e)) => return Err(e),
        Err(mpsc::RecvTimeoutError::Timeout) => {
            return Err(format!(
                "the desktop portal did not answer within {HANDSHAKE_TIMEOUT:?} — the \
                 screen-share dialog may be waiting for a response, or the portal may \
                 not be running (check xdg-desktop-portal-gnome)"
            ))
        }
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            return Err("portal handshake task exited without answering".into())
        }
    };

    Ok((stream, PortalConnection { shutdown: Some(shutdown_tx), exited: std::sync::Mutex::new(exited_rx) }))
}

async fn handshake() -> Result<
    (ashpd::desktop::Session<ashpd::desktop::screencast::Screencast>, PortalStream),
    String,
> {
    use ashpd::desktop::screencast::{
        Screencast, SelectSourcesOptions, SourceType, StartCastOptions,
    };
    use ashpd::desktop::{CreateSessionOptions, PersistMode};

    info!("handshake: connecting to ScreenCast portal");
    let proxy = Screencast::new()
        .await
        .map_err(|e| format!("connect to ScreenCast portal: {e}"))?;
    info!("handshake: proxy ready, creating session");
    let session = proxy
        .create_session(CreateSessionOptions::default())
        .await
        .map_err(|e| format!("ScreenCast.CreateSession: {e}"))?;

    // The restore token encodes WHAT WAS PREVIOUSLY GRANTED. Feeding back a token issued
    // for a different source type makes the portal silently restore the old grant and
    // ignore the types asked for here.
    let restore = read_restore_token();
    let mut opts = SelectSourcesOptions::default()
        .set_multiple(false)
        .set_cursor_mode(cursor_mode())
        .set_sources(ashpd::enumflags2::BitFlags::from(SourceType::Monitor))
        .set_persist_mode(PersistMode::ExplicitlyRevoked);
    if let Some(tok) = restore.as_deref() {
        opts = opts.set_restore_token(tok);
    }
    info!("handshake: session created, selecting sources");
    proxy
        .select_sources(&session, opts)
        .await
        .map_err(|e| format!("ScreenCast.SelectSources: {e}"))?
        .response()
        .map_err(|e| format!("SelectSources refused: {e}"))?;

    info!("handshake: sources selected, starting cast");
    let streams = proxy
        .start(&session, None, StartCastOptions::default())
        .await
        .map_err(|e| format!("ScreenCast.Start: {e}"))?
        .response()
        .map_err(|e| {
            format!("screen share was not granted ({e}) — the portal dialog may have been dismissed")
        })?;

    if let Some(tok) = streams.restore_token() {
        write_restore_token(tok);
    }

    let stream = streams
        .streams()
        .first()
        .ok_or_else(|| "ScreenCast.Start returned no streams".to_string())?;
    let node_id = stream.pipe_wire_node_id();
    let size = stream.size();

    let fd = proxy
        .open_pipe_wire_remote(&session, Default::default())
        .await
        .map_err(|e| format!("ScreenCast.OpenPipeWireRemote: {e}"))?;

    Ok((session, PortalStream { node_id, fd, size }))
}
