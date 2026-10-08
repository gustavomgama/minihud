pub mod etw;

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// Recompute cadence, global default: 200ms. The lock holds are
/// microseconds and ETW delivery is ~1s-batched anyway, so recomputing
/// faster just re-reads identical data; slower makes the HUD feel
/// dead next to 200ms HW rows. Single value, no active/idle split.
const TOP_TTL: Duration = Duration::from_millis(200);

/// One present-producing process, ranked by recent present rate.
#[derive(Clone, Debug)]
pub struct AppFrame {
    pub pid: u32,
    pub name: String,
    pub fps: f32,
    pub avg_ms: f32,
    /// Detected graphics API ("D3D12", "D3D11", "Vulkan", …), "--" if unknown.
    pub api: String,
    /// Newest ≤120 frame intervals in ms, chronological. Feeds the HUD
    /// frametime graph so it shows the GAME, never the overlay itself.
    pub recent_ms: Vec<f32>,
}

/// Cached per-process identity: exe name + detected graphics API.
/// Resolved once (single OpenProcess) and kept; dead pids prune with
/// their event queues.
#[derive(Clone, Debug, Default)]
struct ProcInfo {
    name: String,
    api: Option<String>,
}

#[derive(Default)]
struct State {
    /// Present_Start QPC timestamps per pid (cap + prune in `top`).
    /// QPC ticks from the event header, NOT callback arrival time:
    /// real-time delivery batches events, so arrival times cluster.
    events: HashMap<u32, VecDeque<i64>>,
    infos: HashMap<u32, ProcInfo>,
    /// ETW session failed (e.g. not elevated). Shown in HUD as placeholder.
    session_err: Option<String>,
    /// Newest event timestamp seen. Windows are relative to this
    /// watermark, not to wall-clock QPC: real-time delivery can lag
    /// seconds behind (observed 2.3s), which empties wall-clock windows
    /// even while totals climb. Watermark windows stay correct.
    max_stamp: i64,
}

fn qpc_freq() -> i64 {
    static FREQ: OnceLock<i64> = OnceLock::new();
    *FREQ.get_or_init(|| {
        use windows::Win32::System::Performance::QueryPerformanceFrequency;
        let mut f = 0i64;
        unsafe {
            let _ = QueryPerformanceFrequency(&mut f);
        }
        if f <= 0 {
            10_000_000 // QPC fallback frequency
        } else {
            f
        }
    })
}

/// CLI process target: `--process Overwatch.exe` or `--process 1234`.
/// Name matches case-insensitively by substring ("overwatch" hits
/// "Overwatch.exe"); a pure number matches the PID exactly.
#[derive(Clone, Debug)]
pub enum ProcessFilter {
    Pid(u32),
    Name(String),
}

impl ProcessFilter {
    pub fn parse(arg: &str) -> Self {
        match arg.parse::<u32>() {
            Ok(pid) => Self::Pid(pid),
            Err(_) => Self::Name(arg.to_lowercase()),
        }
    }

    /// Short label for HUD placeholders ("Overwatch.exe" / "pid 1234").
    pub fn label(&self) -> String {
        match self {
            Self::Pid(pid) => format!("pid {pid}"),
            Self::Name(n) => n.clone(),
        }
    }
}

#[derive(Clone)]
pub struct AppTracker {
    state: Arc<Mutex<State>>,
    cache: Arc<Mutex<(Instant, Option<AppFrame>)>>,
    /// PID currently shown. It keeps its slot while it has ANY event in
    /// the 2s prune horizon; only 2s of total silence drops it back to
    /// "-- (listening…)". Newcomers still need >= 2 presents in 1s.
    incumbent: Arc<Mutex<Option<u32>>>,
}

impl AppTracker {
    /// Spawns the ETW consumer thread and returns immediately.
    /// Until the first events arrive (or if the session fails),
    /// `top()` returns None and `status_text()` explains why.
    pub fn start() -> Self {
        let tracker = Self {
            state: Arc::new(Mutex::new(State::default())),
            cache: Arc::new(Mutex::new((Instant::now() - TOP_TTL, None))),
            incumbent: Arc::new(Mutex::new(None)),
        };
        let state = tracker.state.clone();
        std::thread::spawn(move || etw::run(state));
        tracker
    }

    /// Highest-rate present producer excluding `exclude_pid` (our own HUD).
    /// Needs >= 2 presents in the last second; prunes data older than 2s.
    /// With a filter, only the targeted process is considered.
    /// Result is cached for 500ms so per-frame HUD reads don't contend
    /// with the ETW delivery thread.
    pub fn top(&self, exclude_pid: u32, filter: Option<&ProcessFilter>) -> Option<AppFrame> {
        if let Ok(cache) = self.cache.lock() {
            if cache.0.elapsed() < TOP_TTL {
                return cache.1.clone();
            }
        }
        let fresh = self.compute_top(exclude_pid, filter);
        if let Ok(mut cache) = self.cache.lock() {
            *cache = (Instant::now(), fresh.clone());
        }
        fresh
    }

    /// True when `pid` is eligible: not us, and matching the filter.
    /// Name matching resolves+caches process info (cheap: cached).
    fn eligible(
        infos: &mut HashMap<u32, ProcInfo>,
        pid: u32,
        exclude_pid: u32,
        filter: Option<&ProcessFilter>,
    ) -> bool {
        if pid == exclude_pid {
            return false;
        }
        match filter {
            None => true,
            Some(ProcessFilter::Pid(p)) => pid == *p,
            Some(ProcessFilter::Name(q)) => {
                if let Some(info) = infos.get(&pid) {
                    return info.name.to_lowercase().contains(q.as_str());
                }
                let info = Self::resolve_info(infos, pid);
                info.name.to_lowercase().contains(q.as_str())
            }
        }
    }

    fn compute_top(&self, exclude_pid: u32, filter: Option<&ProcessFilter>) -> Option<AppFrame> {
        let mut st = self.state.lock().ok()?;
        // Windows relative to the newest event seen, not wall clock:
        // delivery can lag seconds, which would empty wall-clock windows
        // even while totals climb.
        let mark = st.max_stamp;
        if mark <= 0 {
            return None;
        }
        let freq = qpc_freq() as f64;
        let keep_from = mark - 2 * qpc_freq();
        let win_from = mark - qpc_freq();
        let mut best: Option<(u32, usize)> = None;
        // Prune + rank. Single pass; map is tiny (one entry per presenter).
        // Reborrow through the guard once so `events`/`infos` are
        // disjoint field borrows (two direct `&mut st.x` borrows alias
        // through DerefMut and won't compile).
        let st_ref: &mut State = &mut st;
        let (events, infos) = (&mut st_ref.events, &mut st_ref.infos);
        let pids: Vec<u32> = events.keys().copied().collect();
        for pid in pids {
            let empty = match events.get_mut(&pid) {
                Some(q) => {
                    while q.front().is_some_and(|t| *t < keep_from) {
                        q.pop_front();
                    }
                    q.is_empty()
                }
                None => continue,
            };
            if empty {
                events.remove(&pid);
                continue;
            }
            if !Self::eligible(infos, pid, exclude_pid, filter) {
                continue;
            }
            let n = events
                .get(&pid)
                .map(|q| q.iter().filter(|t| **t >= win_from).count())
                .unwrap_or(0);
            if n >= 2 && best.is_none_or(|(_, bn)| n > bn) {
                best = Some((pid, n));
            }
        }
        // Winner, or the incumbent if it is still alive (any event in the
        // 2s horizon). This stops flapping on sparse stretches: the row
        // shows the dip honestly instead of blanking to listening.
        let pid = match best {
            Some((pid, _)) => {
                if let Ok(mut inc) = self.incumbent.lock() {
                    *inc = Some(pid);
                }
                pid
            }
            None => {
                let inc = self.incumbent.lock().ok().and_then(|g| *g);
                match inc {
                    // Incumbent needs >= 1 present in the 1s window (and
                    // must still match the filter) to stay displayed;
                    // 0 means over a second of silence, which reads
                    // honestly as no-presents, not "0fps".
                    Some(p)
                        if Self::eligible(&mut st.infos, p, exclude_pid, filter)
                            && st.events.get(&p).is_some_and(|q| {
                                q.iter().filter(|t| **t >= win_from).count() >= 1
                            }) =>
                    {
                        p
                    }
                    _ => {
                        if let Ok(mut g) = self.incumbent.lock() {
                            *g = None;
                        }
                        return None;
                    }
                }
            }
        };
        let n = st
            .events
            .get(&pid)
            .map(|q| q.iter().filter(|t| **t >= win_from).count())
            .unwrap_or(0);
        let q = st.events.get(&pid)?;
        // avg_ms from consecutive QPC intervals inside the 1s window,
        // plus the newest intervals for the HUD frametime graph.
        let mut sum = 0.0f64;
        let mut gaps = 0u32;
        let mut prev: Option<i64> = None;
        let mut recent_ms: Vec<f32> = Vec::new();
        for t in q.iter().filter(|t| **t >= win_from) {
            if let Some(p) = prev {
                let dt = *t - p;
                if dt > 0 {
                    let ms = dt as f64 * 1000.0 / freq;
                    sum += ms;
                    gaps += 1;
                    recent_ms.push(ms as f32);
                }
            }
            prev = Some(*t);
        }
        if recent_ms.len() > 120 {
            recent_ms.drain(..recent_ms.len() - 120);
        }
        let avg_ms = if gaps > 0 { sum / gaps as f64 } else { 0.0 } as f32;
        let info = st
            .infos
            .get(&pid)
            .cloned()
            .unwrap_or_else(|| Self::resolve_info(&mut st.infos, pid));
        Some(AppFrame {
            pid,
            name: info.name,
            api: info.api.unwrap_or_else(|| "--".to_string()),
            fps: n as f32,
            avg_ms,
            recent_ms,
        })
    }

    /// Top presenters in the last second as (name, pid, count, api), up
    /// to 4. For logs only: settles "who is actually presenting" and
    /// "which API did we detect" arguments the APP row can't.
    pub fn snapshot(&self) -> Vec<(String, u32, usize, String)> {
        let mut st = match self.state.lock() {
            Ok(g) => g,
            Err(_) => return Vec::new(),
        };
        let mark = st.max_stamp;
        if mark <= 0 {
            return Vec::new();
        }
        let win_from = mark - qpc_freq();
        let mut rows: Vec<(String, u32, usize, String)> = Vec::new();
        let pids: Vec<u32> = st.events.keys().copied().collect();
        for pid in pids {
            let n = st
                .events
                .get(&pid)
                .map(|q| q.iter().filter(|t| **t >= win_from).count())
                .unwrap_or(0);
            if n == 0 {
                continue;
            }
            let info = st.infos.get(&pid).cloned().unwrap_or_else(|| {
                let info = process_info(pid).unwrap_or_else(|| ProcInfo {
                    name: format!("pid {pid}"),
                    ..Default::default()
                });
                let out = info.clone();
                st.infos.insert(pid, info);
                out
            });
            rows.push((
                info.name,
                pid,
                n,
                info.api.unwrap_or_else(|| "?".to_string()),
            ));
        }
        rows.sort_by(|a, b| b.2.cmp(&a.2));
        rows.truncate(4);
        rows
    }

    /// Placeholder line when no app data yet: "APP  -- (run as admin)" etc.
    /// With a process filter the placeholder names the target so a silent
    /// game is distinguishable from a dead session.
    pub fn status_text(&self, filter: Option<&ProcessFilter>) -> String {
        if let Some(err) = self.state.lock().ok().and_then(|st| st.session_err.clone()) {
            return format!("APP  -- ({err})");
        }
        match filter {
            Some(f) => format!("APP  {} -- (no presents)", f.label()),
            None => "APP  -- (listening…)".to_string(),
        }
    }

    fn resolve_info(infos: &mut HashMap<u32, ProcInfo>, pid: u32) -> ProcInfo {
        let info = process_info(pid).unwrap_or_else(|| ProcInfo {
            name: format!("pid {pid}"),
            ..Default::default()
        });
        infos.insert(pid, info.clone());
        // Drop cached infos for dead processes so the map stays tiny.
        if infos.len() > 64 {
            infos.retain(|_, _| false);
            infos.insert(pid, info.clone());
        }
        info
    }
}

/// Open a process once and read both its exe name and its graphics API
/// (from loaded runtime dlls). Full access for the module list, limited
/// fallback for the name alone.
fn process_info(pid: u32) -> Option<ProcInfo> {
    use windows::Win32::Foundation::{CloseHandle, HMODULE};
    use windows::Win32::System::ProcessStatus::{EnumProcessModules, GetModuleBaseNameW};
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_INFORMATION,
        PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_VM_READ,
    };
    unsafe {
        // Try full access first (name + modules in one handle).
        match OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ, false, pid) {
            Ok(h) => {
                let name = image_name(h).unwrap_or_else(|| format!("pid {pid}"));
                let api = detect_api(h);
                if api.is_none() {
                    // Handle worked, modules didn't (or no known runtime
                    // loaded yet): one line, then "--" stands honestly.
                    tracing::info!("appstats: no known graphics runtime in pid {pid} ({name})");
                }
                let _ = CloseHandle(h);
                return Some(ProcInfo { name, api });
            }
            Err(e) => {
                // Typically OS error 5: higher-integrity or protected
                // target. The name still resolves via the limited
                // fallback; the API is unknowable from outside.
                tracing::info!("appstats: pid {pid} denies full access ({e:?}); api unknown");
            }
        }
        // Protected process: name only. API stays unknown, not guessed.
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let name = image_name(h).unwrap_or_else(|| format!("pid {pid}"));
        let _ = CloseHandle(h);
        Some(ProcInfo { name, api: None })
    }
}

/// Highest loaded runtime wins: D3D12 > Vulkan > D3D11 > D3D9 > OpenGL.
/// dxgi.dll alone says nothing (it loads for all D3D10+), hence this.
fn detect_api(hprocess: windows::Win32::Foundation::HANDLE) -> Option<String> {
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::System::ProcessStatus::{EnumProcessModules, GetModuleBaseNameW};
    // (dll substring, label) in preference order.
    const RUNTIMES: &[(&str, &str)] = &[
        ("d3d12.dll", "D3D12"),
        ("vulkan-1.dll", "Vulkan"),
        ("d3d11.dll", "D3D11"),
        ("d3d9.dll", "D3D9"),
        ("opengl32.dll", "OpenGL"),
    ];
    unsafe {
        // Room for thousands: UE games load hundreds of DLLs and a
        // 128-slot cap silently truncated the runtime list (this exact
        // bug hid d3d12.dll once already).
        let mut mods = vec![HMODULE::default(); 2048];
        let mut needed = 0u32;
        if EnumProcessModules(
            hprocess,
            mods.as_mut_ptr(),
            (mods.len() * std::mem::size_of::<HMODULE>()) as u32,
            &mut needed,
        )
        .is_err()
        {
            return None;
        }
        let count = (needed as usize / std::mem::size_of::<HMODULE>()).min(mods.len());
        let mut loaded: Vec<String> = Vec::with_capacity(count);
        for m in mods.iter().take(count) {
            let mut buf = [0u16; 64];
            let n = GetModuleBaseNameW(hprocess, Some(*m), &mut buf);
            if n > 0 {
                loaded.push(String::from_utf16_lossy(&buf[..n as usize]).to_lowercase());
            }
        }
        for (dll, label) in RUNTIMES {
            if loaded.iter().any(|m| m.contains(dll)) {
                return Some(label.to_string());
            }
        }
        None
    }
}

fn image_name(hprocess: windows::Win32::Foundation::HANDLE) -> Option<String> {
    use windows::Win32::System::Threading::{QueryFullProcessImageNameW, PROCESS_NAME_WIN32};
    unsafe {
        let mut buf = [0u16; 512];
        let mut len = buf.len() as u32;
        QueryFullProcessImageNameW(
            hprocess,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
        .ok()?;
        let full = String::from_utf16_lossy(&buf[..len as usize]);
        Some(full.rsplit(['\\', '/']).next().unwrap_or(&full).to_string())
    }
}
