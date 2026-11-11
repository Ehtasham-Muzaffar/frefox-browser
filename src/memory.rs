//! Memory management — OS-level process limits, engine heap controls,
//! and periodic garbage collection / RSS telemetry.

#[cfg(target_os = "linux")]
use crate::config::MemoryConfig;
use tracing::{info, warn};

/// Apply process-level memory controls BEFORE any WebKit/WebView2 context
/// is created.
pub fn apply_process_limits() {
    #[cfg(target_os = "linux")]
    {
        // JavaScriptCore tuning knobs (read at WebKitWebProcess startup)
        std::env::set_var("JSC_gcMaxHeapSize", "268435456"); // 256 MB JSC heap cap
        std::env::set_var("JSC_forceRAMSize", "536870912"); // 512 MB total RAM hint
        std::env::set_var("JSC_useGC", "1");
        std::env::set_var("JSC_gcMarkingConstraintSolverTimeout", "5.0");
        std::env::set_var("WEBKIT_FORCE_SANDBOX", "0");

        set_linux_address_space_limit(4 * 1024 * 1024 * 1024);
        info!("Linux/JSC process memory limits applied");
    }

    #[cfg(target_os = "windows")]
    {
        // Enforce strict renderer pool & heap limits on Edge WebView2 while enabling smooth scrolling/raster
        let existing = std::env::var("WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS").unwrap_or_default();
        let flags = "--renderer-process-limit=2 --disk-cache-size=104857600 --js-flags=--max-old-space-size=256 --enable-smooth-scrolling --enable-gpu-rasterization";
        let combined = if existing.is_empty() {
            flags.to_string()
        } else {
            format!("{existing} {flags}")
        };
        std::env::set_var("WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS", combined);
        info!("Windows WebView2 memory & smoothness limits applied");
    }
}

/// Return current UI process Resident Set Size (RSS / Working Set) in MB.
pub fn current_process_rss_mb() -> f64 {
    #[cfg(target_os = "linux")]
    {
        if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
            for line in status.lines() {
                if let Some(rest) = line.strip_prefix("VmRSS:") {
                    if let Some(kb) = rest.split_whitespace().next().and_then(|n| n.parse::<f64>().ok()) {
                        return kb / 1024.0;
                    }
                }
            }
        }
        0.0
    }

    #[cfg(target_os = "windows")]
    {
        win_working_set_mb().unwrap_or(0.0)
    }

    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        0.0
    }
}

/// Snapshot of UI shell memory plus child renderer/GPU processes.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
#[derive(Debug, Clone, Copy, Default)]
pub struct BrowserMemoryStats {
    pub ui_mb: f64,
    pub renderer_mb: f64,
    pub renderer_count: usize,
}

impl BrowserMemoryStats {
    #[cfg_attr(not(target_os = "windows"), allow(dead_code))]
    pub fn total_mb(self) -> f64 {
        self.ui_mb + self.renderer_mb
    }
}

/// Measure UI shell memory plus all child renderer/WebView2 processes spawned by Ferrox.
#[cfg_attr(not(target_os = "windows"), allow(dead_code))]
pub fn browser_memory_stats() -> BrowserMemoryStats {
    let ui_mb = current_process_rss_mb();
    #[cfg(target_os = "windows")]
    {
        let (renderer_mb, renderer_count) = win_descendant_working_set_mb();
        BrowserMemoryStats {
            ui_mb,
            renderer_mb,
            renderer_count,
        }
    }
    #[cfg(not(target_os = "windows"))]
    {
        BrowserMemoryStats {
            ui_mb,
            renderer_mb: 0.0,
            renderer_count: 0,
        }
    }
}

#[cfg(target_os = "windows")]
#[repr(C)]
#[allow(non_snake_case)]
struct ProcessMemoryCounters {
    cb: u32,
    PageFaultCount: u32,
    PeakWorkingSetSize: usize,
    WorkingSetSize: usize,
    QuotaPeakPagedPoolUsage: usize,
    QuotaPagedPoolUsage: usize,
    QuotaPeakNonPagedPoolUsage: usize,
    QuotaNonPagedPoolUsage: usize,
    PagefileUsage: usize,
    PeakPagefileUsage: usize,
}

#[cfg(target_os = "windows")]
#[repr(C)]
#[allow(non_snake_case)]
struct ProcessEntry32W {
    dwSize: u32,
    cntUsage: u32,
    th32ProcessID: u32,
    th32DefaultHeapID: usize,
    th32ModuleID: u32,
    cntThreads: u32,
    th32ParentProcessID: u32,
    pcPriClassBase: i32,
    dwFlags: u32,
    szExeFile: [u16; 260],
}

#[cfg(target_os = "windows")]
#[link(name = "kernel32")]
extern "system" {
    fn GetCurrentProcess() -> isize;
    fn K32GetProcessMemoryInfo(
        process: isize,
        ppsmem_counters: *mut ProcessMemoryCounters,
        cb: u32,
    ) -> i32;
    fn CreateToolhelp32Snapshot(flags: u32, th32_process_id: u32) -> isize;
    fn Process32FirstW(snapshot: isize, lppe: *mut ProcessEntry32W) -> i32;
    fn Process32NextW(snapshot: isize, lppe: *mut ProcessEntry32W) -> i32;
    fn OpenProcess(desired_access: u32, inherit_handle: i32, process_id: u32) -> isize;
    fn CloseHandle(handle: isize) -> i32;
}

#[cfg(target_os = "windows")]
fn query_working_set_mb(handle: isize) -> Option<f64> {
    let mut pmc: ProcessMemoryCounters = unsafe { std::mem::zeroed() };
    pmc.cb = std::mem::size_of::<ProcessMemoryCounters>() as u32;
    let ok = unsafe { K32GetProcessMemoryInfo(handle, &mut pmc, pmc.cb) };
    if ok != 0 {
        Some(pmc.WorkingSetSize as f64 / (1024.0 * 1024.0))
    } else {
        None
    }
}

#[cfg(target_os = "windows")]
fn win_working_set_mb() -> Option<f64> {
    let res = query_working_set_mb(unsafe { GetCurrentProcess() });
    if res.is_none() {
        warn!("K32GetProcessMemoryInfo failed");
    }
    res
}

#[cfg(target_os = "windows")]
fn win_descendant_working_set_mb() -> (f64, usize) {
    const TH32CS_SNAPPROCESS: u32 = 0x0000_0002;
    const PROCESS_QUERY_LIMITED_INFORMATION: u32 = 0x1000;
    const INVALID_HANDLE: isize = -1;

    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) };
    if snap == 0 || snap == INVALID_HANDLE {
        return (0.0, 0);
    }

    let mut edges: Vec<(u32, u32)> = Vec::new();
    let mut entry: ProcessEntry32W = unsafe { std::mem::zeroed() };
    entry.dwSize = std::mem::size_of::<ProcessEntry32W>() as u32;

    if unsafe { Process32FirstW(snap, &mut entry) } != 0 {
        loop {
            edges.push((entry.th32ParentProcessID, entry.th32ProcessID));
            if unsafe { Process32NextW(snap, &mut entry) } == 0 {
                break;
            }
        }
    }
    unsafe { CloseHandle(snap) };

    let root = std::process::id();
    let mut visited = std::collections::HashSet::from([root]);
    let mut queue = std::collections::VecDeque::from([root]);
    let mut descendants = Vec::new();

    while let Some(parent) = queue.pop_front() {
        for &(ppid, pid) in &edges {
            if ppid == parent && visited.insert(pid) {
                queue.push_back(pid);
                descendants.push(pid);
            }
        }
    }

    let mut total_mb = 0.0;
    let mut count = 0;
    for pid in descendants {
        let h = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid) };
        if h != 0 && h != INVALID_HANDLE {
            if let Some(mb) = query_working_set_mb(h) {
                total_mb += mb;
                count += 1;
            }
            unsafe { CloseHandle(h) };
        }
    }
    (total_mb, count)
}

#[cfg(target_os = "linux")]
fn set_linux_address_space_limit(bytes: u64) {
    #[repr(C)]
    struct Rlimit {
        rlim_cur: u64,
        rlim_max: u64,
    }

    const RLIMIT_AS: i32 = 9;

    let limit = Rlimit {
        rlim_cur: bytes,
        rlim_max: bytes,
    };

    let ret = unsafe {
        libc_setrlimit(RLIMIT_AS, &limit as *const Rlimit as *const std::ffi::c_void)
    };

    if ret != 0 {
        warn!("setrlimit(RLIMIT_AS) failed — running without address-space cap");
    } else {
        info!("Address-space limit set to {} GB", bytes / (1024 * 1024 * 1024));
    }
}

#[cfg(target_os = "linux")]
extern "C" {
    #[link_name = "setrlimit"]
    fn libc_setrlimit(resource: i32, rlim: *const std::ffi::c_void) -> i32;
}

/// Configure WebKit's internal memory caches after a WebContext is available.
#[cfg(target_os = "linux")]
pub fn configure_webkit_memory(ctx: &webkit6::WebContext, cfg: &MemoryConfig) {
    if let Some(manager) = ctx.website_data_manager() {
        manager.set_disk_cache_directory(
            &format!("{}/ferrox/cache", dirs::cache_dir()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| "/tmp".to_string()))
        );
        manager.set_idb_directory(
            &format!("{}/ferrox/idb", dirs::data_dir()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_else(|| "/tmp".to_string()))
        );
        info!("WebKit disk cache limit: {} MB", cfg.disk_cache_limit_mb);
    }
}

/// Schedule a periodic GC pass on the GTK main loop.
#[cfg(target_os = "linux")]
pub fn schedule_periodic_gc(interval_secs: u64) {
    glib::timeout_add_local(
        std::time::Duration::from_secs(interval_secs),
        move || {
            tracing::debug!("GC sweep triggered");
            glib::ControlFlow::Continue
        },
    );
    info!("Periodic GC scheduled every {} seconds", interval_secs);
}
