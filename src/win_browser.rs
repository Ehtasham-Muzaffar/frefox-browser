//! Windows native browser shell using `tao` + `wry` (Edge WebView2).
//!
//! Features:
//! - Instant local `ferrox://start` new-tab dashboard (zero network wait)
//! - Smooth 2px top progress bar, Stop/Reload toggle, Home button, Find-in-Page (`Ctrl+F`), Zoom (`Ctrl+/-/0`)
//! - Seamless `target="_blank"` / `window.open` routing into Ferrox tabs
//! - Download manager routing files to the user's Downloads folder with status toast
//! - Shared `TabManager` suspension/restore, Aho-Corasick `AdBlocker`, `BrowserStorage` Omnibox, and `F12` DevTools

use crate::adblock;
use crate::config::FeroxConfig;
use crate::memory;
use crate::network;
use crate::storage::BrowserStorage;
use crate::tabs::{TabManager, TabState};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet};
use std::time::{Duration, Instant};
use tao::dpi::LogicalSize;
use tao::event::{Event, KeyEvent, WindowEvent};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tao::keyboard::KeyCode;
use tao::window::{Window, WindowBuilder};
use tracing::info;
use uuid::Uuid;
use wry::dpi::LogicalPosition;
use wry::{NewWindowResponse, PageLoadEvent, Rect, WebView, WebViewBuilder};

const CHROME_HEIGHT: u32 = 86;
const DEVTOOLS_HEIGHT: u32 = 300;
const NEWTAB_URL: &str = "ferrox://start";

#[derive(Debug)]
pub enum AppEvent {
    ChromeIpc(String),
    DevToolsIpc(String),
    DevToolsEvalScript(String),
    ContentTelemetry { tab_id: Uuid, payload: String },
    NavigationBlocked { tab_id: Uuid, url: String },
    RedirectUrl { tab_id: Uuid, url: String },
    OpenInNewTab { url: String },
    DownloadNotice { message: String },
    PageLoaded { tab_id: Uuid, url: String, finished: bool },
    TitleChanged { tab_id: Uuid, title: String },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "cmd")]
enum ChromeCmd {
    Ready,
    Navigate { input: String },
    QuerySuggest { query: String },
    SuggestRows { count: usize },
    Back,
    Forward,
    ReloadOrStop,
    Home,
    NewTab,
    SwitchTab { id: String },
    CloseTab { id: String },
    ToggleBookmark,
    ToggleDevTools,
    SuspendBackground,
    FindInPage { query: String },
    Shortcut { action: String },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "cmd")]
enum DevToolsCmd {
    Eval { code: String },
    InspectDom,
    InspectStyle { selector: String },
    SetAttr { selector: String, attr: String, value: String },
    SampleMemory,
    InspectStorage { kind: String },
    OpenNativeDevTools,
    SuspendIdle,
    BlockFetchPattern { pattern: String },
    SetThrottle { ms: u64 },
    Shortcut { action: String },
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind")]
enum PageTelemetry {
    #[serde(rename = "console")]
    Console { level: String, message: String },
    #[serde(rename = "network")]
    Network {
        method: String,
        url: String,
        status: String,
        duration_ms: u64,
    },
    #[serde(rename = "batch")]
    Batch { items: Vec<PageTelemetry> },
    #[serde(rename = "shortcut")]
    Shortcut { action: String },
}

#[derive(Serialize)]
struct TabViewItem {
    id: String,
    title: String,
    url: String,
    active: bool,
    suspended: bool,
    loading: bool,
}

struct UiSyncCache {
    last_rss_sample: Instant,
    cached_rss_mb: f64,
    loading_tabs: HashSet<Uuid>,
    tab_zoom: HashMap<Uuid, f64>,
    suggest_rows: usize,
    toast: Option<(String, Instant)>,
}

impl Default for UiSyncCache {
    fn default() -> Self {
        Self {
            last_rss_sample: Instant::now() - Duration::from_secs(10),
            cached_rss_mb: 0.0,
            loading_tabs: HashSet::new(),
            tab_zoom: HashMap::new(),
            suggest_rows: 0,
            toast: None,
        }
    }
}

pub fn run_windows_browser(config: FeroxConfig) -> Result<()> {
    let event_loop = EventLoopBuilder::<AppEvent>::with_user_event().build();
    let proxy = event_loop.create_proxy();

    let window = WindowBuilder::new()
        .with_title("Ferrox Browser")
        .with_visible(true)
        .with_inner_size(LogicalSize::new(
            config.ui.window_width.max(960) as u32,
            config.ui.window_height.max(640) as u32,
        ))
        .build(&event_loop)?;
    // Force visibility even if parent process started with SW_HIDE
    window.set_visible(true);
    window.set_focus();

    let mut devtools_open = false;
    let mut ui_cache = UiSyncCache::default();
    let (chrome_rect, content_rect, dt_rect) =
        compute_rects(&window, devtools_open, ui_cache.suggest_rows);

    let chrome_proxy = proxy.clone();
    let chrome_wv = WebViewBuilder::new()
        .with_bounds(chrome_rect)
        .with_clipboard(true)
        .with_html(CHROME_HTML)
        .with_ipc_handler(move |req| {
            let _ = chrome_proxy.send_event(AppEvent::ChromeIpc(req.body().clone()));
        })
        .build_as_child(&window)?;

    let dt_proxy = proxy.clone();
    let devtools_wv = WebViewBuilder::new()
        .with_bounds(dt_rect)
        .with_visible(false)
        .with_clipboard(true)
        .with_html(DEVTOOLS_HTML)
        .with_ipc_handler(move |req| {
            let _ = dt_proxy.send_event(AppEvent::DevToolsIpc(req.body().clone()));
        })
        .build_as_child(&window)?;

    let mut tab_manager = TabManager::new(config.tabs.clone())?;
    let mut storage = BrowserStorage::load();

    let start_url = if config.ui.homepage == "about:blank" {
        NEWTAB_URL.to_string()
    } else {
        config.ui.homepage.clone()
    };

    let active_id = open_and_focus_tab(
        &window,
        &mut tab_manager,
        &start_url,
        content_rect,
        &config,
        &storage,
        &mut ui_cache,
        &proxy,
    )?;

    let mut last_tick = Instant::now();
    let mut ctrl_held = false;
    let mut shift_held = false;
    let mut alt_held = false;

    info!("Ferrox Windows browser ready — initial tab {}", active_id);

    event_loop.run(move |event, _, control_flow| {
        *control_flow = ControlFlow::WaitUntil(Instant::now() + Duration::from_secs(10));

        if last_tick.elapsed() >= Duration::from_secs(60) {
            last_tick = Instant::now();
            let suspended = tab_manager.tick_suspension_policy();
            if !suspended.is_empty() {
                sync_chrome_ui(&chrome_wv, &tab_manager, &storage, &mut ui_cache);
            }
        }

        match event {
            Event::WindowEvent {
                event: WindowEvent::CloseRequested,
                ..
            } => {
                *control_flow = ControlFlow::Exit;
            }
            Event::WindowEvent {
                event: WindowEvent::Resized(_),
                ..
            } => {
                relayout_views(&window, &chrome_wv, &devtools_wv, &tab_manager, devtools_open, ui_cache.suggest_rows);
            }
            Event::WindowEvent {
                event: WindowEvent::ModifiersChanged(mods),
                ..
            } => {
                ctrl_held = mods.control_key();
                shift_held = mods.shift_key();
                alt_held = mods.alt_key();
            }
            Event::WindowEvent {
                event:
                    WindowEvent::KeyboardInput {
                        event:
                            KeyEvent {
                                physical_key,
                                state: tao::event::ElementState::Pressed,
                                ..
                            },
                        ..
                    },
                ..
            } => {
                let action = if physical_key == KeyCode::F12
                    || (ctrl_held && shift_held && physical_key == KeyCode::KeyI)
                {
                    Some("devtools")
                } else if ctrl_held && physical_key == KeyCode::KeyL {
                    Some("focus_url")
                } else if ctrl_held && physical_key == KeyCode::KeyF {
                    Some("find")
                } else if ctrl_held && physical_key == KeyCode::KeyT {
                    Some("new_tab")
                } else if ctrl_held && physical_key == KeyCode::KeyW {
                    Some("close_tab")
                } else if ctrl_held && physical_key == KeyCode::KeyD {
                    Some("bookmark")
                } else if physical_key == KeyCode::F5 || (ctrl_held && physical_key == KeyCode::KeyR) {
                    Some("reload")
                } else if alt_held && physical_key == KeyCode::ArrowLeft {
                    Some("back")
                } else if alt_held && physical_key == KeyCode::ArrowRight {
                    Some("forward")
                } else if ctrl_held && physical_key == KeyCode::Equal {
                    Some("zoom_in")
                } else if ctrl_held && physical_key == KeyCode::Minus {
                    Some("zoom_out")
                } else if ctrl_held && physical_key == KeyCode::Digit0 {
                    Some("zoom_reset")
                } else {
                    None
                };
                if let Some(act) = action {
                    handle_shortcut(
                        act,
                        &window,
                        &chrome_wv,
                        &devtools_wv,
                        &mut tab_manager,
                        &mut storage,
                        &mut ui_cache,
                        &mut devtools_open,
                        &config,
                        &proxy,
                    );
                }
            }
            Event::UserEvent(app_event) => match app_event {
                AppEvent::ChromeIpc(raw) => {
                    if let Ok(cmd) = serde_json::from_str::<ChromeCmd>(&raw) {
                        match cmd {
                            ChromeCmd::Ready => {
                                sync_chrome_ui(&chrome_wv, &tab_manager, &storage, &mut ui_cache);
                            }
                            ChromeCmd::Navigate { input } => {
                                ui_cache.suggest_rows = 0;
                                relayout_views(&window, &chrome_wv, &devtools_wv, &tab_manager, devtools_open, 0);
                                let url = normalize_input(&input, &config);
                                if let Some(id) = tab_manager.active_tab_id() {
                                    if url == NEWTAB_URL {
                                        tab_manager.update_tab_metadata(id, Some(NEWTAB_URL), Some("New Tab"));
                                        if let Some(wv) = tab_manager.get_tab(id).and_then(|t| t.webview.as_ref()) {
                                            let _ = wv.load_html(&build_start_page_html(&storage, &tab_manager, &mut ui_cache));
                                        }
                                        sync_chrome_ui(&chrome_wv, &tab_manager, &storage, &mut ui_cache);
                                    } else if let Some(wv) = tab_manager.get_tab(id).and_then(|t| t.webview.as_ref()) {
                                        ui_cache.loading_tabs.insert(id);
                                        sync_chrome_ui(&chrome_wv, &tab_manager, &storage, &mut ui_cache);
                                        let _ = wv.load_url(&url);
                                    }
                                }
                            }
                            ChromeCmd::QuerySuggest { query } => {
                                let suggestions = storage.suggest(&query, 5);
                                let next_rows = suggestions.len();
                                if ui_cache.suggest_rows != next_rows {
                                    ui_cache.suggest_rows = next_rows;
                                    relayout_views(&window, &chrome_wv, &devtools_wv, &tab_manager, devtools_open, next_rows);
                                }
                                if let Ok(json) = serde_json::to_string(&suggestions) {
                                    let _ = chrome_wv.evaluate_script(&format!(
                                        "window.renderSuggestions({json});"
                                    ));
                                }
                            }
                            ChromeCmd::SuggestRows { count } => {
                                if ui_cache.suggest_rows != count {
                                    ui_cache.suggest_rows = count;
                                    relayout_views(&window, &chrome_wv, &devtools_wv, &tab_manager, devtools_open, count);
                                }
                            }
                            ChromeCmd::Back => {
                                if let Some(wv) = active_webview(&tab_manager) {
                                    let _ = wv.evaluate_script("history.back();");
                                }
                            }
                            ChromeCmd::Forward => {
                                if let Some(wv) = active_webview(&tab_manager) {
                                    let _ = wv.evaluate_script("history.forward();");
                                }
                            }
                            ChromeCmd::ReloadOrStop => {
                                if let Some(id) = tab_manager.active_tab_id() {
                                    let is_loading = ui_cache.loading_tabs.contains(&id);
                                    if let Some(wv) = active_webview(&tab_manager) {
                                        if is_loading {
                                            let _ = wv.evaluate_script("window.stop();");
                                            ui_cache.loading_tabs.remove(&id);
                                            sync_chrome_ui(&chrome_wv, &tab_manager, &storage, &mut ui_cache);
                                        } else {
                                            ui_cache.loading_tabs.insert(id);
                                            sync_chrome_ui(&chrome_wv, &tab_manager, &storage, &mut ui_cache);
                                            let _ = wv.evaluate_script("location.reload();");
                                        }
                                    }
                                }
                            }
                            ChromeCmd::Home => {
                                if let Some(id) = tab_manager.active_tab_id() {
                                    ui_cache.loading_tabs.remove(&id);
                                    tab_manager.update_tab_metadata(id, Some(NEWTAB_URL), Some("New Tab"));
                                    if let Some(wv) = tab_manager.get_tab(id).and_then(|t| t.webview.as_ref()) {
                                        let _ = wv.load_html(&build_start_page_html(&storage, &tab_manager, &mut ui_cache));
                                    }
                                    sync_chrome_ui(&chrome_wv, &tab_manager, &storage, &mut ui_cache);
                                }
                            }
                            ChromeCmd::NewTab => {
                                handle_shortcut(
                                    "new_tab",
                                    &window,
                                    &chrome_wv,
                                    &devtools_wv,
                                    &mut tab_manager,
                                    &mut storage,
                                    &mut ui_cache,
                                    &mut devtools_open,
                                    &config,
                                    &proxy,
                                );
                            }
                            ChromeCmd::SwitchTab { id } => {
                                if let Ok(target_id) = Uuid::parse_str(&id) {
                                    let (_, content_r, _) =
                                        compute_rects(&window, devtools_open, ui_cache.suggest_rows);
                                    let is_suspended = tab_manager
                                        .get_tab(target_id)
                                        .map(|t| t.state == TabState::Suspended)
                                        .unwrap_or(false);
                                    if is_suspended {
                                        let restore_url = tab_manager
                                            .get_tab(target_id)
                                            .map(|t| t.url.clone())
                                            .unwrap_or_else(|| NEWTAB_URL.to_string());
                                        let html = if restore_url == NEWTAB_URL {
                                            Some(build_start_page_html(&storage, &tab_manager, &mut ui_cache))
                                        } else {
                                            None
                                        };
                                        if let Ok((new_wv, _)) = build_tab_webview(
                                            &window,
                                            target_id,
                                            &restore_url,
                                            html.as_deref(),
                                            content_r,
                                            &config,
                                            &proxy,
                                        ) {
                                            let _ = tab_manager.restore_tab(target_id, new_wv);
                                        }
                                    }
                                    tab_manager.focus_tab(target_id);
                                    focus_current_active(&window, &tab_manager, devtools_open, ui_cache.suggest_rows);
                                    if let Some(&z) = ui_cache.tab_zoom.get(&target_id) {
                                        if let Some(wv) = active_webview(&tab_manager) {
                                            let _ = wv.zoom(z);
                                        }
                                    }
                                    sync_chrome_ui(&chrome_wv, &tab_manager, &storage, &mut ui_cache);
                                }
                            }
                            ChromeCmd::CloseTab { id } => {
                                if tab_manager.total_tab_count() > 1 {
                                    if let Ok(target_id) = Uuid::parse_str(&id) {
                                        ui_cache.loading_tabs.remove(&target_id);
                                        ui_cache.tab_zoom.remove(&target_id);
                                        tab_manager.close_tab(target_id);
                                        focus_current_active(&window, &tab_manager, devtools_open, ui_cache.suggest_rows);
                                        sync_chrome_ui(&chrome_wv, &tab_manager, &storage, &mut ui_cache);
                                    }
                                }
                            }
                            ChromeCmd::ToggleBookmark => {
                                handle_shortcut(
                                    "bookmark",
                                    &window,
                                    &chrome_wv,
                                    &devtools_wv,
                                    &mut tab_manager,
                                    &mut storage,
                                    &mut ui_cache,
                                    &mut devtools_open,
                                    &config,
                                    &proxy,
                                );
                            }
                            ChromeCmd::ToggleDevTools => {
                                handle_shortcut(
                                    "devtools",
                                    &window,
                                    &chrome_wv,
                                    &devtools_wv,
                                    &mut tab_manager,
                                    &mut storage,
                                    &mut ui_cache,
                                    &mut devtools_open,
                                    &config,
                                    &proxy,
                                );
                            }
                            ChromeCmd::SuspendBackground => {
                                let bg_ids: Vec<Uuid> = tab_manager
                                    .ordered_tabs()
                                    .into_iter()
                                    .filter(|t| t.state == TabState::Background)
                                    .map(|t| t.id)
                                    .collect();
                                let count = bg_ids.len();
                                for bid in bg_ids {
                                    ui_cache.loading_tabs.remove(&bid);
                                    let _ = tab_manager.suspend_tab(bid);
                                }
                                ui_cache.toast = Some((
                                    format!("Suspended {count} background tab(s)"),
                                    Instant::now(),
                                ));
                                sync_chrome_ui(&chrome_wv, &tab_manager, &storage, &mut ui_cache);
                            }
                            ChromeCmd::FindInPage { query } => {
                                if let Some(wv) = active_webview(&tab_manager) {
                                    let q_json = serde_json::to_string(&query).unwrap_or_default();
                                    let _ = wv.evaluate_script(&format!("window.find({q_json});"));
                                }
                            }
                            ChromeCmd::Shortcut { action } => {
                                handle_shortcut(
                                    &action,
                                    &window,
                                    &chrome_wv,
                                    &devtools_wv,
                                    &mut tab_manager,
                                    &mut storage,
                                    &mut ui_cache,
                                    &mut devtools_open,
                                    &config,
                                    &proxy,
                                );
                            }
                        }
                    }
                }
                AppEvent::DevToolsIpc(raw) => {
                    if let Ok(cmd) = serde_json::from_str::<DevToolsCmd>(&raw) {
                        if let DevToolsCmd::Shortcut { action } = cmd {
                            handle_shortcut(
                                &action,
                                &window,
                                &chrome_wv,
                                &devtools_wv,
                                &mut tab_manager,
                                &mut storage,
                                &mut ui_cache,
                                &mut devtools_open,
                                &config,
                                &proxy,
                            );
                        } else {
                            handle_devtools_cmd(
                                cmd,
                                &tab_manager,
                                &devtools_wv,
                                &chrome_wv,
                                &storage,
                                &proxy,
                            );
                        }
                    }
                }
                AppEvent::DevToolsEvalScript(js) => {
                    let _ = devtools_wv.evaluate_script(&js);
                }
                AppEvent::ContentTelemetry { tab_id, payload } => {
                    if let Ok(ev) = serde_json::from_str::<PageTelemetry>(&payload) {
                        dispatch_telemetry(
                            ev,
                            tab_id,
                            &window,
                            &chrome_wv,
                            &devtools_wv,
                            &mut tab_manager,
                            &mut storage,
                            &mut ui_cache,
                            &mut devtools_open,
                            &config,
                            &proxy,
                        );
                    }
                }
                AppEvent::NavigationBlocked { tab_id, url } => {
                    if tab_manager.active_tab_id() == Some(tab_id) {
                        let u = serde_json::to_string(&url).unwrap_or_default();
                        let _ = devtools_wv.evaluate_script(&format!(
                            "window.appendNetwork('GET', {u}, 'BLOCKED (AdBlock)');"
                        ));
                    }
                }
                AppEvent::RedirectUrl { tab_id, url } => {
                    if let Some(wv) = tab_manager.get_tab(tab_id).and_then(|t| t.webview.as_ref()) {
                        let _ = wv.load_url(&url);
                    }
                }
                AppEvent::OpenInNewTab { url } => {
                    let (_, content_r, _) =
                        compute_rects(&window, devtools_open, ui_cache.suggest_rows);
                    if open_and_focus_tab(
                        &window,
                        &mut tab_manager,
                        &url,
                        content_r,
                        &config,
                        &storage,
                        &mut ui_cache,
                        &proxy,
                    )
                    .is_ok()
                    {
                        sync_chrome_ui(&chrome_wv, &tab_manager, &storage, &mut ui_cache);
                    }
                }
                AppEvent::DownloadNotice { message } => {
                    ui_cache.toast = Some((message, Instant::now()));
                    sync_chrome_ui(&chrome_wv, &tab_manager, &storage, &mut ui_cache);
                }
                AppEvent::PageLoaded {
                    tab_id,
                    url,
                    finished,
                } => {
                    let display_url = if url.starts_with("data:") || url == "about:blank" {
                        tab_manager
                            .get_tab(tab_id)
                            .map(|t| t.url.clone())
                            .unwrap_or_else(|| NEWTAB_URL.to_string())
                    } else {
                        url.clone()
                    };
                    if !url.starts_with("data:") && url != "about:blank" {
                        tab_manager.update_tab_metadata(tab_id, Some(&url), None);
                    }
                    if finished {
                        ui_cache.loading_tabs.remove(&tab_id);
                        if !display_url.starts_with("ferrox://") && !display_url.starts_with("data:") {
                            let title = tab_manager
                                .get_tab(tab_id)
                                .map(|t| t.title.clone())
                                .unwrap_or_else(|| display_url.clone());
                            storage.record_visit(&display_url, &title);
                            let u = serde_json::to_string(&display_url).unwrap_or_default();
                            let _ = devtools_wv.evaluate_script(&format!(
                                "window.appendNetwork('NAV', {u}, '200 OK');"
                            ));
                        }
                    } else {
                        ui_cache.loading_tabs.insert(tab_id);
                    }
                    sync_chrome_ui(&chrome_wv, &tab_manager, &storage, &mut ui_cache);
                }
                AppEvent::TitleChanged { tab_id, title } => {
                    let clean = title.trim();
                    if !clean.is_empty() && !clean.starts_with("data:") {
                        tab_manager.update_tab_metadata(tab_id, None, Some(clean));
                        if tab_manager.active_tab_id() == Some(tab_id) {
                            window.set_title(&format!("{clean} — Ferrox Browser"));
                        }
                        sync_chrome_ui(&chrome_wv, &tab_manager, &storage, &mut ui_cache);
                    }
                }
            },
            _ => {}
        }
    });
}

#[allow(clippy::too_many_arguments)]
fn dispatch_telemetry(
    ev: PageTelemetry,
    tab_id: Uuid,
    window: &Window,
    chrome_wv: &WebView,
    devtools_wv: &WebView,
    tab_manager: &mut TabManager,
    storage: &mut BrowserStorage,
    ui_cache: &mut UiSyncCache,
    devtools_open: &mut bool,
    config: &FeroxConfig,
    proxy: &EventLoopProxy<AppEvent>,
) {
    match ev {
        PageTelemetry::Batch { items } => {
            for item in items {
                dispatch_telemetry(
                    item,
                    tab_id,
                    window,
                    chrome_wv,
                    devtools_wv,
                    tab_manager,
                    storage,
                    ui_cache,
                    devtools_open,
                    config,
                    proxy,
                );
            }
        }
        PageTelemetry::Shortcut { action } => {
            if tab_manager.active_tab_id() == Some(tab_id) {
                handle_shortcut(
                    &action,
                    window,
                    chrome_wv,
                    devtools_wv,
                    tab_manager,
                    storage,
                    ui_cache,
                    devtools_open,
                    config,
                    proxy,
                );
            }
        }
        PageTelemetry::Console { level, message } => {
            if tab_manager.active_tab_id() == Some(tab_id) {
                let l = serde_json::to_string(&level).unwrap_or_default();
                let m = serde_json::to_string(&message).unwrap_or_default();
                let _ = devtools_wv.evaluate_script(&format!(
                    "window.appendConsole({l}, {m});"
                ));
            }
        }
        PageTelemetry::Network {
            method,
            url,
            status,
            duration_ms,
        } => {
            if tab_manager.active_tab_id() == Some(tab_id) {
                let m = serde_json::to_string(&method).unwrap_or_default();
                let u = serde_json::to_string(&url).unwrap_or_default();
                let s = serde_json::to_string(&format!("{status} ({duration_ms} ms)"))
                    .unwrap_or_default();
                let _ = devtools_wv.evaluate_script(&format!(
                    "window.appendNetwork({m}, {u}, {s});"
                ));
            }
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_shortcut(
    action: &str,
    window: &Window,
    chrome_wv: &WebView,
    devtools_wv: &WebView,
    tab_manager: &mut TabManager,
    storage: &mut BrowserStorage,
    ui_cache: &mut UiSyncCache,
    devtools_open: &mut bool,
    config: &FeroxConfig,
    proxy: &EventLoopProxy<AppEvent>,
) {
    match action {
        "devtools" => {
            *devtools_open = !*devtools_open;
            relayout_views(
                window,
                chrome_wv,
                devtools_wv,
                tab_manager,
                *devtools_open,
                ui_cache.suggest_rows,
            );
            let _ = devtools_wv.set_visible(*devtools_open);
        }
        "focus_url" => {
            let _ = chrome_wv.focus();
            let _ = chrome_wv.evaluate_script(
                "const u=document.getElementById('url');u.focus();u.select();",
            );
        }
        "find" => {
            let _ = chrome_wv.focus();
            let _ = chrome_wv.evaluate_script("window.toggleFindBar();");
        }
        "new_tab" => {
            let (_, content_r, _) =
                compute_rects(window, *devtools_open, ui_cache.suggest_rows);
            if open_and_focus_tab(
                window,
                tab_manager,
                NEWTAB_URL,
                content_r,
                config,
                storage,
                ui_cache,
                proxy,
            )
            .is_ok()
            {
                sync_chrome_ui(chrome_wv, tab_manager, storage, ui_cache);
            }
        }
        "close_tab" => {
            if tab_manager.total_tab_count() > 1 {
                if let Some(id) = tab_manager.active_tab_id() {
                    ui_cache.loading_tabs.remove(&id);
                    ui_cache.tab_zoom.remove(&id);
                    tab_manager.close_tab(id);
                    focus_current_active(window, tab_manager, *devtools_open, ui_cache.suggest_rows);
                    sync_chrome_ui(chrome_wv, tab_manager, storage, ui_cache);
                }
            }
        }
        "bookmark" => {
            if let Some(id) = tab_manager.active_tab_id() {
                if let Some(tab) = tab_manager.get_tab(id) {
                    storage.toggle_bookmark(&tab.url, &tab.title);
                    sync_chrome_ui(chrome_wv, tab_manager, storage, ui_cache);
                }
            }
        }
        "reload" => {
            if let Some(id) = tab_manager.active_tab_id() {
                ui_cache.loading_tabs.insert(id);
                sync_chrome_ui(chrome_wv, tab_manager, storage, ui_cache);
                if let Some(wv) = active_webview(tab_manager) {
                    let _ = wv.evaluate_script("location.reload();");
                }
            }
        }
        "back" => {
            if let Some(wv) = active_webview(tab_manager) {
                let _ = wv.evaluate_script("history.back();");
            }
        }
        "forward" => {
            if let Some(wv) = active_webview(tab_manager) {
                let _ = wv.evaluate_script("history.forward();");
            }
        }
        "zoom_in" | "zoom_out" | "zoom_reset" => {
            if let Some(id) = tab_manager.active_tab_id() {
                let cur = *ui_cache.tab_zoom.get(&id).unwrap_or(&1.0);
                let next = match action {
                    "zoom_in" => (cur + 0.1).min(2.5),
                    "zoom_out" => (cur - 0.1).max(0.5),
                    _ => 1.0,
                };
                ui_cache.tab_zoom.insert(id, next);
                if let Some(wv) = active_webview(tab_manager) {
                    let _ = wv.zoom(next);
                }
            }
        }
        _ => {}
    }
}

#[allow(clippy::too_many_arguments)]
fn open_and_focus_tab(
    window: &Window,
    tab_manager: &mut TabManager,
    url: &str,
    bounds: Rect,
    config: &FeroxConfig,
    storage: &BrowserStorage,
    ui_cache: &mut UiSyncCache,
    proxy: &EventLoopProxy<AppEvent>,
) -> Result<Uuid> {
    let id = Uuid::new_v4();
    let html = if url == NEWTAB_URL {
        Some(build_start_page_html(storage, tab_manager, ui_cache))
    } else {
        None
    };

    let (wv, _) = build_tab_webview(
        window,
        id,
        url,
        html.as_deref(),
        bounds,
        config,
        proxy,
    )?;

    if !url.starts_with("ferrox://") {
        ui_cache.loading_tabs.insert(id);
    }

    tab_manager.open_tab_with_id(id, url, wv);
    if url == NEWTAB_URL {
        tab_manager.update_tab_metadata(id, Some(NEWTAB_URL), Some("New Tab"));
    }
    tab_manager.focus_tab(id);
    // Show active tab before hiding previous tabs to avoid frame flicker
    if let Some(active_wv) = active_webview(tab_manager) {
        let _ = active_wv.set_visible(true);
    }
    hide_inactive_tab_views(tab_manager, Some(id));
    Ok(id)
}

fn build_tab_webview(
    window: &Window,
    tab_id: Uuid,
    url: &str,
    inline_html: Option<&str>,
    bounds: Rect,
    config: &FeroxConfig,
    proxy: &EventLoopProxy<AppEvent>,
) -> Result<(WebView, Uuid)> {
    let nav_adblock = config.network.adblock_enabled;
    let nav_net_cfg = config.network.clone();
    let nav_proxy = proxy.clone();

    let ipc_proxy = proxy.clone();
    let load_proxy = proxy.clone();
    let title_proxy = proxy.clone();

    let popup_proxy = proxy.clone();
    let popup_adblock = config.network.adblock_enabled;

    let dl_start_proxy = proxy.clone();
    let dl_done_proxy = proxy.clone();

    let mut builder = WebViewBuilder::new()
        .with_bounds(bounds)
        .with_clipboard(true)
        .with_devtools(config.devtools.enabled)
        .with_initialization_script(CONTENT_TELEMETRY_JS)
        .with_navigation_handler(move |target_url| {
            if nav_adblock && adblock::is_blocked(&target_url) {
                let _ = nav_proxy.send_event(AppEvent::NavigationBlocked {
                    tab_id,
                    url: target_url,
                });
                return false;
            }
            if let Some(https_uri) = network::enforce_https(&target_url, &nav_net_cfg) {
                let _ = nav_proxy.send_event(AppEvent::RedirectUrl {
                    tab_id,
                    url: https_uri,
                });
                return false;
            }
            true
        })
        .with_new_window_req_handler(move |target_url, _features| {
            if !(popup_adblock && adblock::is_blocked(&target_url)) {
                let _ = popup_proxy.send_event(AppEvent::OpenInNewTab { url: target_url });
            }
            NewWindowResponse::Deny
        })
        .with_download_started_handler(move |dl_url, path| {
            let file_name = dl_url
                .split('/')
                .last()
                .and_then(|s| s.split('?').next())
                .filter(|s| !s.is_empty())
                .unwrap_or("download.bin");
            let dest = dirs::download_dir()
                .unwrap_or_else(|| std::path::PathBuf::from("."))
                .join(file_name);
            *path = dest;
            let _ = dl_start_proxy.send_event(AppEvent::DownloadNotice {
                message: format!("Downloading {file_name}..."),
            });
            true
        })
        .with_download_completed_handler(move |_dl_url, path, ok| {
            let name = path
                .as_ref()
                .and_then(|p| p.file_name())
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "file".to_string());
            let msg = if ok {
                format!("Saved {name} to Downloads")
            } else {
                format!("Download failed: {name}")
            };
            let _ = dl_done_proxy.send_event(AppEvent::DownloadNotice { message: msg });
        })
        .with_ipc_handler(move |req| {
            let _ = ipc_proxy.send_event(AppEvent::ContentTelemetry {
                tab_id,
                payload: req.body().clone(),
            });
        })
        .with_on_page_load_handler(move |ev, loaded_url| {
            let finished = matches!(ev, PageLoadEvent::Finished);
            let _ = load_proxy.send_event(AppEvent::PageLoaded {
                tab_id,
                url: loaded_url,
                finished,
            });
        })
        .with_document_title_changed_handler(move |title| {
            let _ = title_proxy.send_event(AppEvent::TitleChanged { tab_id, title });
        });

    builder = if let Some(html) = inline_html {
        builder.with_html(html)
    } else {
        builder.with_url(url)
    };

    let wv = builder.build_as_child(window)?;
    Ok((wv, tab_id))
}

fn focus_current_active(
    window: &Window,
    tab_manager: &TabManager,
    devtools_open: bool,
    suggest_rows: usize,
) {
    let (_, content_r, _) = compute_rects(window, devtools_open, suggest_rows);
    let active_id = tab_manager.active_tab_id();
    if let Some(wv) = active_webview(tab_manager) {
        let _ = wv.set_bounds(content_r);
        let _ = wv.set_visible(true);
    }
    hide_inactive_tab_views(tab_manager, active_id);
}

fn active_webview(tab_manager: &TabManager) -> Option<&WebView> {
    tab_manager
        .active_tab_id()
        .and_then(|id| tab_manager.get_tab(id))
        .and_then(|t| t.webview.as_ref())
}

fn hide_inactive_tab_views(tab_manager: &TabManager, active_id: Option<Uuid>) {
    for tab in tab_manager.ordered_tabs() {
        if Some(tab.id) != active_id {
            if let Some(wv) = &tab.webview {
                let _ = wv.set_visible(false);
            }
        }
    }
}

fn compute_rects(window: &Window, devtools_open: bool, suggest_rows: usize) -> (Rect, Rect, Rect) {
    let size = window.inner_size().to_logical::<u32>(window.scale_factor());
    let w = size.width.max(320);
    let h = size.height.max(240);
    let suggest_extra = if suggest_rows > 0 {
        (suggest_rows as u32 * 30 + 12).min(170)
    } else {
        0
    };
    let chrome_h = CHROME_HEIGHT + suggest_extra;
    let dt_h = if devtools_open && h > chrome_h + 160 {
        DEVTOOLS_HEIGHT.min(h - chrome_h - 120)
    } else {
        0
    };
    let content_h = h.saturating_sub(chrome_h + dt_h).max(60);

    let chrome = Rect {
        position: LogicalPosition::new(0, 0).into(),
        size: LogicalSize::new(w, chrome_h).into(),
    };
    let content = Rect {
        position: LogicalPosition::new(0, chrome_h).into(),
        size: LogicalSize::new(w, content_h).into(),
    };
    let devtools = Rect {
        position: LogicalPosition::new(0, chrome_h + content_h).into(),
        size: LogicalSize::new(w, dt_h.max(1)).into(),
    };
    (chrome, content, devtools)
}

fn relayout_views(
    window: &Window,
    chrome_wv: &WebView,
    devtools_wv: &WebView,
    tab_manager: &TabManager,
    devtools_open: bool,
    suggest_rows: usize,
) {
    let (cr, content_r, dr) = compute_rects(window, devtools_open, suggest_rows);
    let _ = chrome_wv.set_bounds(cr);
    let _ = devtools_wv.set_bounds(dr);
    for tab in tab_manager.ordered_tabs() {
        if let Some(wv) = &tab.webview {
            let _ = wv.set_bounds(content_r);
        }
    }
}

fn sync_chrome_ui(
    chrome_wv: &WebView,
    tab_manager: &TabManager,
    storage: &BrowserStorage,
    cache: &mut UiSyncCache,
) {
    if cache.last_rss_sample.elapsed() >= Duration::from_secs(2) {
        cache.cached_rss_mb = memory::browser_memory_stats().total_mb();
        cache.last_rss_sample = Instant::now();
    }
    if let Some((_, ts)) = &cache.toast {
        if ts.elapsed() >= Duration::from_secs(5) {
            cache.toast = None;
        }
    }

    let active_id = tab_manager.active_tab_id();
    let active_loading = active_id
        .map(|id| cache.loading_tabs.contains(&id))
        .unwrap_or(false);

    let tabs: Vec<TabViewItem> = tab_manager
        .ordered_tabs()
        .into_iter()
        .map(|t| TabViewItem {
            id: t.id.to_string(),
            title: t.title.clone(),
            url: t.url.clone(),
            active: Some(t.id) == active_id,
            suspended: t.state == TabState::Suspended,
            loading: cache.loading_tabs.contains(&t.id),
        })
        .collect();

    let active_url = active_id
        .and_then(|id| tab_manager.get_tab(id))
        .map(|t| t.url.clone())
        .unwrap_or_default();
    let bookmarked = storage.is_bookmarked(&active_url);
    let rss_mb = cache.cached_rss_mb;
    let live = tab_manager.live_tab_count();
    let total = tab_manager.total_tab_count();
    let toast_msg = cache
        .toast
        .as_ref()
        .map(|(m, _)| m.as_str())
        .unwrap_or("");

    if let (Ok(tabs_json), Ok(url_json), Ok(toast_json)) = (
        serde_json::to_string(&tabs),
        serde_json::to_string(&active_url),
        serde_json::to_string(toast_msg),
    ) {
        let script = format!(
            "window.syncState({tabs_json}, {url_json}, {bookmarked}, {rss_mb:.1}, {live}, {total}, {active_loading}, {toast_json});"
        );
        let _ = chrome_wv.evaluate_script(&script);
    }
}

fn build_start_page_html(
    storage: &BrowserStorage,
    tab_manager: &TabManager,
    cache: &mut UiSyncCache,
) -> String {
    if cache.cached_rss_mb == 0.0 {
        cache.cached_rss_mb = memory::browser_memory_stats().total_mb();
    }
    let bookmarks_html: String = if storage.bookmarks().is_empty() {
        "<div class='empty'>No bookmarks yet — press <b>Ctrl+D</b> or click &#9734; on any page.</div>".to_string()
    } else {
        storage
            .bookmarks()
            .iter()
            .take(8)
            .map(|b| {
                let t = b.title.replace('<', "&lt;").replace('>', "&gt;");
                let u = b.url.replace('"', "&quot;");
                format!("<a class='card' href=\"{u}\"><div class='ct'>{t}</div><div class='cu'>{u}</div></a>")
            })
            .collect()
    };

    format!(
        r#"<!doctype html><html><head><meta charset="utf-8"><title>New Tab</title><style>
*{{box-sizing:border-box;font-family:system-ui,-apple-system,Segoe UI,sans-serif}}
body{{margin:0;min-height:100vh;background:radial-gradient(circle at top,#24273a,#181825 65%);color:#cdd6f4;display:flex;flex-direction:column;align-items:center;padding:56px 24px}}
h1{{margin:0 0 6px;font-size:32px;letter-spacing:.5px;color:#89b4fa}}
.sub{{color:#a6adc8;font-size:13px;margin-bottom:26px}}
.search{{display:flex;width:100%;max-width:620px;gap:8px;margin-bottom:28px}}
.search input{{flex:1;padding:12px 18px;border-radius:24px;border:1px solid #45475a;background:#1e1e2e;color:#cdd6f4;font-size:15px;outline:none}}
.search input:focus{{border-color:#89b4fa}}
.search button{{padding:0 20px;border-radius:24px;border:none;background:#89b4fa;color:#11111b;font-weight:600;cursor:pointer}}
.pills{{display:flex;flex-wrap:wrap;gap:10px;justify-content:center;margin-bottom:32px}}
.pill{{padding:8px 14px;border-radius:18px;background:#313244;color:#cdd6f4;text-decoration:none;font-size:13px;border:1px solid #45475a}}
.pill:hover{{background:#45475a;border-color:#89b4fa}}
.sec{{width:100%;max-width:760px}}
.sec h3{{font-size:13px;color:#a6adc8;text-transform:uppercase;letter-spacing:.8px;margin-bottom:10px}}
.grid{{display:grid;grid-template-columns:repeat(auto-fill,minmax(220px,1fr));gap:10px}}
.card{{display:block;padding:12px;border-radius:10px;background:#1e1e2e;border:1px solid #313244;text-decoration:none;color:inherit}}
.card:hover{{border-color:#89b4fa;transform:translateY(-1px)}}
.ct{{font-size:13px;font-weight:600;margin-bottom:4px;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}}
.cu{{font-size:11px;color:#89b4fa;overflow:hidden;text-overflow:ellipsis;white-space:nowrap}}
.empty{{color:#6c7086;font-size:13px}}
</style></head><body>
<h1>Ferrox</h1>
<div class="sub">Lightweight Engine &bull; UI {rss:.1} MB &bull; {live}/{total} Live Tabs &bull; AdBlock Active</div>
<form class="search" onsubmit="event.preventDefault();const q=document.getElementById('q').value.trim();if(!q)return;location.href=(q.includes('://')||(q.includes('.')&&!q.includes(' ')))?(q.includes('://')?q:'https://'+q):'https://search.brave.com/search?q='+encodeURIComponent(q);">
  <input id="q" autofocus placeholder="Search the web or enter URL..." />
  <button type="submit">Go</button>
</form>
<div class="pills">
  <a class="pill" href="https://github.com">GitHub</a>
  <a class="pill" href="https://news.ycombinator.com">Hacker News</a>
  <a class="pill" href="https://www.wikipedia.org">Wikipedia</a>
  <a class="pill" href="https://doc.rust-lang.org">Rust Docs</a>
  <a class="pill" href="https://developer.mozilla.org">MDN Web Docs</a>
</div>
<div class="sec">
  <h3>Bookmarks</h3>
  <div class="grid">{bookmarks}</div>
</div>
</body></html>"#,
        rss = cache.cached_rss_mb,
        live = tab_manager.live_tab_count(),
        total = tab_manager.total_tab_count(),
        bookmarks = bookmarks_html
    )
}

fn handle_devtools_cmd(
    cmd: DevToolsCmd,
    tab_manager: &TabManager,
    devtools_wv: &WebView,
    chrome_wv: &WebView,
    storage: &BrowserStorage,
    proxy: &EventLoopProxy<AppEvent>,
) {
    let Some(wv) = active_webview(tab_manager) else {
        return;
    };
    match cmd {
        DevToolsCmd::Eval { code } => {
            let p = proxy.clone();
            let code_json = serde_json::to_string(&code).unwrap_or_default();
            let wrapper = format!(
                "(function(){{ try {{ const r = eval({code_json}); return typeof r === 'object' ? JSON.stringify(r, null, 2) : String(r); }} catch(e) {{ return 'Error: ' + e; }} }})()"
            );
            let _ = wv.evaluate_script_with_callback(&wrapper, move |res| {
                let res_json = serde_json::to_string(&res).unwrap_or_default();
                let _ = p.send_event(AppEvent::DevToolsEvalScript(format!(
                    "window.appendConsole('EVAL', {res_json});"
                )));
            });
        }
        DevToolsCmd::InspectDom => {
            let p = proxy.clone();
            let _ = wv.evaluate_script_with_callback(
                "document.documentElement ? document.documentElement.outerHTML.substring(0, 16384) : '<empty>'",
                move |res| {
                    let j = serde_json::to_string(&res).unwrap_or_default();
                    let _ = p.send_event(AppEvent::DevToolsEvalScript(format!(
                        "window.renderDomOutput({j});"
                    )));
                },
            );
        }
        DevToolsCmd::InspectStyle { selector } => {
            let p = proxy.clone();
            let sel = if selector.trim().is_empty() { "body" } else { selector.trim() };
            let sel_json = serde_json::to_string(sel).unwrap_or_default();
            let script = format!(
                r#"(function(){{
                    const el = document.querySelector({sel_json});
                    if(!el) return 'Element not found';
                    const cs = getComputedStyle(el);
                    const r = el.getBoundingClientRect();
                    return JSON.stringify({{
                        tag: el.tagName,
                        attributes: Object.fromEntries(Array.from(el.attributes).map(a=>[a.name,a.value])),
                        boxModel: {{width:r.width,height:r.height,margin:cs.margin,border:cs.borderWidth,padding:cs.padding}},
                        styles: {{display:cs.display,position:cs.position,color:cs.color,backgroundColor:cs.backgroundColor,fontFamily:cs.fontFamily,fontSize:cs.fontSize}}
                    }}, null, 2);
                }})()"#
            );
            let _ = wv.evaluate_script_with_callback(&script, move |res| {
                let j = serde_json::to_string(&res).unwrap_or_default();
                let _ = p.send_event(AppEvent::DevToolsEvalScript(format!(
                    "window.renderDomOutput({j});"
                )));
            });
        }
        DevToolsCmd::SetAttr {
            selector,
            attr,
            value,
        } => {
            let p = proxy.clone();
            let sel_j = serde_json::to_string(&selector).unwrap_or_default();
            let attr_j = serde_json::to_string(&attr).unwrap_or_default();
            let val_j = serde_json::to_string(&value).unwrap_or_default();
            let script = format!(
                "(function(){{ const el = document.querySelector({sel_j}); if(!el) return 'Element not found'; el.setAttribute({attr_j}, {val_j}); return 'Updated ' + {attr_j}; }})()"
            );
            let _ = wv.evaluate_script_with_callback(&script, move |res| {
                let j = serde_json::to_string(&res).unwrap_or_default();
                let _ = p.send_event(AppEvent::DevToolsEvalScript(format!(
                    "window.renderDomOutput({j});"
                )));
            });
        }
        DevToolsCmd::SampleMemory => {
            let p = proxy.clone();
            let mem = memory::browser_memory_stats();
            let live = tab_manager.live_tab_count();
            let total = tab_manager.total_tab_count();
            let _ = wv.evaluate_script_with_callback(
                "JSON.stringify({domNodes:document.getElementsByTagName('*').length,scripts:document.scripts.length,images:document.images.length,resources:performance.getEntriesByType('resource').length}, null, 2)",
                move |res| {
                    let j = serde_json::to_string(&res).unwrap_or_default();
                    let _ = p.send_event(AppEvent::DevToolsEvalScript(format!(
                        "window.renderMemory({:.1}, {:.1}, {}, {live}, {total}, {j});",
                        mem.ui_mb, mem.renderer_mb, mem.renderer_count
                    )));
                },
            );
        }
        DevToolsCmd::InspectStorage { kind } => {
            if kind == "bookmarks" {
                let j = serde_json::to_string(
                    &serde_json::to_string_pretty(storage.bookmarks()).unwrap_or_default(),
                )
                .unwrap_or_default();
                let _ = devtools_wv.evaluate_script(&format!("window.renderStorage({j});"));
                return;
            }
            let p = proxy.clone();
            let script = match kind.as_str() {
                "sessionStorage" => "JSON.stringify(Object.fromEntries(Array.from({length:sessionStorage.length},(_,i)=>[sessionStorage.key(i),sessionStorage.getItem(sessionStorage.key(i))])),null,2)",
                "cookies" => "document.cookie || '<no accessible cookies>'",
                "indexedDB" => "indexedDB.databases ? 'Use async console: indexedDB.databases().then(console.table)' : 'Unsupported'",
                _ => "JSON.stringify(Object.fromEntries(Array.from({length:localStorage.length},(_,i)=>[localStorage.key(i),localStorage.getItem(localStorage.key(i))])),null,2)",
            };
            let _ = wv.evaluate_script_with_callback(script, move |res| {
                let j = serde_json::to_string(&res).unwrap_or_default();
                let _ = p.send_event(AppEvent::DevToolsEvalScript(format!(
                    "window.renderStorage({j});"
                )));
            });
        }
        DevToolsCmd::OpenNativeDevTools => {
            wv.open_devtools();
        }
        DevToolsCmd::SuspendIdle => {
            let _ = chrome_wv.evaluate_script(
                "window.ipc.postMessage(JSON.stringify({cmd:'SuspendBackground'}));",
            );
        }
        DevToolsCmd::BlockFetchPattern { pattern } => {
            let p = serde_json::to_string(&pattern).unwrap_or_default();
            let _ = wv.evaluate_script(&format!(
                "window.__ferroxBlocked = window.__ferroxBlocked || []; window.__ferroxBlocked.push({p});"
            ));
        }
        DevToolsCmd::SetThrottle { ms } => {
            let _ = wv.evaluate_script(&format!("window.__ferroxThrottleMs = {ms};"));
        }
        DevToolsCmd::Shortcut { .. } => {}
    }
}

fn normalize_input(raw: &str, cfg: &FeroxConfig) -> String {
    let trimmed = raw.trim();
    if trimmed.is_empty() || trimmed == NEWTAB_URL {
        return NEWTAB_URL.to_string();
    }
    let mut uri = if trimmed.contains("://") || trimmed.starts_with("about:") {
        trimmed.to_string()
    } else if trimmed.contains('.') && !trimmed.contains(' ') {
        format!("https://{trimmed}")
    } else {
        let q: String = trimmed
            .chars()
            .map(|c| match c {
                'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
                ' ' => "+".to_string(),
                _ => format!("%{:02X}", c as u32),
            })
            .collect();
        format!("https://search.brave.com/search?q={q}")
    };
    if let Some(secure) = network::enforce_https(&uri, &cfg.network) {
        uri = secure;
    }
    uri
}

const CONTENT_TELEMETRY_JS: &str = r#"(function(){
  if(window.__ferroxHooked) return;
  window.__ferroxHooked = true;
  window.__ferroxBlocked = window.__ferroxBlocked || [
    'doubleclick.net','googlesyndication.com','google-analytics.com',
    'googletagmanager.com','connect.facebook.net','scorecardresearch.com'
  ];
  window.__ferroxThrottleMs = window.__ferroxThrottleMs || 0;
  let q = [];
  let timer = null;
  const flush = () => {
    timer = null;
    if(!q.length) return;
    const batch = q.splice(0, 40);
    try { window.ipc.postMessage(JSON.stringify({kind:'batch', items:batch})); } catch(_){}
    if(q.length && !timer) timer = setTimeout(flush, 60);
  };
  const send = (obj) => {
    if(q.length >= 160) return;
    q.push(obj);
    if(!timer) timer = setTimeout(flush, 60);
  };
  const sendNow = (obj) => {
    try { window.ipc.postMessage(JSON.stringify(obj)); } catch(_){}
  };
  window.addEventListener('keydown', (e) => {
    const k = e.key.toLowerCase();
    let action = null;
    if(e.key === 'F12' || (e.ctrlKey && e.shiftKey && k === 'i')) action = 'devtools';
    else if(e.ctrlKey && !e.altKey && k === 't') action = 'new_tab';
    else if(e.ctrlKey && !e.altKey && k === 'w') action = 'close_tab';
    else if(e.ctrlKey && !e.altKey && k === 'l') action = 'focus_url';
    else if(e.ctrlKey && !e.altKey && k === 'f') action = 'find';
    else if(e.ctrlKey && !e.altKey && k === 'd') action = 'bookmark';
    else if(e.key === 'F5' || (e.ctrlKey && !e.altKey && k === 'r')) action = 'reload';
    else if(e.altKey && e.key === 'ArrowLeft') action = 'back';
    else if(e.altKey && e.key === 'ArrowRight') action = 'forward';
    else if(e.ctrlKey && (e.key === '=' || e.key === '+')) action = 'zoom_in';
    else if(e.ctrlKey && e.key === '-') action = 'zoom_out';
    else if(e.ctrlKey && e.key === '0') action = 'zoom_reset';
    if(action){ e.preventDefault(); sendNow({kind:'shortcut', action}); }
  }, true);
  const fmt = (args) => Array.from(args).map(a => {
    if(typeof a === 'object' && a !== null) { try { return JSON.stringify(a); } catch { return String(a); } }
    return String(a);
  }).join(' ');
  ['log','warn','error','info'].forEach(lvl => {
    const orig = console[lvl].bind(console);
    console[lvl] = (...args) => { send({kind:'console', level:lvl.toUpperCase(), message:fmt(args)}); return orig(...args); };
  });
  const origTable = console.table ? console.table.bind(console) : null;
  console.table = (d) => { send({kind:'console', level:'TABLE', message:fmt([d])}); if(origTable) return origTable(d); };
  const timers = new Map();
  console.time = (l='default') => timers.set(l, performance.now());
  console.timeEnd = (l='default') => {
    if(timers.has(l)){ const ms = (performance.now()-timers.get(l)).toFixed(2); timers.delete(l); send({kind:'console', level:'TIME', message:`${l}: ${ms} ms`}); }
  };
  const origFetch = window.fetch;
  if(origFetch){
    window.fetch = async (input, init) => {
      const url = typeof input === 'string' ? input : (input && input.url) || '';
      const method = (init && init.method) || 'GET';
      if(window.__ferroxBlocked.some(p => url.includes(p))){
        send({kind:'network', method, url, status:'BLOCKED (AdBlock)', duration_ms:0});
        throw new TypeError('Blocked by Ferrox AdBlock');
      }
      const t0 = performance.now();
      if(window.__ferroxThrottleMs > 0) await new Promise(r => setTimeout(r, window.__ferroxThrottleMs));
      try {
        const res = await origFetch(input, init);
        send({kind:'network', method, url, status:String(res.status), duration_ms:Math.round(performance.now()-t0)});
        return res;
      } catch(e) {
        send({kind:'network', method, url, status:'ERR', duration_ms:Math.round(performance.now()-t0)});
        throw e;
      }
    };
  }
})();"#;

const CHROME_HTML: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><style>
*{box-sizing:border-box;font-family:system-ui,-apple-system,Segoe UI,sans-serif}
html,body{margin:0;padding:0;background:#181825;color:#cdd6f4;overflow:hidden;user-select:none}
.tabs{display:flex;align-items:center;gap:4px;height:36px;padding:4px 8px 0;background:#11111b;overflow-x:auto}
.tab{display:flex;align-items:center;gap:6px;padding:4px 10px;border-radius:8px 8px 0 0;background:#313244;color:#bac2de;font-size:12px;cursor:pointer;max-width:190px;transition:background .12s ease}
.tab:hover{background:#45475a}
.tab.active{background:#1e1e2e;color:#89b4fa;font-weight:600;border-top:2px solid #89b4fa}
.tab.suspended{opacity:.65;font-style:italic}
.dot{width:7px;height:7px;border-radius:50%;background:#89b4fa;flex-shrink:0;animation:pulse .75s infinite alternate}
@keyframes pulse{from{opacity:.3;transform:scale(.85)}to{opacity:1;transform:scale(1.15)}}
.tab span.title{overflow:hidden;text-overflow:ellipsis;white-space:nowrap}
.tab button.x{background:none;border:none;color:#9399b2;cursor:pointer;padding:0 2px;font-size:13px}
.tab button.x:hover{color:#f38ba8}
.newtab{background:#313244;border:none;color:#cdd6f4;border-radius:6px;width:24px;height:24px;cursor:pointer;font-weight:bold;flex-shrink:0}
.nav{display:flex;align-items:center;gap:6px;height:50px;padding:6px 10px;background:#181825;border-bottom:1px solid #313244;position:relative}
button.navbtn{background:#313244;border:1px solid #45475a;color:#cdd6f4;border-radius:8px;padding:5px 10px;cursor:pointer;font-size:13px;transition:background .12s ease}
button.navbtn:hover{background:#45475a}
#url{flex:1;background:#313244;border:1px solid #45475a;border-radius:18px;padding:6px 14px;color:#cdd6f4;font-size:13px;outline:none}
#url:focus{border-color:#89b4fa}
.hud{font-size:11px;color:#a6e3a1;background:#1e1e2e;border:1px solid #313244;padding:4px 8px;border-radius:6px;white-space:nowrap;cursor:pointer}
.toast{font-size:11px;color:#f9e2af;background:#313244;padding:4px 8px;border-radius:6px;display:none;white-space:nowrap}
.star.on{color:#f9e2af;border-color:#f9e2af}
#progress{position:fixed;left:0;right:0;top:84px;height:2px;background:transparent;overflow:hidden;pointer-events:none}
#bar{height:100%;width:0%;background:linear-gradient(90deg,#89b4fa,#a6e3a1,#89b4fa);background-size:200% 100%;transition:width .22s ease,opacity .22s ease;opacity:0}
#bar.loading{opacity:1;width:78%;animation:glide 1.2s linear infinite}
@keyframes glide{0%{background-position:0% 0%}100%{background-position:200% 0%}}
#suggest{position:fixed;left:150px;right:240px;top:86px;background:#1e1e2e;border:1px solid #45475a;border-radius:0 0 8px 8px;display:none;z-index:99;max-height:164px;overflow-y:auto}
.sitem{padding:6px 12px;font-size:12px;cursor:pointer;border-bottom:1px solid #313244;white-space:nowrap;overflow:hidden;text-overflow:ellipsis}
.sitem:hover{background:#313244}
#findbar{position:fixed;right:12px;top:42px;background:#1e1e2e;border:1px solid #89b4fa;border-radius:8px;padding:4px 8px;display:none;gap:6px;z-index:100}
#findIn{background:#313244;border:1px solid #45475a;color:#cdd6f4;border-radius:4px;padding:3px 8px;font-size:12px;outline:none}
</style></head><body>
<div class="tabs" id="tabs"></div>
<div class="nav">
  <button class="navbtn" onclick="send({cmd:'Back'})" title="Back (Alt+Left)">&#8592;</button>
  <button class="navbtn" onclick="send({cmd:'Forward'})" title="Forward (Alt+Right)">&#8594;</button>
  <button id="relBtn" class="navbtn" onclick="send({cmd:'ReloadOrStop'})" title="Reload / Stop (F5)">&#8635;</button>
  <button class="navbtn" onclick="send({cmd:'Home'})" title="Home">&#8962;</button>
  <input id="url" placeholder="Search or enter URL..." />
  <span id="toast" class="toast"></span>
  <button id="star" class="navbtn star" onclick="send({cmd:'ToggleBookmark'})" title="Bookmark (Ctrl+D)">&#9734;</button>
  <span id="hud" class="hud" onclick="send({cmd:'SuspendBackground'})" title="Click to suspend background tabs">0 MB &bull; 1/1 live</span>
  <button class="navbtn" onclick="send({cmd:'ToggleDevTools'})" title="DevTools (F12)">F12</button>
</div>
<div id="progress"><div id="bar"></div></div>
<div id="suggest"></div>
<div id="findbar">
  <input id="findIn" placeholder="Find in page..." />
  <button class="navbtn" onclick="send({cmd:'FindInPage',query:document.getElementById('findIn').value})">Next</button>
  <button class="navbtn" onclick="document.getElementById('findbar').style.display='none'">&times;</button>
</div>
<script>
const send = (obj) => window.ipc.postMessage(JSON.stringify(obj));
const escHtml = (s) => String(s).replace(/&/g,'&amp;').replace(/</g,'&lt;').replace(/>/g,'&gt;').replace(/"/g,'&quot;');
const urlEl = document.getElementById('url');
const sugEl = document.getElementById('suggest');
const barEl = document.getElementById('bar');
const relEl = document.getElementById('relBtn');
const toastEl = document.getElementById('toast');
let wasLoading = false;
let finishTimer = null;

window.addEventListener('keydown', (e) => {
  const k = e.key.toLowerCase();
  let action = null;
  if(e.key === 'F12' || (e.ctrlKey && e.shiftKey && k === 'i')) action = 'devtools';
  else if(e.ctrlKey && !e.altKey && k === 't') action = 'new_tab';
  else if(e.ctrlKey && !e.altKey && k === 'w') action = 'close_tab';
  else if(e.ctrlKey && !e.altKey && k === 'l') action = 'focus_url';
  else if(e.ctrlKey && !e.altKey && k === 'f') action = 'find';
  else if(e.ctrlKey && !e.altKey && k === 'd') action = 'bookmark';
  else if(e.key === 'F5' || (e.ctrlKey && !e.altKey && k === 'r')) action = 'reload';
  if(action){ e.preventDefault(); send({cmd:'Shortcut', action}); }
}, true);

function hideSuggest(){
  sugEl.style.display='none';
  sugEl.innerHTML='';
  send({cmd:'SuggestRows', count:0});
}
urlEl.addEventListener('keydown', (e) => {
  if(e.key === 'Enter'){ hideSuggest(); send({cmd:'Navigate', input:urlEl.value}); }
  if(e.key === 'Escape'){ hideSuggest(); urlEl.blur(); }
});
urlEl.addEventListener('input', () => send({cmd:'QuerySuggest', query:urlEl.value}));
urlEl.addEventListener('blur', () => setTimeout(hideSuggest, 140));
document.getElementById('findIn').addEventListener('keydown', (e) => {
  if(e.key === 'Enter') send({cmd:'FindInPage', query:e.target.value});
  if(e.key === 'Escape') document.getElementById('findbar').style.display='none';
});
window.toggleFindBar = () => {
  const fb = document.getElementById('findbar');
  const open = fb.style.display !== 'flex';
  fb.style.display = open ? 'flex' : 'none';
  if(open) document.getElementById('findIn').focus();
};
window.renderSuggestions = (items) => {
  sugEl.innerHTML = '';
  if(!items || !items.length){ sugEl.style.display='none'; return; }
  items.forEach(([t, u]) => {
    const div = document.createElement('div');
    div.className = 'sitem';
    const strong = document.createElement('span');
    strong.textContent = t + ' \u2014 ';
    const addr = document.createElement('span');
    addr.style.color = '#89b4fa';
    addr.textContent = u;
    div.appendChild(strong);
    div.appendChild(addr);
    div.addEventListener('mousedown', (e) => {
      e.preventDefault();
      urlEl.value = u;
      hideSuggest();
      send({cmd:'Navigate', input:u});
    });
    sugEl.appendChild(div);
  });
  sugEl.style.display = 'block';
};
window.syncState = (tabs, activeUrl, bookmarked, rssMb, live, total, activeLoading, toast) => {
  const tabsEl = document.getElementById('tabs');
  tabsEl.innerHTML = tabs.map(t => `<div class="tab ${t.active?'active':''} ${t.suspended?'suspended':''}" onclick="send({cmd:'SwitchTab',id:'${escHtml(t.id)}'})">${t.loading?'<span class="dot"></span>':''}<span class="title">${t.suspended?'[zZ] ':''}${escHtml(t.title)}</span><button class="x" onclick="event.stopPropagation();send({cmd:'CloseTab',id:'${escHtml(t.id)}'})">&times;</button></div>`).join('')
    + `<button class="newtab" onclick="send({cmd:'NewTab'})" title="New Tab (Ctrl+T)">+</button>`;
  if(document.activeElement !== urlEl) urlEl.value = activeUrl || '';
  const star = document.getElementById('star');
  star.textContent = bookmarked ? '\u2605' : '\u2606';
  star.classList.toggle('on', bookmarked);
  document.getElementById('hud').textContent = `${rssMb} MB \u2022 ${live}/${total} live`;
  relEl.innerHTML = activeLoading ? '&times;' : '&#8635;';
  if(activeLoading){
    if(finishTimer){ clearTimeout(finishTimer); finishTimer=null; }
    barEl.style.width = '';
    barEl.style.opacity = '';
    barEl.classList.add('loading');
  } else if(wasLoading){
    barEl.style.width = '100%';
    finishTimer = setTimeout(()=>{
      barEl.classList.remove('loading');
      barEl.style.width = '0%';
      finishTimer = null;
    }, 220);
  }
  wasLoading = activeLoading;
  if(toast){ toastEl.textContent=toast; toastEl.style.display='inline-block'; }
  else { toastEl.style.display='none'; }
};
send({cmd:'Ready'});
</script></body></html>"#;

const DEVTOOLS_HTML: &str = r#"<!doctype html>
<html><head><meta charset="utf-8"><style>
*{box-sizing:border-box;font-family:'Cascadia Code','JetBrains Mono',monospace}
html,body{margin:0;padding:0;height:100%;background:#181825;color:#cdd6f4;display:flex;flex-direction:column}
.bar{display:flex;align-items:center;gap:6px;padding:4px 8px;background:#11111b;border-top:2px solid #89b4fa;border-bottom:1px solid #313244}
.tbtn{background:none;border:none;color:#9399b2;padding:4px 10px;cursor:pointer;font-size:12px;border-radius:4px}
.tbtn.active{background:#313244;color:#89b4fa;font-weight:bold}
.spacer{flex:1}
.btn{background:#313244;border:1px solid #45475a;color:#cdd6f4;padding:3px 8px;border-radius:4px;cursor:pointer;font-size:11px}
.btn:hover{background:#45475a}
.pane{display:none;flex:1;flex-direction:column;padding:6px;overflow:hidden}
.pane.active{display:flex}
.row{display:flex;gap:6px;margin-bottom:6px}
input.in{flex:1;background:#1e1e2e;border:1px solid #45475a;color:#cdd6f4;padding:4px 8px;border-radius:4px;font-size:12px}
pre, .log{flex:1;margin:0;background:#1e1e2e;border:1px solid #313244;padding:8px;overflow:auto;font-size:11px;white-space:pre-wrap}
.netrow{padding:2px 4px;border-bottom:1px solid #313244}
.err{color:#f38ba8}
</style></head><body>
<div class="bar">
  <button class="tbtn active" onclick="showTab('elements',this)">Elements</button>
  <button class="tbtn" onclick="showTab('console',this)">Console</button>
  <button class="tbtn" onclick="showTab('network',this)">Network</button>
  <button class="tbtn" onclick="showTab('memory',this)">Memory</button>
  <button class="tbtn" onclick="showTab('app',this)">Application</button>
  <span class="spacer"></span>
  <button class="btn" onclick="send({cmd:'OpenNativeDevTools'})">WebView2 Inspector</button>
</div>

<div id="elements" class="pane active">
  <div class="row">
    <button class="btn" onclick="send({cmd:'InspectDom'})">Inspect DOM</button>
    <input id="sel" class="in" placeholder="CSS selector (e.g. body, #app)" />
    <button class="btn" onclick="send({cmd:'InspectStyle',selector:document.getElementById('sel').value})">Computed Style</button>
    <input id="attr" class="in" placeholder="attr=value" />
    <button class="btn" onclick="setAttr()">Set Attr</button>
  </div>
  <pre id="domOut">Click "Inspect DOM" or query a CSS selector...</pre>
</div>

<div id="console" class="pane">
  <div id="conOut" class="log">// Ferrox Console Ready</div>
  <div class="row" style="margin-top:6px;margin-bottom:0">
    <input id="jsIn" class="in" placeholder="Evaluate JS in active tab (Enter)..." />
    <button class="btn" onclick="runJs()">Run</button>
  </div>
</div>

<div id="network" class="pane">
  <div class="row">
    <input id="blkPat" class="in" placeholder="Block fetch substring..." />
    <button class="btn" onclick="send({cmd:'BlockFetchPattern',pattern:document.getElementById('blkPat').value});document.getElementById('blkPat').value=''">Block Pattern</button>
    <button id="thrBtn" class="btn" onclick="toggleThrottle()">Throttle: 0ms</button>
    <button class="btn" onclick="document.getElementById('netOut').innerHTML=''">Clear</button>
  </div>
  <div id="netOut" class="log"></div>
</div>

<div id="memory" class="pane">
  <div class="row">
    <button class="btn" onclick="send({cmd:'SampleMemory'})">Sample Memory &amp; DOM</button>
    <button class="btn" onclick="send({cmd:'SuspendIdle'})">Suspend Background Tabs</button>
  </div>
  <pre id="memOut">Click "Sample Memory &amp; DOM" for live Working Set &amp; DOM node telemetry...</pre>
</div>

<div id="app" class="pane">
  <div class="row">
    <button class="btn" onclick="send({cmd:'InspectStorage',kind:'localStorage'})">localStorage</button>
    <button class="btn" onclick="send({cmd:'InspectStorage',kind:'sessionStorage'})">sessionStorage</button>
    <button class="btn" onclick="send({cmd:'InspectStorage',kind:'cookies'})">Cookies</button>
    <button class="btn" onclick="send({cmd:'InspectStorage',kind:'bookmarks'})">Bookmarks</button>
  </div>
  <pre id="appOut">Select a storage bucket above...</pre>
</div>

<script>
const send = (obj) => window.ipc.postMessage(JSON.stringify(obj));
window.addEventListener('keydown', (e) => {
  if(e.key === 'F12' || (e.ctrlKey && e.shiftKey && e.key.toLowerCase() === 'i')){
    e.preventDefault();
    send({cmd:'Shortcut', action:'devtools'});
  }
}, true);
function showTab(id, btn){
  document.querySelectorAll('.pane').forEach(p=>p.classList.remove('active'));
  document.querySelectorAll('.tbtn').forEach(b=>b.classList.remove('active'));
  document.getElementById(id).classList.add('active'); btn.classList.add('active');
}
function runJs(){
  const el = document.getElementById('jsIn');
  if(!el.value.trim()) return;
  window.appendConsole('>', el.value);
  send({cmd:'Eval', code:el.value}); el.value='';
}
document.getElementById('jsIn').addEventListener('keydown', e=>{ if(e.key==='Enter') runJs(); });
function setAttr(){
  const raw = document.getElementById('attr').value;
  const idx = raw.indexOf('=');
  if(idx<1) return;
  send({cmd:'SetAttr', selector:document.getElementById('sel').value||'body', attr:raw.slice(0,idx), value:raw.slice(idx+1)});
}
let thr = 0;
function toggleThrottle(){
  thr = thr===0 ? 300 : 0;
  document.getElementById('thrBtn').textContent = thr ? 'Throttle: 300ms (3G)' : 'Throttle: 0ms';
  send({cmd:'SetThrottle', ms:thr});
}
window.renderDomOutput = (t) => document.getElementById('domOut').textContent = t;
window.appendConsole = (lvl, msg) => {
  const el = document.getElementById('conOut');
  const d = document.createElement('div');
  if(lvl==='ERROR') d.className='err';
  d.textContent = `[${lvl}] ${msg}`;
  el.appendChild(d); el.scrollTop = el.scrollHeight;
};
window.appendNetwork = (m, u, s) => {
  const el = document.getElementById('netOut');
  const d = document.createElement('div');
  d.className = 'netrow' + (String(s).startsWith('BLOCKED')||String(s).startsWith('ERR') ? ' err' : '');
  d.textContent = `[${s}] ${m} ${u}`;
  el.appendChild(d); el.scrollTop = el.scrollHeight;
};
window.renderMemory = (uiMb, renMb, renCount, live, total, domJson) => {
  const tot = (uiMb + renMb).toFixed(1);
  document.getElementById('memOut').textContent = `Total Browser RAM: ${tot} MB (UI: ${uiMb} MB + ${renCount} WebView2 child proc(s): ${renMb} MB)\nLive/Total Tabs: ${live}/${total}\nPage Telemetry:\n${domJson}`;
};
window.renderStorage = (t) => document.getElementById('appOut').textContent = t;
</script></body></html>"#;
