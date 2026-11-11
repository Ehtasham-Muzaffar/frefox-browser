//! Application lifecycle — activation and open handlers.

use crate::browser;
use crate::config::FeroxConfig;
use crate::memory;
use crate::adblock;
use anyhow::Result;
use gtk4::prelude::*;
use tracing::info;

/// Called when the GTK application activates (first window open).
/// This is the main startup sequence:
///   1. Load configuration from disk
///   2. Initialize the global ad blocker
///   3. Schedule periodic GC
///   4. Create the main browser window
pub fn on_activate(app: &gtk4::Application) -> Result<()> {
    info!("Application activated");

    // Load configuration (auto-creates defaults on first run)
    let config = FeroxConfig::load();

    // Initialize the global Aho-Corasick ad/tracker blocker.
    // This must happen before any WebView is created so the content
    // filter can be applied to the WebContext.
    let custom_path = config.network.custom_blocklist_path.as_deref();
    adblock::init_global_blocker(custom_path)
        .unwrap_or_else(|e| tracing::warn!("AdBlocker init failed: {}", e));

    // Schedule periodic JavaScriptCore GC sweeps on the GTK main loop.
    memory::schedule_periodic_gc(config.memory.gc_interval_secs);

    // Create and show the main browser window.
    browser::create_browser_window(app, config)?;

    Ok(())
}

/// Called when the application is asked to open files/URIs from the
/// command line (e.g. `ferrox https://example.com`).
pub fn on_open(app: &gtk4::Application, uri: &str) {
    info!("Opening URI from command line: {}", uri);
    let config = FeroxConfig::load();
    if let Err(e) = browser::create_browser_window(app, config) {
        eprintln!("Failed to open window for {}: {}", uri, e);
    }
}

