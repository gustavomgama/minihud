pub mod etw;

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

/// How often the HUD may recompute the top app. The HUD calls `top()`
/// every frame (~50-115/s); without this cache each call locks the
/// shared state and the ETW callback's `try_lock` starts dropping
/// events — which shows up as flapping between the game and
/// "-- (listening…)".
const TOP_TTL: Duration = Duration::from_millis(500);

/// One present-producing process, ranked by recent present rate.
#[derive(Clone, Debug)]
pub struct AppFrame {
    pub pid: u32,
    pub name: String,
    pub fps: f32,
    pub avg_ms: f32,
}

#[derive(Default)]
struct State {
    /// Present_Start QPC timestamps per pid (cap + prune in `top`).
    /// QPC ticks from the event header, NOT callback arrival time:
    /// real-time delivery batches events, so arrival times cluster.
    events: HashMap<u32, VecDeque<i64>>,
    names: HashMap<u32, String>,
    /// ETW session failed (e.g. not elevated). Shown in HUD as placeholder.
    session_err: Option<String>,
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

fn qpc_now() -> i64 {
    use windows::Win32::System::Performance::QueryPerformanceCounter;
    let mut t = 0i64;
    unsafe {
        let _ = QueryPerformanceCounter(&mut t);
    }
    t
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
    /// Result is cached for 500ms so per-frame HUD reads don't contend
    /// with the ETW delivery thread.
    pub fn top(&self, exclude_pid: u32) -> Option<AppFrame> {
        if let Ok(cache) = self.cache.lock() {
            if cache.0.elapsed() < TOP_TTL {
                return cache.1.clone();
            }
        }
        let fresh = self.compute_top(exclude_pid);
        if let Ok(mut cache) = self.cache.lock() {
            *cache = (Instant::now(), fresh.clone());
        }
        fresh
    }

    fn compute_top(&self, exclude_pid: u32) -> Option<AppFrame> {
        let mut st = self.state.lock().ok()?;
        let freq = qpc_freq() as f64;
        let now = qpc_now();
        let keep_from = now - 2 * qpc_freq();
        let win_from = now - qpc_freq();
        let mut best: Option<(u32, usize)> = None;
        // Prune + rank. Single pass; map is tiny (one entry per presenter).
        let pids: Vec<u32> = st.events.keys().copied().collect();
        for pid in pids {
            let q = match st.events.get_mut(&pid) {
                Some(q) => q,
                None => continue,
            };
            while q.front().is_some_and(|t| *t < keep_from) {
                q.pop_front();
            }
            if q.is_empty() {
                st.events.remove(&pid);
                continue;
            }
            if pid == exclude_pid {
                continue;
            }
            let n = q.iter().filter(|t| **t >= win_from).count();
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
                    // Incumbent needs >= 1 present in the 1s window to
                    // stay displayed; 0 means over a second of silence,
                    // which reads honestly as listening, not "0fps".
                    Some(p)
                        if p != exclude_pid
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
        // avg_ms from consecutive QPC intervals inside the 1s window.
        let mut sum = 0.0f64;
        let mut gaps = 0u32;
        let mut prev: Option<i64> = None;
        for t in q.iter().filter(|t| **t >= win_from) {
            if let Some(p) = prev {
                let dt = *t - p;
                if dt > 0 {
                    sum += dt as f64 * 1000.0 / freq;
                    gaps += 1;
                }
            }
            prev = Some(*t);
        }
        let avg_ms = if gaps > 0 { sum / gaps as f64 } else { 0.0 } as f32;
        let name = st
            .names
            .get(&pid)
            .cloned()
            .unwrap_or_else(|| Self::resolve_name(&mut st.names, pid));
        Some(AppFrame {
            pid,
            name,
            fps: n as f32,
            avg_ms,
        })
    }

    /// Placeholder line when no app data yet: "APP  -- (run as admin)" etc.
    pub fn status_text(&self) -> String {
        match self.state.lock().ok().and_then(|st| st.session_err.clone()) {
            Some(e) => format!("APP  -- ({e})"),
            None => "APP  -- (listening…)".to_string(),
        }
    }

    fn resolve_name(names: &mut HashMap<u32, String>, pid: u32) -> String {
        let name = process_name(pid).unwrap_or_else(|| format!("pid {pid}"));
        names.insert(pid, name.clone());
        // Drop cached names for dead processes so the map stays tiny.
        if names.len() > 64 {
            names.retain(|_, _| false);
            names.insert(pid, name.clone());
        }
        name
    }
}

fn process_name(pid: u32) -> Option<String> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
        PROCESS_QUERY_LIMITED_INFORMATION,
    };
    unsafe {
        let h = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid).ok()?;
        let mut buf = [0u16; 512];
        let mut len = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(
            h,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
        .is_ok();
        let _ = CloseHandle(h);
        if !ok {
            return None;
        }
        let full = String::from_utf16_lossy(&buf[..len as usize]);
        Some(full.rsplit(['\\', '/']).next().unwrap_or(&full).to_string())
    }
}
