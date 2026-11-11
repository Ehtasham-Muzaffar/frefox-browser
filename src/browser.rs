//! Core browser window — tab strip, URL bar (Omnibox), WebView container,
//! network/adblock policy hook, and F12 / Ctrl+Shift+I DevTools integration.

use crate::adblock;
use crate::config::FeroxConfig;
use crate::devtools::DevToolsPanel;
use crate::network;
use crate::tabs::TabManager;
use anyhow::Result;
use gtk4::prelude::*;
use gtk4::{
    ApplicationWindow, Box, Button, CssProvider, Entry, Orientation, Paned, ScrolledWindow,
};
use std::cell::RefCell;
use std::rc::Rc;
use tracing::info;
use webkit6::prelude::*;
use webkit6::{PolicyDecisionType, WebContext, WebView};

/// Shared mutable browser state accessible throughout window callbacks.
pub struct BrowserState {
    pub config: FeroxConfig,
    pub tab_manager: TabManager,
}

/// Build and show the main browser window.
pub fn create_browser_window(
    app: &gtk4::Application,
    config: FeroxConfig,
) -> Result<ApplicationWindow> {
    // WebKit context (shared across all tabs in this window)
    let web_ctx = build_web_context(&config)?;

    // Shared state
    let tab_mgr = TabManager::new(config.tabs.clone())?;
    let state = Rc::new(RefCell::new(BrowserState {
        config: config.clone(),
        tab_manager: tab_mgr,
    }));

    // Main window
    let window = ApplicationWindow::builder()
        .application(app)
        .title("Ferrox Browser")
        .default_width(config.ui.window_width)
        .default_height(config.ui.window_height)
        .build();

    // Load custom CSS theme
    load_css();

    // Root layout: vertical stack
    let root = Box::new(Orientation::Vertical, 0);

    // Navigation bar (Back, Forward, Reload, Omnibox, New Tab, HW Accel toggle, DevTools)
    let (navbar, url_entry, back_btn, fwd_btn, reload_btn, new_tab_btn, hw_btn, devtools_btn) =
        build_navbar(config.ui.hardware_acceleration);
    root.append(&navbar);

    // Tab strip
    let tab_strip = Box::new(Orientation::Horizontal, 2);
    tab_strip.add_css_class("tab-strip");
    root.append(&tab_strip);

    // Initial WebView
    let webview = build_webview(&web_ctx, &config);
    let tab_id = state
        .borrow_mut()
        .tab_manager
        .open_tab(&config.ui.homepage, webview.clone());
    state.borrow_mut().tab_manager.focus_tab(tab_id);

    let webview_scroll = ScrolledWindow::new();
    webview_scroll.set_vexpand(true);
    webview_scroll.set_hexpand(true);
    webview_scroll.set_child(Some(&webview));

    // Docked DevTools panel
    let devtools_panel = Rc::new(DevToolsPanel::new(webview.clone(), &config.devtools));

    // Vertical Paned: WebView on top, DevTools below (initially hidden)
    let paned = Paned::new(Orientation::Vertical);
    paned.set_vexpand(true);
    paned.set_start_child(Some(&webview_scroll));
    paned.set_end_child(Some(&devtools_panel.container));
    paned.set_position(config.ui.window_height - config.devtools.panel_height - 60);
    devtools_panel.container.set_visible(false);

    root.append(&paned);
    window.set_child(Some(&root));

    // Wire callbacks
    wire_navigation_callbacks(
        &webview,
        &url_entry,
        &back_btn,
        &fwd_btn,
        &reload_btn,
        &config,
    );
    wire_policy_callbacks(&webview, &config, devtools_panel.clone());
    wire_hw_accel_toggle(&hw_btn, &webview);
    wire_new_tab_button(&new_tab_btn, &web_ctx, &config, state.clone(), &tab_strip);
    wire_devtools_toggle(&devtools_btn, &devtools_panel.container);
    wire_keyboard_shortcuts(&window, &devtools_panel.container, &url_entry, &webview);
    wire_title_update(&webview, &window);
    wire_suspension_timer(state.clone());

    // Load homepage
    webview.load_uri(&config.ui.homepage);

    window.present();
    info!("Browser window created (active tab {})", tab_id);
    Ok(window)
}

/// Load the GTK4 CSS stylesheet if available.
fn load_css() {
    let provider = CssProvider::new();
    let css_bytes = include_str!("../assets/style.css");
    provider.load_from_data(css_bytes);
    if let Some(display) = gdk4::Display::default() {
        gtk4::style_context_add_provider_for_display(
            &display,
            &provider,
            gtk4::STYLE_PROVIDER_PRIORITY_APPLICATION,
        );
    }
}

/// Create and configure the shared WebKit context with memory limits.
fn build_web_context(cfg: &FeroxConfig) -> Result<WebContext> {
    let ctx = WebContext::default();

    // Apply cache directory & memory budgets
    crate::memory::configure_webkit_memory(&ctx, &cfg.memory);

    // Install native WebKit ad/tracker content filter rules
    network::install_content_filter(&ctx, &cfg.network);

    Ok(ctx)
}

/// Build a WebView with low-memory settings and DevTools hooks.
fn build_webview(ctx: &WebContext, cfg: &FeroxConfig) -> WebView {
    let settings = webkit6::Settings::new();

    settings.set_enable_javascript(true);
    settings.set_enable_javascript_markup(true);
    settings.set_enable_html5_local_storage(true);
    settings.set_enable_html5_database(true);
    settings.set_enable_webgl(cfg.ui.hardware_acceleration);
    settings.set_enable_accelerated_2d_canvas(cfg.ui.hardware_acceleration);
    settings.set_enable_media_stream(false); // Disabled by default to save RAM
    settings.set_enable_smooth_scrolling(true);
    settings.set_javascript_can_open_windows_automatically(false);
    settings.set_enable_dns_prefetching(true);

    // Developer tools support
    settings.set_enable_developer_extras(cfg.devtools.enabled);
    settings.set_enable_write_console_messages_to_stdout(true);

    // Hardware acceleration policy
    let hw_policy = if cfg.ui.hardware_acceleration {
        webkit6::HardwareAccelerationPolicy::OnDemand
    } else {
        webkit6::HardwareAccelerationPolicy::Never
    };
    settings.set_hardware_acceleration_policy(hw_policy);

    let webview = WebView::builder()
        .web_context(ctx)
        .settings(&settings)
        .build();

    webview.set_vexpand(true);
    webview.set_hexpand(true);
    webview
}

/// Build the navigation bar widget tuple.
fn build_navbar(
    hw_enabled: bool,
) -> (Box, Entry, Button, Button, Button, Button, Button, Button) {
    let navbar = Box::new(Orientation::Horizontal, 4);
    navbar.add_css_class("navbar");
    navbar.set_margin_top(4);
    navbar.set_margin_bottom(4);
    navbar.set_margin_start(8);
    navbar.set_margin_end(8);

    let back_btn = Button::from_icon_name("go-previous-symbolic");
    back_btn.set_tooltip_text(Some("Back (Alt+Left)"));

    let fwd_btn = Button::from_icon_name("go-next-symbolic");
    fwd_btn.set_tooltip_text(Some("Forward (Alt+Right)"));

    let reload_btn = Button::from_icon_name("view-refresh-symbolic");
    reload_btn.set_tooltip_text(Some("Reload (F5)"));

    let url_entry = Entry::new();
    url_entry.set_hexpand(true);
    url_entry.set_placeholder_text(Some("Search or enter address..."));
    url_entry.add_css_class("url-bar");

    let new_tab_btn = Button::from_icon_name("tab-new-symbolic");
    new_tab_btn.set_tooltip_text(Some("New Tab (Ctrl+T)"));

    let hw_btn = Button::with_label(if hw_enabled { "GPU:ON" } else { "GPU:OFF" });
    hw_btn.set_tooltip_text(Some("Toggle Hardware-Accelerated Rendering"));

    let devtools_btn = Button::from_icon_name("applications-engineering-symbolic");
    devtools_btn.set_tooltip_text(Some("DevTools (F12 / Ctrl+Shift+I)"));

    navbar.append(&back_btn);
    navbar.append(&fwd_btn);
    navbar.append(&reload_btn);
    navbar.append(&url_entry);
    navbar.append(&new_tab_btn);
    navbar.append(&hw_btn);
    navbar.append(&devtools_btn);

    (
        navbar,
        url_entry,
        back_btn,
        fwd_btn,
        reload_btn,
        new_tab_btn,
        hw_btn,
        devtools_btn,
    )
}

fn wire_navigation_callbacks(
    webview: &WebView,
    url_entry: &Entry,
    back_btn: &Button,
    fwd_btn: &Button,
    reload_btn: &Button,
    cfg: &FeroxConfig,
) {
    {
        let wv = webview.clone();
        back_btn.connect_clicked(move |_| wv.go_back());
    }
    {
        let wv = webview.clone();
        fwd_btn.connect_clicked(move |_| wv.go_forward());
    }
    {
        let wv = webview.clone();
        reload_btn.connect_clicked(move |_| wv.reload());
    }
    {
        let wv = webview.clone();
        let net_cfg = cfg.network.clone();
        url_entry.connect_activate(move |entry| {
            let mut uri = entry.text().trim().to_string();
            if uri.is_empty() {
                return;
            }
            if !uri.contains("://") && !uri.starts_with("about:") {
                if uri.contains('.') && !uri.contains(' ') {
                    uri = format!("https://{uri}");
                } else {
                    uri = format!(
                        "https://search.brave.com/search?q={}",
                        urlencoding_encode(&uri)
                    );
                }
            }
            if let Some(secure_uri) = network::enforce_https(&uri, &net_cfg) {
                uri = secure_uri;
            }
            wv.load_uri(&uri);
        });
    }
    {
        let url_e = url_entry.clone();
        webview.connect_load_changed(move |wv, event| {
            if event == webkit6::LoadEvent::Committed {
                if let Some(uri) = wv.uri() {
                    url_e.set_text(uri.as_str());
                }
            }
        });
    }
}

/// Wire WebKit navigation & resource policy interception for AdBlock + HTTPS-only + DevTools network log.
fn wire_policy_callbacks(
    webview: &WebView,
    cfg: &FeroxConfig,
    devtools: Rc<DevToolsPanel>,
) {
    let adblock_on = cfg.network.adblock_enabled;
    let net_cfg = cfg.network.clone();

    webview.connect_decide_policy(move |wv, decision, decision_type| {
        if decision_type == PolicyDecisionType::NavigationAction
            || decision_type == PolicyDecisionType::NewWindowAction
        {
            if let Some(nav_decision) = decision.downcast_ref::<webkit6::NavigationPolicyDecision>() {
                if let Some(action) = nav_decision.navigation_action() {
                    if let Some(req) = action.request() {
                        if let Some(uri) = req.uri() {
                            let uri_str = uri.as_str();
                            if adblock_on && adblock::is_blocked(uri_str) {
                                devtools.log_network_request("GET", uri_str, "BLOCKED (AdBlock)");
                                decision.ignore();
                                return true;
                            }
                            if let Some(https_uri) = network::enforce_https(uri_str, &net_cfg) {
                                decision.ignore();
                                wv.load_uri(&https_uri);
                                return true;
                            }
                            devtools.log_network_request("NAV", uri_str, "ALLOWED");
                        }
                    }
                }
            }
        }
        false
    });
}

fn wire_hw_accel_toggle(btn: &Button, webview: &WebView) {
    let wv = webview.clone();
    btn.connect_clicked(move |b| {
        if let Some(settings) = webkit6::prelude::WebViewExt::settings(&wv) {
            let currently_never =
                settings.hardware_acceleration_policy() == webkit6::HardwareAccelerationPolicy::Never;
            if currently_never {
                settings.set_hardware_acceleration_policy(
                    webkit6::HardwareAccelerationPolicy::OnDemand,
                );
                settings.set_enable_webgl(true);
                settings.set_enable_accelerated_2d_canvas(true);
                b.set_label("GPU:ON");
            } else {
                settings.set_hardware_acceleration_policy(
                    webkit6::HardwareAccelerationPolicy::Never,
                );
                settings.set_enable_webgl(false);
                settings.set_enable_accelerated_2d_canvas(false);
                b.set_label("GPU:OFF");
            }
        }
    });
}

fn wire_new_tab_button(
    btn: &Button,
    web_ctx: &WebContext,
    cfg: &FeroxConfig,
    state: Rc<RefCell<BrowserState>>,
    tab_strip: &Box,
) {
    let ctx = web_ctx.clone();
    let cfg = cfg.clone();
    let strip = tab_strip.clone();
    btn.connect_clicked(move |_| {
        let wv = build_webview(&ctx, &cfg);
        wv.load_uri(&cfg.ui.homepage);
        let id = state.borrow_mut().tab_manager.open_tab(&cfg.ui.homepage, wv);
        let tab_btn = Button::with_label(&format!("Tab {}", &id.to_string()[..4]));
        tab_btn.add_css_class("tab");
        strip.append(&tab_btn);
        info!("Spawned background tab {}", id);
    });
}

fn wire_devtools_toggle(btn: &Button, panel: &impl IsA<gtk4::Widget>) {
    let p = panel.clone();
    btn.connect_clicked(move |_| {
        p.set_visible(!p.is_visible());
    });
}

fn wire_keyboard_shortcuts(
    window: &ApplicationWindow,
    panel: &impl IsA<gtk4::Widget>,
    url_entry: &Entry,
    webview: &WebView,
) {
    let p = panel.clone();
    let entry = url_entry.clone();
    let wv = webview.clone();
    let controller = gtk4::EventControllerKey::new();
    controller.connect_key_pressed(move |_, key, _code, mods| {
        use gdk4::{Key, ModifierType};
        let ctrl_shift = mods.contains(ModifierType::CONTROL_MASK | ModifierType::SHIFT_MASK);
        let ctrl = mods.contains(ModifierType::CONTROL_MASK);

        match key {
            Key::F12 => {
                p.set_visible(!p.is_visible());
                glib::Propagation::Stop
            }
            Key::I | Key::i if ctrl_shift => {
                p.set_visible(!p.is_visible());
                glib::Propagation::Stop
            }
            Key::l | Key::L if ctrl => {
                entry.grab_focus();
                glib::Propagation::Stop
            }
            Key::F5 => {
                wv.reload();
                glib::Propagation::Stop
            }
            Key::r | Key::R if ctrl => {
                wv.reload();
                glib::Propagation::Stop
            }
            _ => glib::Propagation::Proceed,
        }
    });
    window.add_controller(controller);
}

fn wire_title_update(webview: &WebView, window: &ApplicationWindow) {
    let win = window.clone();
    webview.connect_title_notify(move |wv| {
        if let Some(title) = wv.title() {
            win.set_title(Some(&format!("{title} - Ferrox Browser")));
        }
    });
}

fn wire_suspension_timer(state: Rc<RefCell<BrowserState>>) {
    glib::timeout_add_seconds_local(60, move || {
        state.borrow_mut().tab_manager.tick_suspension_policy();
        glib::ControlFlow::Continue
    });
}

fn urlencoding_encode(s: &str) -> String {
    s.chars()
        .map(|c| match c {
            'A'..='Z' | 'a'..='z' | '0'..='9' | '-' | '_' | '.' | '~' => c.to_string(),
            ' ' => '+'.to_string(),
            _ => format!("%{:02X}", c as u32),
        })
        .collect()
}

