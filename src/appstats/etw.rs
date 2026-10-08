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
    atomic::{AtomicU64, Ordering},
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

fn wide_nul(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn set_err(state: &Arc<Mutex<State>>, e: impl Into<String>) {
    if let Ok(mut st) = state.lock() {
        if st.session_err.is_none() {
            st.session_err = Some(e.into());
        }
    }
}

pub fn run(state: Arc<Mutex<State>>) {
    unsafe { run_inner(&state) }
}

unsafe fn run_inner(state: &Arc<Mutex<State>>) {
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
        } else {
            tracing::warn!("appstats: StartTrace failed: {err:?}");
            set_err(state, format!("etw error {}", err.0));
        }
        return;
    }

    // ID filter: Present_Start + Present_Stop only (EVENT_FILTER_EVENT_ID
    // is variable-length: FilterIn u8, Reserved u8, Count u16, Events…).
    let id_filter: [u16; 4] = [0x0001, 2, PRESENT_START_ID, PRESENT_STOP_ID];
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
        return;
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
        return;
    }
    tracing::info!("appstats: ETW consumer running (DXGI Present_Start/Stop)");
    let err = ProcessTrace(&[trace], None, None);
    tracing::warn!("appstats: ProcessTrace ended: {err:?}");
}

unsafe extern "system" fn event_callback(record: *mut EVENT_RECORD) {
    if record.is_null() {
        return;
    }
    let r = &*record;
    if r.EventHeader.ProviderId != DXGI_GUID || r.EventHeader.EventDescriptor.Id != PRESENT_START_ID
    {
        return;
    }
    let pid = r.EventHeader.ProcessId;
    if pid == 0 {
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
            let q = st.events.entry(pid).or_insert_with(VecDeque::new);
            q.push_back(r.EventHeader.TimeStamp);
            while q.len() > MAX_QUEUE {
                q.pop_front();
            }
        }
        Err(_) => {
            DROPPED.fetch_add(1, Ordering::Relaxed);
        }
    }
}
