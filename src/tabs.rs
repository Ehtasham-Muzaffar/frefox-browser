//! Tab lifecycle management — creation, suspension, restoration, and
//! destruction with memory budgeting across all live tabs.

use crate::config::TabConfig;
use anyhow::Result;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use tracing::{info, warn};
use uuid::Uuid;
#[cfg(target_os = "linux")]
use webkit6::prelude::*;

#[cfg(target_os = "linux")]
pub type PlatformWebView = webkit6::WebView;
#[cfg(target_os = "windows")]
pub type PlatformWebView = wry::WebView;

/// Every tab that has ever been opened is tracked by this record.
pub struct Tab {
    pub id: Uuid,
    pub title: String,
    pub url: String,
    pub state: TabState,
    pub last_active: DateTime<Utc>,
    #[allow(dead_code)]
    pub favicon_path: Option<PathBuf>,
    /// Platform WebView handle (None when suspended — frees DOM/JS heap)
    pub webview: Option<PlatformWebView>,
}

/// The lifecycle state machine for a single tab.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TabState {
    /// Fully live — WebView allocated and rendering
    Active,
    /// Loaded but not focused — WebView still alive, eligible for suspension
    Background,
    /// WebView destroyed; session state serialized to disk
    Suspended,
    /// Being loaded from suspended state
    Restoring,
    /// Closed and removed from the tab strip
    Closed,
}

/// Serializable state saved when a tab is suspended.
#[derive(Debug, Serialize, Deserialize)]
pub struct SuspendedTabState {
    pub id: String,
    pub url: String,
    pub title: String,
    pub scroll_x: f64,
    pub scroll_y: f64,
    pub suspended_at: DateTime<Utc>,
}

/// Manages all tabs and enforces memory policies.
pub struct TabManager {
    tabs: HashMap<Uuid, Tab>,
    tab_order: Vec<Uuid>,
    active_tab: Option<Uuid>,
    config: TabConfig,
    suspend_dir: PathBuf,
}

impl TabManager {
    pub fn new(config: TabConfig) -> Result<Self> {
        let suspend_dir = config
            .suspend_cache_dir
            .clone()
            .unwrap_or_else(|| {
                dirs::cache_dir()
                    .unwrap_or_else(|| PathBuf::from("/tmp"))
                    .join("ferrox")
                    .join("suspended_tabs")
            });
        std::fs::create_dir_all(&suspend_dir)?;

        Ok(Self {
            tabs: HashMap::new(),
            tab_order: Vec::new(),
            active_tab: None,
            config,
            suspend_dir,
        })
    }

    /// Open a new tab navigating to `url`, returning its ID.
    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub fn open_tab(&mut self, url: &str, webview: PlatformWebView) -> Uuid {
        self.open_tab_with_id(Uuid::new_v4(), url, webview)
    }

    /// Open a new tab with a pre-allocated UUID so early callbacks bind immediately.
    pub fn open_tab_with_id(&mut self, id: Uuid, url: &str, webview: PlatformWebView) -> Uuid {
        let tab = Tab {
            id,
            title: url.to_string(),
            url: url.to_string(),
            state: TabState::Active,
            last_active: Utc::now(),
            favicon_path: None,
            webview: Some(webview),
        };
        self.tabs.insert(id, tab);
        self.tab_order.push(id);
        info!("Tab opened: {} -> {}", id, url);
        self.enforce_live_tab_limit();
        id
    }

    /// Mark a tab as the focused tab; puts the previous active tab into Background.
    pub fn focus_tab(&mut self, id: Uuid) {
        if let Some(prev) = self.active_tab {
            if prev != id {
                if let Some(t) = self.tabs.get_mut(&prev) {
                    if t.state == TabState::Active {
                        t.state = TabState::Background;
                    }
                }
            }
        }
        if let Some(tab) = self.tabs.get_mut(&id) {
            tab.state = TabState::Active;
            tab.last_active = Utc::now();
            self.active_tab = Some(id);
        }
    }

    /// Suspend a background tab: serialize state, destroy WebView.
    pub fn suspend_tab(&mut self, id: Uuid) -> Result<()> {
        let tab = self.tabs.get_mut(&id).ok_or_else(|| {
            anyhow::anyhow!("Tab {} not found", id)
        })?;

        if tab.state == TabState::Active {
            warn!("Refusing to suspend the active tab ({})", id);
            return Ok(());
        }
        if tab.state == TabState::Suspended {
            return Ok(());
        }

        let suspended = SuspendedTabState {
            id: id.to_string(),
            url: tab.url.clone(),
            title: tab.title.clone(),
            scroll_x: 0.0,
            scroll_y: 0.0,
            suspended_at: Utc::now(),
        };
        let state_path = self.suspend_dir.join(format!("{}.json", id));
        let json = serde_json::to_string_pretty(&suspended)?;
        std::fs::write(&state_path, json)?;

        // Drop WebView handle to reclaim renderer heap
        tab.webview = None;
        tab.state = TabState::Suspended;
        info!("Tab suspended: {} ({})", id, tab.url);
        Ok(())
    }

    /// Restore a suspended tab, re-creating its WebView and navigating back.
    pub fn restore_tab(&mut self, id: Uuid, new_webview: PlatformWebView) -> Result<()> {
        let state_path = self.suspend_dir.join(format!("{}.json", id));
        let json = std::fs::read_to_string(&state_path)?;
        let state: SuspendedTabState = serde_json::from_str(&json)?;

        if let Some(tab) = self.tabs.get_mut(&id) {
            tab.state = TabState::Restoring;
            #[cfg(target_os = "linux")]
            new_webview.load_uri(&state.url);
            #[cfg(target_os = "windows")]
            let _ = new_webview.load_url(&state.url);
            tab.webview = Some(new_webview);
            tab.url = state.url.clone();
            tab.title = state.title;
            tab.state = TabState::Background;
            info!("Tab restored: {} -> {}", id, state.url);
            let _ = std::fs::remove_file(&state_path);
        }
        Ok(())
    }

    /// Close and permanently remove a tab.
    pub fn close_tab(&mut self, id: Uuid) {
        if let Some(tab) = self.tabs.get_mut(&id) {
            tab.webview = None;
            tab.state = TabState::Closed;
        }
        self.tabs.remove(&id);
        self.tab_order.retain(|t| *t != id);
        if self.active_tab == Some(id) {
            self.active_tab = self.tab_order.last().copied();
        }
        let state_path = self.suspend_dir.join(format!("{}.json", id));
        let _ = std::fs::remove_file(state_path);
        info!("Tab closed: {}", id);
    }

    /// Scan background tabs and suspend any that have been inactive longer
    /// than `config.suspend_after_secs`. Returns IDs of newly suspended tabs.
    pub fn tick_suspension_policy(&mut self) -> Vec<Uuid> {
        let threshold = chrono::Duration::seconds(self.config.suspend_after_secs as i64);
        let now = Utc::now();
        let candidates: Vec<Uuid> = self
            .tabs
            .iter()
            .filter(|(_, t)| {
                t.state == TabState::Background && (now - t.last_active) > threshold
            })
            .map(|(id, _)| *id)
            .collect();

        let mut suspended = Vec::new();
        for id in candidates {
            match self.suspend_tab(id) {
                Ok(()) => suspended.push(id),
                Err(e) => warn!("Could not suspend tab {}: {}", id, e),
            }
        }
        suspended
    }

    /// Enforce that no more than `config.max_live_tabs` tabs are live
    /// by suspending the least-recently-used background tabs.
    fn enforce_live_tab_limit(&mut self) {
        let live_count = self.live_tab_count();
        if live_count <= self.config.max_live_tabs {
            return;
        }

        let mut bg_tabs: Vec<(Uuid, DateTime<Utc>)> = self
            .tabs
            .iter()
            .filter(|(_, t)| t.state == TabState::Background)
            .map(|(id, t)| (*id, t.last_active))
            .collect();
        bg_tabs.sort_by_key(|(_, ts)| *ts);

        let to_suspend = live_count - self.config.max_live_tabs;
        for (id, _) in bg_tabs.into_iter().take(to_suspend) {
            if let Err(e) = self.suspend_tab(id) {
                warn!("LRU suspension failed for {}: {}", id, e);
            }
        }
    }

    pub fn update_tab_metadata(&mut self, id: Uuid, url: Option<&str>, title: Option<&str>) {
        if let Some(tab) = self.tabs.get_mut(&id) {
            if let Some(u) = url {
                tab.url = u.to_string();
            }
            if let Some(t) = title {
                if !t.is_empty() {
                    tab.title = t.to_string();
                }
            }
        }
    }

    pub fn active_tab_id(&self) -> Option<Uuid> {
        self.active_tab
    }

    pub fn live_tab_count(&self) -> usize {
        self.tabs
            .values()
            .filter(|t| t.state == TabState::Active || t.state == TabState::Background)
            .count()
    }

    pub fn total_tab_count(&self) -> usize {
        self.tabs.len()
    }

    pub fn get_tab(&self, id: Uuid) -> Option<&Tab> {
        self.tabs.get(&id)
    }

    pub fn ordered_tabs(&self) -> Vec<&Tab> {
        self.tab_order
            .iter()
            .filter_map(|id| self.tabs.get(id))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn suspended_tab_state_round_trips_json() {
        let state = SuspendedTabState {
            id: Uuid::new_v4().to_string(),
            url: "https://www.rust-lang.org".to_string(),
            title: "Rust".to_string(),
            scroll_x: 0.0,
            scroll_y: 420.5,
            suspended_at: Utc::now(),
        };
        let json = serde_json::to_string(&state).expect("serialize");
        let parsed: SuspendedTabState = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(parsed.url, state.url);
        assert_eq!(parsed.title, state.title);
        assert_eq!(parsed.scroll_y, 420.5);
    }
}
