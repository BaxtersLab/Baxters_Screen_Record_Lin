// SPDX-License-Identifier: MIT
// Baxter's Screen Record — Main Binary
// Entry point for the packaged application

use bsr_core::app::SingleInstanceGuard;
use bsr_core::config::{ensure_config_exists, load_config_from_default_path, BsrConfig};
use bsr_ui::AppWindow;
use eframe::egui;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize logging
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    tracing::info!("Starting Baxter's Screen Record");

    // Prevent multiple instances (single source of truth in bsr-core).
    let _lock = match SingleInstanceGuard::new() {
        Ok(guard) => guard,
        Err(e) => {
            tracing::error!("{e}");
            eprintln!("Baxter's Screen Record is already running.");
            return Ok(());
        }
    };

    // Ensure config file exists and load it
    let _config_path = ensure_config_exists().ok();

    let config = match load_config_from_default_path() {
        Ok(config) => {
            tracing::info!("Loaded configuration successfully");
            config
        }
        Err(e) => {
            tracing::warn!("Failed to load config, using defaults: {}", e);
            BsrConfig::default()
        }
    };

    // Get tokio handle for pipeline spawning
    let handle = tokio::runtime::Handle::current();

    // Decode splash/icon PNG embedded at compile time.
    const SPLASH_PNG: &[u8] = include_bytes!("../../assets/bsr 512x512.png");

    let mut splash_rgba: Option<(u32, u32, Vec<u8>)> = None;
    let icon = match image::load_from_memory(SPLASH_PNG) {
        Ok(img) => {
            let rgba_img = img.to_rgba8();
            let (w, h) = rgba_img.dimensions();
            let full_rgba = rgba_img.into_raw();
            splash_rgba = Some((w, h, full_rgba.clone()));
            let icon_img = image::imageops::resize(
                &image::RgbaImage::from_raw(w, h, full_rgba).unwrap(),
                64, 64,
                image::imageops::FilterType::Lanczos3,
            );
            egui::IconData { rgba: icon_img.into_raw(), width: 64, height: 64 }
        }
        Err(_) => egui::IconData { rgba: vec![0u8; 32 * 32 * 4], width: 32, height: 32 },
    };

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            // Tall enough that the status, controls and Record space section are all
            // on screen at once. The panel scrolls now, so this is a comfort default
            // rather than a limit.
            .with_inner_size([520.0, 680.0])
            .with_min_inner_size([400.0, 300.0])
            .with_title("Baxter's Screen Record")
            // The Wayland app_id, and it is what makes the launcher's icon appear.
            //
            // Under a native Wayland session GNOME pairs a window with its .desktop entry
            // by matching this string against the entry's FILENAME. Without it, winit
            // falls back to a default and the Shell has nothing to match, so the dock and
            // Alt-Tab show a generic placeholder no matter how many icons are installed.
            //
            // It must stay equal to the basename of
            // `packaging/baxters-screen-record.desktop`, and to the `Icon=` key inside it.
            // (`StartupWMClass` in that file covers the XWayland case; BSR does not use
            // XWayland — it captures the screen — but the key is harmless and correct.)
            .with_app_id("baxters-screen-record")
            .with_icon(std::sync::Arc::new(icon)),
        ..Default::default()
    };

    // Create IPC server with consent channel and forward consent requests to UI queue
    let (server, client, _telemetry_tx, mut consent_rx) = bsr_ipc::IpcServer::new_with_consent();

    // Spawn IPC server
    tokio::spawn(async move {
        server.run().await;
    });

    // Forward consent requests from async channel into a UI-visible global queue
    tokio::spawn(async move {
        while let Some(req) = consent_rx.recv().await {
            bsr_ipc::push_consent_request_to_ui(req);
        }
    });

    eframe::run_native(
        "Baxter's Screen Record",
        options,
        Box::new(move |cc| {
            Box::new(AppWindow::new_with_splash(cc, config, handle, splash_rgba, Some(client)))
        }),
    )
    .map_err(|e| format!("Failed to run UI: {}", e))?;

    Ok(())
}