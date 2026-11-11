//! Configuration management — loads from ~/.config/ferrox/config.toml
//! and provides runtime defaults for memory, tabs, and network settings.

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tracing::warn;

/// Top-level browser configuration
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FeroxConfig {
    pub memory: MemoryConfig,
    pub tabs: TabConfig,
    pub network: NetworkConfig,
    pub ui: UiConfig,
    pub devtools: DevToolsConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryConfig {
    /// Maximum heap size per WebKit web process (MB)
    pub max_webkit_heap_mb: u64,
    /// Disk cache limit (MB)
    pub disk_cache_limit_mb: u64,
    /// Memory cache limit (MB)
    pub memory_cache_limit_mb: u64,
    /// Interval in seconds to run garbage collection sweep
    pub gc_interval_secs: u64,
    /// Interval in seconds to purge stale image caches
    pub image_cache_purge_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TabConfig {
    /// Seconds of inactivity before a background tab is suspended
    pub suspend_after_secs: u64,
    /// Maximum number of live (non-suspended) tabs before forced suspension
    pub max_live_tabs: usize,
    /// Directory to serialize suspended tab state
    pub suspend_cache_dir: Option<PathBuf>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NetworkConfig {
    /// Enable built-in ad/tracker blocking
    pub adblock_enabled: bool,
    /// Path to custom blocklist file (one pattern per line)
    pub custom_blocklist_path: Option<PathBuf>,
    /// Enable HTTPS-only mode (redirect HTTP to HTTPS)
    pub https_only: bool,
    /// Enable DNS-over-HTTPS
    pub dns_over_https: bool,
    /// DoH resolver URL
    pub doh_url: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UiConfig {
    /// Enable hardware acceleration in WebKit
    pub hardware_acceleration: bool,
    /// Default window width
    pub window_width: i32,
    /// Default window height
    pub window_height: i32,
    /// Default homepage URL
    pub homepage: String,
    /// Enable dark mode
    pub dark_mode: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DevToolsConfig {
    /// Enable the built-in DevTools panel
    pub enabled: bool,
    /// Default panel height when docked (pixels)
    pub panel_height: i32,
    /// Enable remote debugging on a local port (0 = disabled)
    pub remote_debug_port: u16,
}

impl Default for FeroxConfig {
    fn default() -> Self {
        Self {
            memory: MemoryConfig {
                max_webkit_heap_mb: 256,
                disk_cache_limit_mb: 100,
                memory_cache_limit_mb: 64,
                gc_interval_secs: 30,
                image_cache_purge_secs: 60,
            },
            tabs: TabConfig {
                suspend_after_secs: 300, // 5 minutes
                max_live_tabs: 8,
                suspend_cache_dir: None,
            },
            network: NetworkConfig {
                adblock_enabled: true,
                custom_blocklist_path: None,
                https_only: false,
                dns_over_https: false,
                doh_url: "https://1.1.1.1/dns-query".to_string(),
            },
            ui: UiConfig {
                hardware_acceleration: true,
                window_width: 1280,
                window_height: 800,
                homepage: "about:blank".to_string(),
                dark_mode: false,
            },
            devtools: DevToolsConfig {
                enabled: true,
                panel_height: 300,
                remote_debug_port: 0,
            },
        }
    }
}

impl Default for NetworkConfig {
    fn default() -> Self {
        Self {
            adblock_enabled: true,
            custom_blocklist_path: None,
            https_only: false,
            dns_over_https: false,
            doh_url: "https://1.1.1.1/dns-query".to_string(),
        }
    }
}

impl FeroxConfig {
    /// Load configuration from disk, falling back to defaults on any error.
    pub fn load() -> Self {
        match Self::try_load() {
            Ok(cfg) => cfg,
            Err(e) => {
                warn!("Failed to load config ({e}), using defaults");
                Self::default()
            }
        }
    }

    fn try_load() -> Result<Self> {
        let path = config_path()?;
        if !path.exists() {
            let cfg = Self::default();
            cfg.save()?;
            return Ok(cfg);
        }
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("Reading {}", path.display()))?;
        toml::from_str(&content).context("Parsing config TOML")
    }

    /// Persist the current configuration to disk.
    pub fn save(&self) -> Result<()> {
        let path = config_path()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = toml::to_string_pretty(self)?;
        std::fs::write(&path, content)?;
        Ok(())
    }
}

fn config_path() -> Result<PathBuf> {
    let base = dirs::config_dir().context("Cannot determine config directory")?;
    Ok(base.join("ferrox").join("config.toml"))
}
