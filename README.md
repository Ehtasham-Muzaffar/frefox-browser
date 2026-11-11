# Ferrox Browser

Ultra-lightweight, high-performance web browser built in **Rust**, targeting 8 GB RAM systems with:
- **Linux Backend:** GTK4 + WebKitGTK (`webkit6`) with JSC heap caps (`JSC_gcMaxHeapSize`), shared `WebProcess` pool, and native `UserContentFilterStore` ad-blocking.
- **Windows Backend:** `tao` + `wry` (Edge WebView2) with `--renderer-process-limit=2`, 256 MB JS heap cap, and Working Set telemetry.

## Core Features
- **Automatic Tab Hibernation (`TabManager`):** Suspends inactive background tabs to disk (`suspended_tabs/<uuid>.json`) and frees their WebView/DOM heap; restores on click.
- **Built-in Ad/Tracker Blocker (`AdBlocker`):** O(n) Aho-Corasick URL matcher blocking ad/tracker payloads at the navigation/policy layer.
- **Omnibox, Bookmarks & History (`BrowserStorage`):** Smart URL/search routing, bookmark toggling (`Ctrl+D`), and bounded JSON history autocomplete.
- **Docked Developer Tools (`F12` / `Ctrl+Shift+I`):** Elements/DOM & Computed Style inspector, interactive JS Console + live log capture, Network waterfall & throttling simulator, Memory/RSS profiler, and Storage inspector.

---

## Build & Run

### Windows (Native WebView2)
```powershell
# Prerequisites: Visual Studio Build Tools (MSVC) + Rustup + Edge WebView2 Runtime (built into Windows 10/11)
cargo test
cargo build --release
.\target\release\ferrox.exe
```

### Ubuntu Linux 24.04+ (GTK4 + WebKitGTK 6.0)
```bash
sudo apt update && sudo apt install -y \
  pkg-config libwebkitgtk-6.0-dev libgtk-4-dev libglib2.0-dev \
  libjavascriptcoregtk-6.0-dev libssl-dev build-essential curl git smem

curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source ~/.cargo/env

cargo test
cargo build --release
./target/release/ferrox
```

## Memory Verification (Linux)
```bash
./target/release/ferrox &
FERROX_PID=$!
sleep 5
grep VmRSS /proc/$FERROX_PID/status
smem -P "ferrox|WebKit" -c "pid name pss rss uss"
```
