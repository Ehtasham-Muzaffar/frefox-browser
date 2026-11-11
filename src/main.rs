#![cfg_attr(all(target_os = "windows", not(debug_assertions)), windows_subsystem = "windows")]

//! Ferrox Browser — Entry Point
//!
//! Applies OS/engine memory limits before initializing the platform shell:
//!   - Linux: GTK4 + WebKitGTK (`webkit6`)
//!   - Windows: Tao + Wry (Edge WebView2)

#[cfg(target_os = "linux")]
mod app;
#[cfg(target_os = "linux")]
mod browser;
#[cfg(target_os = "linux")]
mod devtools;
#[cfg(target_os = "windows")]
mod win_browser;

mod adblock;
mod config;
mod memory;
mod network;
mod storage;
mod tabs;

use anyhow::Result;
use tracing::info;

#[cfg(target_os = "linux")]
use gtk4::prelude::*;

#[cfg(target_os = "linux")]
const APP_ID: &str = "io.ferrox.browser";

#[cfg(target_os = "windows")]
fn init_windows_logging() {
    let log_path = dirs::config_dir()
        .unwrap_or_else(|| std::path::PathBuf::from("."))
        .join("ferrox")
        .join("ferrox.log");
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }

    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| "ferrox=info,warn".parse().unwrap());

    if let Ok(file) = std::fs::OpenOptions::new()
        .create(true)
        .write(true)
        .truncate(true)
        .open(&log_path)
    {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .with_ansi(false)
            .with_writer(std::sync::Mutex::new(file))
            .compact()
            .init();
    } else {
        tracing_subscriber::fmt()
            .with_env_filter(filter)
            .compact()
            .init();
    }

    let panic_log = log_path.clone();
    std::panic::set_hook(Box::new(move |info| {
        let msg = format!("Ferrox panic: {info}\n");
        let _ = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&panic_log)
            .and_then(|mut f| std::io::Write::write_all(&mut f, msg.as_bytes()));
    }));
}

#[cfg(target_os = "windows")]
fn show_windows_error(msg: &str) {
    #[link(name = "user32")]
    extern "system" {
        fn MessageBoxW(hwnd: isize, text: *const u16, caption: *const u16, utype: u32) -> i32;
    }
    let text: Vec<u16> = msg.encode_utf16().chain(std::iter::once(0)).collect();
    let title: Vec<u16> = "Ferrox Browser Error\0".encode_utf16().collect();
    unsafe {
        MessageBoxW(0, text.as_ptr(), title.as_ptr(), 0x10);
    }
}

fn main() -> Result<()> {
    #[cfg(not(target_os = "windows"))]
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "ferrox=info,warn".parse().unwrap()),
        )
        .compact()
        .init();

    #[cfg(target_os = "windows")]
    init_windows_logging();

    info!("Ferrox Browser starting — pid={}", std::process::id());

    // Apply OS-level memory limits before any renderer is allocated
    memory::apply_process_limits();

    #[cfg(target_os = "linux")]
    {
        let app = gtk4::Application::builder()
            .application_id(APP_ID)
            .flags(gio::ApplicationFlags::HANDLES_OPEN)
            .build();

        app.connect_activate(|app| {
            if let Err(e) = app::on_activate(app) {
                eprintln!("Fatal activation error: {e:#}");
                std::process::exit(1);
            }
        });

        app.connect_open(|app, files, _hint| {
            if let Some(file) = files.first() {
                let uri = file.uri().to_string();
                app::on_open(app, &uri);
            }
        });

        let exit_code = app.run();
        info!("Ferrox Browser exiting with code {:?}", exit_code);
        Ok(())
    }

    #[cfg(target_os = "windows")]
    {
        let cfg = config::FeroxConfig::load();
        let custom_path = cfg.network.custom_blocklist_path.as_deref();
        if let Err(e) = adblock::init_global_blocker(custom_path) {
            tracing::warn!("AdBlocker init failed: {e}");
        }
        if let Err(e) = win_browser::run_windows_browser(cfg) {
            tracing::error!("Fatal startup error: {e:#}");
            show_windows_error(&format!("Ferrox failed to start:\n\n{e:#}"));
            return Err(e);
        }
        Ok(())
    }
}
