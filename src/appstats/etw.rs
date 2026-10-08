//! Minimal ETW consumer for per-app present stats.
//!
//! Modelled on PresentMon's trace session, stripped to the bone:
//! - one real-time session ("minihud-present")
//! - one provider: Microsoft-Windows-DXGI, ID-filtered to
//!   Present_Start (42) + Present_Stop (43)
//! - the callback only counts Present_Start per pid; the HUD thread
//!   turns counts into FPS.
//!
//! Needs elevation (ETW session control). Without it the tracker stays
//! empty and the HUD shows "APP  -- (run as admin)".
//!
//! The callback context is a leaked `Arc` (process-lifetime thread).

use super::State;
use std::collections::VecDeque;
use std::sync::{
    atomic::{AtomicBool, AtomicU64, Ordering},
    Arc, Mutex,
};
use windows::core::{GUID, PCWSTR};
use windows::Win32::Foundation::{ERROR_ACCESS_DENIED, WIN32_ERROR};
use windows::Win32::System::Diagnostics::Etw::*;

/// Microsoft-Windows-DXGI (from `logman query providers`, matches PresentMon).
const DXGI_GUID: GUID = GUID::from_u128(0xca11c036_0102_4a2d_a6ad_f03cfed5d3c9);
/// Session GUID for our own trace (any stable GUID works).
const SESSION_GUID: GUID = GUID::from_u128(0x9b4e5c1a_3f2d_4a8b_9c0d_1e2f3a4b5c6d);
const SESSION_NAME: &str = "minihud-present";
/// DXGI Present_Start / Present_Stop (PresentMon: Present_Start=0x2A).
const PRESENT_START_ID: u16 = 42;
const PRESENT_STOP_ID: u16 = 43;
/// DXGI PresentMultiplaneOverlay_Start/Stop (0x37/0x38, task 14).
/// Fullscreen games with MPO planes present through these INSTEAD of
/// Present_Start — without them we count a fraction of frames and the
/// APP row flaps back to "-- (listening…)".
const MPO_START_ID: u16 = 0x37;
const MPO_STOP_ID: u16 = 0x38;
/// Analytic channel + Events keyword (matches the manifest descriptors).
const DXGI_KEYWORDS: u64 = 0x8000_0000_0000_0002;
const MAX_QUEUE: usize = 720;

/// Events dropped because the shared state was locked (HUD read in
/// progress). The HUD recomputes at most every 500ms, so this should
/// stay near zero; if it climbs, reads are contending with delivery.
static DROPPED: AtomicU64 = AtomicU64::new(0);

/// Total Present_Start events dropped on lock contention, for diagnostics.
pub fn dropped() -> u64 {
    DROPPED.load(Ordering::Relaxed)
}

/// (start_events, stop_events) seen across all pids. Start should ≈
/// Stop; a big gap means loss somewhere or an unhandled present path.
pub fn seen() -> (u64, u64) {
    (
        SEEN_START.load(Ordering::Relaxed),
        SEEN_STOP.load(Ordering::Relaxed),
    )
}

/// Worst delivery lag seen: now-QPC minus event QPC at arrival, in ms.
/// Real-time ETW can deliver buffers late; if this exceeds ~1000ms,
/// in-window counts collapse even while totals climb — the signature
/// of lag, not of a quiet game.
static MAX_LAG_MS: AtomicU64 = AtomicU64::new(0);

/// Worst observed ETW delivery lag in milliseconds, for diagnostics.
pub fn max_lag_ms() -> u64 {
    MAX_LAG_MS.load(Ordering::Relaxed)
}

fn note_lag(stamp_qpc: i64) {
    use windows::Win32::System::Performance::{QueryPerformanceCounter, QueryPerformanceFrequency};
    unsafe {
        let (mut now, mut freq) = (0i64, 0i64);
        if QueryPerformanceCounter(&mut now).is_err()
            || QueryPerformanceFrequency(&mut freq).is_err()
            || freq <= 0
        {
            return;
        }
        let lag_ms = (now - stamp_qpc).max(0) * 1000 / freq;
        let _ = MAX_LAG_MS.try_update(Ordering::Relaxed, Ordering::Relaxed, |m| {
            if (lag_ms as u64) > m {
                Some(lag_ms as u64)
            } else {
                None
            }
        });
    }
}

fn wide_nul(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Our own PID. The HUD never tracks itself: self presents are dropped
/// in the callback so maps, totals and snapshots only ever contain
/// other processes.
static SELF_PID: AtomicU64 = AtomicU64::new(0);

/// Events seen per DXGI present opcode, for diagnostics (self excluded).
static SEEN_START: AtomicU64 = AtomicU64::new(0);
static SEEN_STOP: AtomicU64 = AtomicU64::new(0);

fn set_err(state: &Arc<Mutex<State>>, e: impl Into<String>) {
    if let Ok(mut st) = state.lock() {
        if st.session_err.is_none() {
            st.session_err = Some(e.into());
        }
    }
}

pub fn run(state: Arc<Mutex<State>>) {
    use windows::Win32::System::Threading::GetCurrentProcessId;
    unsafe {
        SELF_PID.store(GetCurrentProcessId() as u64, Ordering::Relaxed);
    }
    // Supervisor: restart the consumer when it dies unexpectedly
    // (session killed externally, transient StartTrace failure).
    // Bounded backoff, then park — no infinite retry storm.
    // Access-denied never retries: elevation won't appear mid-run.
    // SHUTDOWN (clean quit) breaks out without retrying.
    let backoff = [2u64, 5, 15, 30, 60];
    for (i, secs) in backoff.iter().enumerate() {
        let retry = unsafe { run_inner(&state) };
        if !retry || SHUTDOWN.load(Ordering::Relaxed) {
            break;
        }
        if i + 1 == backoff.len() {
            tracing::warn!("appstats: retries exhausted, parking consumer");
            set_err(&state, "etw consumer stopped");
            break;
        }
        tracing::warn!(
            "appstats: consumer ended, retry {}/{} in {secs}s",
            i + 1,
            backoff.len()
        );
        std::thread::sleep(std::time::Duration::from_secs(*secs));
        if SHUTDOWN.load(Ordering::Relaxed) {
            break;
        }
    }
}

/// Set by stop_session: tells the supervisor the exit was deliberate.
static SHUTDOWN: AtomicBool = AtomicBool::new(false);

/// Stop the real-time session (clean quit path). Kernel sessions
/// survive process death, so without this every run leaves one behind
/// until the next start's stale-stop. Idempotent: stopping a missing
/// session is a debug-level no-op.
pub fn stop_session() {
    SHUTDOWN.store(true, Ordering::Relaxed);
    unsafe {
        let name = wide_nul(SESSION_NAME);
        let mut props = EVENT_TRACE_PROPERTIES::default();
        props.Wnode.BufferSize = std::mem::size_of::<EVENT_TRACE_PROPERTIES>() as u32;
        let err = ControlTraceW(
            CONTROLTRACE_HANDLE::default(),
            PCWSTR(name.as_ptr()),
            &mut props,
            EVENT_TRACE_CONTROL_STOP,
        );
        if err == WIN32_ERROR(0) {
            tracing::info!("appstats: ETW session stopped");
        } else {
            tracing::debug!("appstats: stop_session: {err:?}");
        }
    }
}

/// Starts the session and consumes until it ends.
/// Returns false when retrying is pointless (access denied: elevation
/// won't appear mid-run); true for anything worth another attempt.
unsafe fn run_inner(state: &Arc<Mutex<State>>) -> bool {
    // Stop any stale session from a previous (crashed) run.
    {
        let name = wide_nul(SESSION_NAME);
        let mut props = EVENT_TRACE_PROPERTIES::default();
        props.Wnode.BufferSize = std::mem::size_of::<EVENT_TRACE_PROPERTIES>() as u32;
        let _ = ControlTraceW(
            CONTROLTRACE_HANDLE::default(),
            PCWSTR(name.as_ptr()),
            &mut props,
            EVENT_TRACE_CONTROL_STOP,
        );
    }

    // StartTrace needs a properties block with room for the logger name.
    let name = wide_nul(SESSION_NAME);
    let buf_len = std::mem::size_of::<EVENT_TRACE_PROPERTIES>() + name.len() * 2;
    let mut buf = vec![0u8; buf_len];
    let props = buf.as_mut_ptr() as *mut EVENT_TRACE_PROPERTIES;
    (*props).Wnode.BufferSize = buf_len as u32;
    (*props).Wnode.Flags = WNODE_FLAG_TRACED_GUID;
    (*props).Wnode.Guid = SESSION_GUID;
    (*props).LogFileMode = EVENT_TRACE_REAL_TIME_MODE;
    // Mirror PresentMon's buffering (defaults starve under game bursts).
    (*props).BufferSize = 16; // KB per buffer: small buffers fill fast
    (*props).MinimumBuffers = 64; // at game volumes (~15-60KB/s), so delivery
    (*props).MaximumBuffers = 256; // batches stay short instead of ~1s+
    (*props).FlushTimer = 1; // seconds; hard bound on delivery lag
    (*props).LoggerNameOffset = std::mem::size_of::<EVENT_TRACE_PROPERTIES>() as u32;
    std::ptr::copy_nonoverlapping(
        name.as_ptr(),
        buf.as_mut_ptr()
            .add(std::mem::size_of::<EVENT_TRACE_PROPERTIES>()) as *mut u16,
        name.len(),
    );

    let mut session = CONTROLTRACE_HANDLE::default();
    let err = StartTraceW(&mut session, PCWSTR(name.as_ptr()), props);
    if err != WIN32_ERROR(0) {
        if err == ERROR_ACCESS_DENIED {
            tracing::warn!("appstats: StartTrace denied; run elevated for per-app stats");
            set_err(state, "run as admin");
            return false;
        } else {
            tracing::warn!("appstats: StartTrace failed: {err:?}");
            set_err(state, format!("etw error {}", err.0));
        }
        return true;
    }

    // ID filter: Present_Start/Stop + MPO_Start/Stop (EVENT_FILTER_EVENT_ID
    // is variable-length: FilterIn u8, Reserved u8, Count u16, Events…).
    let id_filter: [u16; 6] = [
        0x0001,
        4,
        PRESENT_START_ID,
        PRESENT_STOP_ID,
        MPO_START_ID,
        MPO_STOP_ID,
    ];
    let filter_desc = EVENT_FILTER_DESCRIPTOR {
        Ptr: id_filter.as_ptr() as u64,
        Size: (id_filter.len() * 2) as u32,
        Type: EVENT_FILTER_TYPE_EVENT_ID,
    };
    let mut params = ENABLE_TRACE_PARAMETERS {
        Version: ENABLE_TRACE_PARAMETERS_VERSION_2,
        EnableFilterDesc: &filter_desc as *const _ as *mut _,
        FilterDescCount: 1,
        ..Default::default()
    };
    let err = EnableTraceEx2(
        session,
        &DXGI_GUID,
        EVENT_CONTROL_CODE_ENABLE_PROVIDER.0,
        TRACE_LEVEL_VERBOSE as u8,
        DXGI_KEYWORDS,
        0,
        0,
        Some(&mut params),
    );
    if err != WIN32_ERROR(0) {
        tracing::warn!("appstats: EnableTraceEx2 failed: {err:?}");
        set_err(state, format!("etw error {}", err.0));
        return true;
    }

    // Leak one Arc ref as callback context (process-lifetime consumer
    // thread). NOTE: into_raw yields *const Mutex<State> (the inner
    // value), NOT *const Arc — the callback must deref it as such.
    let ctx = Arc::into_raw(state.clone()) as *mut std::ffi::c_void;
    let mut logfile = EVENT_TRACE_LOGFILEW::default();
    logfile.LoggerName = windows::core::PWSTR(name.as_ptr() as *mut _);
    logfile.Anonymous1.ProcessTraceMode = PROCESS_TRACE_MODE_REAL_TIME
        | PROCESS_TRACE_MODE_EVENT_RECORD
        | PROCESS_TRACE_MODE_RAW_TIMESTAMP;
    logfile.Anonymous2.EventRecordCallback = Some(event_callback);
    logfile.Context = ctx;

    let trace = OpenTraceW(&mut logfile);
    if trace.Value == u64::MAX {
        tracing::warn!("appstats: OpenTrace failed");
        set_err(state, "etw open failed");
        return true;
    }
    tracing::info!("appstats: ETW consumer running (DXGI Present_Start/Stop)");
    let err = ProcessTrace(&[trace], None, None);
    // A deliberate stop (clean quit) is routine, not a warning; an
    // unexpected end keeps WARN so it gets noticed.
    if SHUTDOWN.load(Ordering::Relaxed) {
        tracing::debug!("appstats: ProcessTrace ended on shutdown: {err:?}");
    } else {
        tracing::warn!("appstats: ProcessTrace ended: {err:?}");
    }
    true
}

unsafe extern "system" fn event_callback(record: *mut EVENT_RECORD) {
    if record.is_null() {
        return;
    }
    let r = &*record;
    if r.EventHeader.ProviderId != DXGI_GUID {
        return;
    }
    // Either flip-model or MPO presents mark one submitted frame.
    let id = r.EventHeader.EventDescriptor.Id;
    if id == PRESENT_START_ID || id == MPO_START_ID {
        SEEN_START.fetch_add(1, Ordering::Relaxed);
        note_lag(r.EventHeader.TimeStamp);
    } else if id == PRESENT_STOP_ID || id == MPO_STOP_ID {
        SEEN_STOP.fetch_add(1, Ordering::Relaxed);
        return;
    } else {
        return;
    }
    let pid = r.EventHeader.ProcessId;
    if pid == 0 {
        return;
    }
    // Never track ourselves (see SELF_PID).
    if pid as u64 == SELF_PID.load(Ordering::Relaxed) {
        return;
    }
    // Context holds *const Mutex<State> (see into_raw note above).
    let mtx = r.UserContext as *const Mutex<State>;
    if mtx.is_null() {
        return;
    }
    // try_lock: never stall the ETW delivery thread; drop on contention.
    match (*mtx).try_lock() {
        Ok(mut st) => {
            // Store the event's QPC timestamp, not arrival time: real-time
            // delivery batches events, so arrivals cluster and fake a 0ms avg.
            let stamp = r.EventHeader.TimeStamp;
            st.max_stamp = st.max_stamp.max(stamp);
            let q = st.events.entry(pid).or_insert_with(VecDeque::new);
            q.push_back(stamp);
            while q.len() > MAX_QUEUE {
                q.pop_front();
            }
        }
        Err(_) => {
            DROPPED.fetch_add(1, Ordering::Relaxed);
        }
    }
}
