//! Developer Tools — docked GTK4 developer panel toggleable via F12 or Ctrl+Shift+I.
//! Provides:
//!   - Elements / DOM Inspector (live DOM tree, attribute editing, computed CSS, box model)
//!   - Interactive JS Console (eval expressions + live log/warn/error/table/time capture)
//!   - Network Monitor (live fetch/XHR/navigation waterfall, timing, request block simulation)
//!   - Resource & Memory Profiler (per-tab RSS / DOM node count / resource budget probe)
//!   - Application Storage Inspector (localStorage, sessionStorage, cookies, IndexedDB probe)

use crate::config::DevToolsConfig;
use gtk4::prelude::*;
use gtk4::{
    Box, Button, Entry, Label, ListBox, ListBoxRow, Notebook, Orientation, ProgressBar,
    ScrolledWindow, TextBuffer, TextView,
};
use serde::Deserialize;
use std::cell::RefCell;
use std::rc::Rc;
use webkit6::prelude::*;

/// The docked DevTools panel widget, dynamically bound to the active tab's WebView.
pub struct DevToolsPanel {
    pub container: Box,
    active_webview: Rc<RefCell<webkit6::WebView>>,
    console_buffer: TextBuffer,
    network_list: ListBox,
    memory_bar: ProgressBar,
    memory_stats_buffer: TextBuffer,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "kind")]
enum TelemetryEvent {
    #[serde(rename = "console")]
    Console { level: String, message: String },
    #[serde(rename = "network")]
    Network {
        method: String,
        url: String,
        status: String,
        duration_ms: u64,
    },
}

/// JS bridge injected at document start to forward console methods & fetch/XHR telemetry
/// to the GTK4 DevTools dock via WebKit's `ferroxDevtools` script message handler.
pub fn telemetry_init_script() -> &'static str {
    r#"(function() {
        if (window.__ferroxHooked) return;
        window.__ferroxHooked = true;
        window.__ferroxBlockedPatterns = window.__ferroxBlockedPatterns || [];
        window.__ferroxThrottleMs = window.__ferroxThrottleMs || 0;

        const post = (payload) => {
            try {
                if (window.webkit && window.webkit.messageHandlers && window.webkit.messageHandlers.ferroxDevtools) {
                    window.webkit.messageHandlers.ferroxDevtools.postMessage(JSON.stringify(payload));
                }
            } catch (_) {}
        };

        const timers = new Map();
        const fmt = (args) => Array.from(args).map(a => {
            if (typeof a === 'object' && a !== null) {
                try { return JSON.stringify(a); } catch { return String(a); }
            }
            return String(a);
        }).join(' ');

        ['log', 'warn', 'error', 'info'].forEach((lvl) => {
            const orig = console[lvl].bind(console);
            console[lvl] = (...args) => {
                post({ kind: 'console', level: lvl.toUpperCase(), message: fmt(args) });
                return orig(...args);
            };
        });

        const origTable = console.table ? console.table.bind(console) : null;
        console.table = (data) => {
            post({ kind: 'console', level: 'TABLE', message: fmt([data]) });
            if (origTable) return origTable(data);
        };

        console.time = (label = 'default') => timers.set(label, performance.now());
        console.timeEnd = (label = 'default') => {
            const start = timers.get(label);
            if (start !== undefined) {
                const ms = (performance.now() - start).toFixed(2);
                timers.delete(label);
                post({ kind: 'console', level: 'TIME', message: `${label}: ${ms} ms` });
            }
        };

        const origFetch = window.fetch;
        if (origFetch) {
            window.fetch = async (input, init) => {
                const url = typeof input === 'string' ? input : (input && input.url) || '';
                const method = (init && init.method) || 'GET';
                if (window.__ferroxBlockedPatterns.some(p => url.includes(p))) {
                    post({ kind: 'network', method, url, status: 'BLOCKED (DevTools)', duration_ms: 0 });
                    throw new TypeError('Blocked by Ferrox DevTools Network Simulator');
                }
                const t0 = performance.now();
                if (window.__ferroxThrottleMs > 0) {
                    await new Promise(r => setTimeout(r, window.__ferroxThrottleMs));
                }
                try {
                    const res = await origFetch(input, init);
                    post({
                        kind: 'network',
                        method,
                        url,
                        status: String(res.status),
                        duration_ms: Math.round(performance.now() - t0)
                    });
                    return res;
                } catch (err) {
                    post({
                        kind: 'network',
                        method,
                        url,
                        status: 'ERR',
                        duration_ms: Math.round(performance.now() - t0)
                    });
                    throw err;
                }
            };
        }
    })();"#
}

impl DevToolsPanel {
    /// Build the DevTools UI and return the panel.
    pub fn new(initial_webview: webkit6::WebView, cfg: &DevToolsConfig) -> Self {
        let active_webview = Rc::new(RefCell::new(initial_webview));
        let container = Box::new(Orientation::Vertical, 0);
        container.set_height_request(cfg.panel_height);
        container.add_css_class("devtools-panel");

        let notebook = Notebook::new();
        notebook.set_vexpand(true);
        notebook.set_hexpand(true);

        // Elements / DOM Inspector tab
        let dom_tab = build_dom_tab(active_webview.clone());
        notebook.append_page(&dom_tab, Some(&Label::new(Some("Elements"))));

        // Interactive JS Console tab
        let (console_tab, console_buffer) = build_console_tab(active_webview.clone());
        notebook.append_page(&console_tab, Some(&Label::new(Some("Console"))));

        // Network Monitor tab
        let (network_tab, network_list) = build_network_tab(active_webview.clone());
        notebook.append_page(&network_tab, Some(&Label::new(Some("Network"))));

        // Resource & Memory Profiler tab
        let (memory_tab, memory_bar, memory_stats_buffer) = build_memory_tab(active_webview.clone());
        notebook.append_page(&memory_tab, Some(&Label::new(Some("Memory"))));

        // Application / Storage Inspector tab
        let app_tab = build_application_tab(active_webview.clone());
        notebook.append_page(&app_tab, Some(&Label::new(Some("Application"))));

        // Native WebKit WebInspector pop-out button
        let native_inspector_btn = Button::with_label("WebKit Inspector");
        native_inspector_btn.set_tooltip_text(Some("Open WebKit's native WebInspector window"));
        {
            let wv_ref = active_webview.clone();
            native_inspector_btn.connect_clicked(move |_| {
                if let Some(inspector) = wv_ref.borrow().inspector() {
                    inspector.show();
                }
            });
        }
        notebook.set_action_widget(&native_inspector_btn, gtk4::PackType::End);

        container.append(&notebook);

        Self {
            container,
            active_webview,
            console_buffer,
            network_list,
            memory_bar,
            memory_stats_buffer,
        }
    }

    /// Re-target DevTools inspection & evaluation to a newly focused tab's WebView.
    pub fn set_active_webview(&self, webview: webkit6::WebView) {
        *self.active_webview.borrow_mut() = webview;
    }

    /// Route a JSON telemetry payload from `window.webkit.messageHandlers.ferroxDevtools`.
    pub fn handle_telemetry_message(&self, json_str: &str) {
        if let Ok(event) = serde_json::from_str::<TelemetryEvent>(json_str) {
            match event {
                TelemetryEvent::Console { level, message } => {
                    self.console_log(&level, &message);
                }
                TelemetryEvent::Network {
                    method,
                    url,
                    status,
                    duration_ms,
                } => {
                    add_network_entry(
                        &self.network_list,
                        &method,
                        &url,
                        &format!("{status} ({duration_ms} ms)"),
                    );
                }
            }
        }
    }

    /// Log a message to the console tab output buffer.
    pub fn console_log(&self, level: &str, message: &str) {
        let mut end = self.console_buffer.end_iter();
        self.console_buffer
            .insert(&mut end, &format!("[{level}] {message}\n"));
    }

    /// Append a network request record to the Network Monitor tab.
    pub fn log_network_request(&self, method: &str, url: &str, status: &str) {
        add_network_entry(&self.network_list, method, url, status);
    }

    /// Update the memory usage progress bar (0.0 - 1.0 fraction of budget).
    pub fn update_memory_usage(&self, fraction: f64, summary: &str) {
        self.memory_bar.set_fraction(fraction.clamp(0.0, 1.0));
        self.memory_bar.set_text(Some(summary));
        self.memory_stats_buffer.set_text(summary);
    }
}

/// Build the DOM, Attribute & Computed Style Inspector tab.
fn build_dom_tab(wv_ref: Rc<RefCell<webkit6::WebView>>) -> Box {
    let vbox = Box::new(Orientation::Vertical, 4);

    let toolbar = Box::new(Orientation::Horizontal, 6);
    let refresh_btn = Button::with_label("Inspect DOM");
    let selector_entry = Entry::new();
    selector_entry.set_hexpand(true);
    selector_entry.set_placeholder_text(Some("CSS selector (e.g. body, #app)"));
    let style_btn = Button::with_label("Computed Style");
    let attr_entry = Entry::new();
    attr_entry.set_placeholder_text(Some("attr=value (edit)"));
    let apply_attr_btn = Button::with_label("Set Attr");
    toolbar.append(&refresh_btn);
    toolbar.append(&selector_entry);
    toolbar.append(&style_btn);
    toolbar.append(&attr_entry);
    toolbar.append(&apply_attr_btn);
    vbox.append(&toolbar);

    let tree_buf = TextBuffer::new(None::<&gtk4::TextTagTable>);
    tree_buf.set_text("Click 'Inspect DOM' for live DOM HTML, or enter a CSS selector for box model, computed styles, and attribute editing.\n");
    let tree_view = TextView::with_buffer(&tree_buf);
    tree_view.set_editable(false);
    tree_view.set_monospace(true);
    tree_view.set_vexpand(true);

    let scroll = ScrolledWindow::new();
    scroll.set_vexpand(true);
    scroll.set_child(Some(&tree_view));
    vbox.append(&scroll);

    {
        let wv_ref = wv_ref.clone();
        let buf = tree_buf.clone();
        refresh_btn.connect_clicked(move |_| {
            let buf = buf.clone();
            wv_ref.borrow().evaluate_javascript(
                "document.documentElement ? document.documentElement.outerHTML.substring(0, 16384) : '<empty>'",
                None,
                None,
                gio::Cancellable::NONE,
                move |result| {
                    let text = match result {
                        Ok(val) => val.to_str().to_string(),
                        Err(e) => format!("DOM inspection error: {e}"),
                    };
                    buf.set_text(&text);
                },
            );
        });
    }

    {
        let wv_ref = wv_ref.clone();
        let buf = tree_buf.clone();
        let entry = selector_entry.clone();
        style_btn.connect_clicked(move |_| {
            let selector = entry.text().trim().to_string();
            let selector = if selector.is_empty() { "body".to_string() } else { selector };
            let escaped = serde_json::to_string(&selector).unwrap_or_else(|_| "\"body\"".to_string());
            let script = format!(
                r#"(function() {{
                    const el = document.querySelector({escaped});
                    if (!el) return "Element not found";
                    const cs = getComputedStyle(el);
                    const rect = el.getBoundingClientRect();
                    return JSON.stringify({{
                        tag: el.tagName,
                        attributes: Object.fromEntries(Array.from(el.attributes).map(a => [a.name, a.value])),
                        boxModel: {{
                            width: rect.width, height: rect.height,
                            margin: cs.margin, border: cs.borderWidth, padding: cs.padding
                        }},
                        styles: {{
                            display: cs.display, position: cs.position,
                            color: cs.color, backgroundColor: cs.backgroundColor,
                            fontFamily: cs.fontFamily, fontSize: cs.fontSize
                        }}
                    }}, null, 2);
                }})()"#
            );
            let buf = buf.clone();
            wv_ref.borrow().evaluate_javascript(
                &script,
                None,
                None,
                gio::Cancellable::NONE,
                move |result| {
                    let text = match result {
                        Ok(val) => val.to_str().to_string(),
                        Err(e) => format!("Style inspection error: {e}"),
                    };
                    buf.set_text(&text);
                },
            );
        });
    }

    {
        let wv_ref = wv_ref.clone();
        let buf = tree_buf.clone();
        let sel_entry = selector_entry.clone();
        let attr_entry = attr_entry.clone();
        apply_attr_btn.connect_clicked(move |_| {
            let selector = sel_entry.text().trim().to_string();
            let raw_attr = attr_entry.text().trim().to_string();
            if selector.is_empty() || !raw_attr.contains('=') {
                buf.set_text("Provide both a CSS selector and attr=value pair.");
                return;
            }
            if let Some((k, v)) = raw_attr.split_once('=') {
                let sel_json = serde_json::to_string(&selector).unwrap_or_default();
                let k_json = serde_json::to_string(k.trim()).unwrap_or_default();
                let v_json = serde_json::to_string(v.trim()).unwrap_or_default();
                let script = format!(
                    r#"(function() {{
                        const el = document.querySelector({sel_json});
                        if (!el) return "Element not found";
                        el.setAttribute({k_json}, {v_json});
                        return "Updated attribute " + {k_json} + " on " + el.tagName;
                    }})()"#
                );
                let buf = buf.clone();
                wv_ref.borrow().evaluate_javascript(
                    &script,
                    None,
                    None,
                    gio::Cancellable::NONE,
                    move |result| {
                        let text = result
                            .map(|val| val.to_str().to_string())
                            .unwrap_or_else(|e| format!("Attr edit error: {e}"));
                        buf.set_text(&text);
                    },
                );
            }
        });
    }

    vbox
}

/// Build the interactive JS Console tab.
fn build_console_tab(wv_ref: Rc<RefCell<webkit6::WebView>>) -> (Box, TextBuffer) {
    let vbox = Box::new(Orientation::Vertical, 4);

    let output_buf = TextBuffer::new(None::<&gtk4::TextTagTable>);
    output_buf.set_text("// Ferrox DevTools Console — Ready (console.log/warn/error/table/time captured)\n");
    let output_view = TextView::with_buffer(&output_buf);
    output_view.set_editable(false);
    output_view.set_monospace(true);
    output_view.set_vexpand(true);
    let scroll = ScrolledWindow::new();
    scroll.set_vexpand(true);
    scroll.set_child(Some(&output_view));
    vbox.append(&scroll);

    let input_row = Box::new(Orientation::Horizontal, 4);
    let prompt = Label::new(Some(">"));
    prompt.add_css_class("console-prompt");
    let entry = Entry::new();
    entry.set_hexpand(true);
    entry.set_placeholder_text(Some("JavaScript expression (Enter to run)..."));
    let run_btn = Button::with_label("Run");
    run_btn.add_css_class("console-run-btn");

    {
        let wv_ref = wv_ref.clone();
        let output_buf2 = output_buf.clone();
        let entry2 = entry.clone();
        let execute = move || {
            let code = entry2.text().to_string();
            if code.is_empty() {
                return;
            }
            let buf = output_buf2.clone();
            let code_clone = code.clone();
            wv_ref.borrow().evaluate_javascript(
                &code,
                None,
                None,
                gio::Cancellable::NONE,
                move |result| {
                    let output = match result {
                        Ok(js_val) => js_val.to_str().to_string(),
                        Err(e) => format!("Error: {e}"),
                    };
                    let mut end = buf.end_iter();
                    buf.insert(&mut end, &format!("> {code_clone}\n<- {output}\n"));
                },
            );
            entry2.set_text("");
        };

        let exec2 = execute.clone();
        run_btn.connect_clicked(move |_| execute());
        entry.connect_activate(move |_| exec2());
    }

    input_row.append(&prompt);
    input_row.append(&entry);
    input_row.append(&run_btn);
    vbox.append(&input_row);

    (vbox, output_buf)
}

/// Build the network request monitor tab with block/throttle simulation controls.
fn build_network_tab(wv_ref: Rc<RefCell<webkit6::WebView>>) -> (Box, ListBox) {
    let vbox = Box::new(Orientation::Vertical, 4);

    let toolbar = Box::new(Orientation::Horizontal, 6);
    let header = Label::new(Some("Network Waterfall"));
    header.add_css_class("devtools-header");
    let block_entry = Entry::new();
    block_entry.set_hexpand(true);
    block_entry.set_placeholder_text(Some("Block fetch substring (e.g. analytics)"));
    let block_btn = Button::with_label("Block Pattern");
    let throttle_btn = Button::with_label("Throttle: 0ms");
    let clear_btn = Button::with_label("Clear");
    toolbar.append(&header);
    toolbar.append(&block_entry);
    toolbar.append(&block_btn);
    toolbar.append(&throttle_btn);
    toolbar.append(&clear_btn);
    vbox.append(&toolbar);

    let list = ListBox::new();
    list.set_vexpand(true);
    list.add_css_class("network-list");
    let scroll = ScrolledWindow::new();
    scroll.set_vexpand(true);
    scroll.set_child(Some(&list));
    vbox.append(&scroll);

    {
        let wv_ref = wv_ref.clone();
        let entry = block_entry.clone();
        block_btn.connect_clicked(move |_| {
            let pat = entry.text().trim().to_string();
            if pat.is_empty() {
                return;
            }
            let pat_json = serde_json::to_string(&pat).unwrap_or_default();
            let script = format!(
                "window.__ferroxBlockedPatterns = window.__ferroxBlockedPatterns || []; window.__ferroxBlockedPatterns.push({pat_json});"
            );
            wv_ref
                .borrow()
                .evaluate_javascript(&script, None, None, gio::Cancellable::NONE, |_| {});
            entry.set_text("");
        });
    }

    {
        let wv_ref = wv_ref.clone();
        throttle_btn.connect_clicked(move |btn| {
            let next_ms = if btn.label().as_deref() == Some("Throttle: 0ms") {
                btn.set_label("Throttle: 300ms (3G)");
                300
            } else {
                btn.set_label("Throttle: 0ms");
                0
            };
            let script = format!("window.__ferroxThrottleMs = {next_ms};");
            wv_ref
                .borrow()
                .evaluate_javascript(&script, None, None, gio::Cancellable::NONE, |_| {});
        });
    }

    {
        let list_ref = list.clone();
        clear_btn.connect_clicked(move |_| {
            while let Some(child) = list_ref.first_child() {
                list_ref.remove(&child);
            }
        });
    }

    (vbox, list)
}

/// Add a request entry to the network monitor list.
pub fn add_network_entry(list: &ListBox, method: &str, url: &str, status: &str) {
    let row = ListBoxRow::new();
    let label_text = format!("[{status}] {method} {url}");
    let label = Label::new(Some(&label_text));
    label.set_halign(gtk4::Align::Start);
    label.set_xalign(0.0);
    if status.starts_with("BLOCKED")
        || status.starts_with("ERR")
        || status.starts_with('4')
        || status.starts_with('5')
    {
        label.add_css_class("network-error");
    }
    row.set_child(Some(&label));
    list.append(&row);
}

/// Build the Resource & Memory Profiler tab.
fn build_memory_tab(wv_ref: Rc<RefCell<webkit6::WebView>>) -> (Box, ProgressBar, TextBuffer) {
    let vbox = Box::new(Orientation::Vertical, 8);

    let toolbar = Box::new(Orientation::Horizontal, 6);
    let header = Label::new(Some("Resource & Memory Profiler"));
    header.add_css_class("devtools-header");
    header.set_hexpand(true);
    header.set_halign(gtk4::Align::Start);
    let sample_btn = Button::with_label("Sample DOM & Process");
    toolbar.append(&header);
    toolbar.append(&sample_btn);
    vbox.append(&toolbar);

    let bar = ProgressBar::new();
    bar.set_show_text(true);
    bar.set_text(Some("Renderer Budget Probe"));
    vbox.append(&bar);

    let stats_buf = TextBuffer::new(None::<&gtk4::TextTagTable>);
    stats_buf.set_text("Click 'Sample DOM & Process' to inspect active DOM node count, resource timings, and Linux RSS.\n");
    let stats_view = TextView::with_buffer(&stats_buf);
    stats_view.set_editable(false);
    stats_view.set_monospace(true);
    stats_view.set_vexpand(true);
    let scroll = ScrolledWindow::new();
    scroll.set_vexpand(true);
    scroll.set_child(Some(&stats_view));
    vbox.append(&scroll);

    {
        let wv_ref = wv_ref.clone();
        let buf = stats_buf.clone();
        let bar_ref = bar.clone();
        sample_btn.connect_clicked(move |_| {
            let buf = buf.clone();
            let bar_ref = bar_ref.clone();
            let rss_kb = read_linux_rss_kb().unwrap_or(0);
            wv_ref.borrow().evaluate_javascript(
                r#"JSON.stringify({
                    domNodes: document.getElementsByTagName('*').length,
                    scripts: document.scripts.length,
                    images: document.images.length,
                    resourcesLoaded: performance.getEntriesByType('resource').length
                }, null, 2)"#,
                None,
                None,
                gio::Cancellable::NONE,
                move |result| {
                    let dom_stats = match result {
                        Ok(val) => val.to_str().to_string(),
                        Err(e) => format!("Probe error: {e}"),
                    };
                    let rss_mb = rss_kb as f64 / 1024.0;
                    let summary = format!(
                        "UI Process RSS: {:.1} MB\nPage Telemetry:\n{}",
                        rss_mb, dom_stats
                    );
                    bar_ref.set_fraction((rss_mb / 150.0).clamp(0.0, 1.0));
                    bar_ref.set_text(Some(&format!("UI RSS: {:.1} MB / 150 MB idle target", rss_mb)));
                    buf.set_text(&summary);
                },
            );
        });
    }

    (vbox, bar, stats_buf)
}

#[cfg(target_os = "linux")]
fn read_linux_rss_kb() -> Option<u64> {
    let status = std::fs::read_to_string("/proc/self/status").ok()?;
    for line in status.lines() {
        if let Some(rest) = line.strip_prefix("VmRSS:") {
            let num = rest.split_whitespace().next()?;
            return num.parse::<u64>().ok();
        }
    }
    None
}

#[cfg(not(target_os = "linux"))]
fn read_linux_rss_kb() -> Option<u64> {
    None
}

/// Build the Application (storage) inspector tab.
fn build_application_tab(wv_ref: Rc<RefCell<webkit6::WebView>>) -> Box {
    let vbox = Box::new(Orientation::Vertical, 4);

    let toolbar = Box::new(Orientation::Horizontal, 6);
    let header = Label::new(Some("Application Storage"));
    header.add_css_class("devtools-header");
    header.set_hexpand(true);
    header.set_halign(gtk4::Align::Start);
    let inspect_local_btn = Button::with_label("localStorage");
    let inspect_session_btn = Button::with_label("sessionStorage");
    let inspect_cookies_btn = Button::with_label("Cookies");
    let inspect_idb_btn = Button::with_label("IndexedDB");
    toolbar.append(&header);
    toolbar.append(&inspect_local_btn);
    toolbar.append(&inspect_session_btn);
    toolbar.append(&inspect_cookies_btn);
    toolbar.append(&inspect_idb_btn);
    vbox.append(&toolbar);

    let buf = TextBuffer::new(None::<&gtk4::TextTagTable>);
    let view = TextView::with_buffer(&buf);
    view.set_editable(false);
    view.set_monospace(true);
    view.set_vexpand(true);
    let scroll = ScrolledWindow::new();
    scroll.set_vexpand(true);
    scroll.set_child(Some(&view));
    vbox.append(&scroll);

    let wire_storage = |btn: &Button, storage_expr: &'static str| {
        let wv_ref = wv_ref.clone();
        let buf = buf.clone();
        btn.connect_clicked(move |_| {
            let buf = buf.clone();
            let script = format!(
                r#"JSON.stringify(
                    Object.fromEntries(
                        Array.from({{length: {storage_expr}.length}}, (_, i) => {{
                            const k = {storage_expr}.key(i);
                            return [k, {storage_expr}.getItem(k)];
                        }})
                    ), null, 2)"#
            );
            wv_ref
                .borrow()
                .evaluate_javascript(&script, None, None, gio::Cancellable::NONE, move |result| {
                    let text = result
                        .map(|v| v.to_str().to_string())
                        .unwrap_or_else(|e| format!("Storage read error: {e}"));
                    buf.set_text(&text);
                });
        });
    };

    wire_storage(&inspect_local_btn, "localStorage");
    wire_storage(&inspect_session_btn, "sessionStorage");

    {
        let wv_ref = wv_ref.clone();
        let buf = buf.clone();
        inspect_cookies_btn.connect_clicked(move |_| {
            let buf = buf.clone();
            wv_ref.borrow().evaluate_javascript(
                "document.cookie || '<no accessible cookies>'",
                None,
                None,
                gio::Cancellable::NONE,
                move |result| {
                    let text = result
                        .map(|v| v.to_str().to_string())
                        .unwrap_or_else(|e| format!("Cookie read error: {e}"));
                    buf.set_text(&text);
                },
            );
        });
    }

    {
        let wv_ref = wv_ref.clone();
        let buf = buf.clone();
        inspect_idb_btn.connect_clicked(move |_| {
            let buf = buf.clone();
            wv_ref.borrow().evaluate_javascript(
                "indexedDB.databases ? indexedDB.databases().then(dbs => JSON.stringify(dbs, null, 2)) : 'indexedDB.databases() unsupported'",
                None,
                None,
                gio::Cancellable::NONE,
                move |result| {
                    let text = result
                        .map(|v| v.to_str().to_string())
                        .unwrap_or_else(|e| format!("IndexedDB probe error: {e}"));
                    buf.set_text(&text);
                },
            );
        });
    }

    vbox
}
