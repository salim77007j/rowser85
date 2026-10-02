//! Rrowser85 — the native browser UI (egui/eframe).
//!
//! Design language: Material-flat hybrid per the reference mockups — light
//! chrome, pill omnibox, rounded tabs; dark and accent variants included.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod app;
mod chrome;
mod icons;
mod pages;
mod theme;

fn main() -> eframe::Result<()> {
    // Engine diagnostics: RROWSER_LOG="rowser=debug" (default info) writes to
    // stderr — invaluable for field debugging of load failures.
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_env("ROWSER_LOG")
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_target(false)
        .init();
    let profile_dir = profile_dir();
    let shell = match rowser_shell::Shell::start(&profile_dir) {
        Ok(shell) => shell,
        Err(err) => {
            eprintln!("rowser: failed to start engine: {err:#}");
            std::process::exit(1);
        }
    };

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([1360.0, 860.0])
            .with_min_inner_size([640.0, 480.0])
            .with_title("Rrowser85")
            .with_app_id("rowser85"),
        ..Default::default()
    };

    eframe::run_native(
        "rowser85",
        options,
        Box::new(move |cc| {
            // Engine events must wake the UI: install the repaint hook now.
            let ctx = cc.egui_ctx.clone();
            shell.set_waker(Box::new(move || ctx.request_repaint()));
            Ok(Box::new(app::BrowserApp::new(shell, cc)))
        }),
    )
}

fn profile_dir() -> std::path::PathBuf {
    dirs::data_local_dir()
        .map(|d| d.join("rowser85"))
        .or_else(|| dirs::home_dir().map(|h| h.join(".rowser85")))
        .unwrap_or_else(|| std::path::PathBuf::from(".rowser85"))
}
