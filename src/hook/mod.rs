//! Host-side injection and ring reader for the in-process present hook.
//!
//! [`inject`] runs the classic remote-load pipeline (`OpenProcess` →
//! `VirtualAllocEx`/`WriteProcessMemory` → `CreateRemoteThread(LoadLibraryW)` →
//! remote `mh_install`), refusing WOW64 targets and known anti-cheat modules.
//! [`FrameReader`] opens the shared ring and computes trailing fps/frametime.
//!
//! This is a tier-3 (in-process, unsigned inject) technique — see `HOOKING.md`
//! and `research/docs/03-anti-cheat-and-safety.md`. It is opt-in only and must
//! never be pointed at a protected/anti-cheat title.

pub mod follow;
pub mod proc;

use std::os::windows::ffi::OsStrExt;
use std::path::{Path, PathBuf};

use hook_ipc::{FrameMapping, Header, RingReader, RingWriter, Trailing};
use windows::core::{s, PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_EVENT, WAIT_OBJECT_0};
use windows::Win32::System::Diagnostics::Debug::WriteProcessMemory;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Module32FirstW, Module32NextW, MODULEENTRY32W, TH32CS_SNAPMODULE,
};
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW};
use windows::Win32::System::Memory::{
    VirtualAllocEx, VirtualFreeEx, MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE,
};
use windows::Win32::System::Performance::QueryPerformanceFrequency;
use windows::Win32::System::Threading::{
    CreateProcessW, CreateRemoteThread, GetExitCodeThread, IsWow64Process, OpenProcess,
    ResumeThread, TerminateProcess, WaitForSingleObject, CREATE_SUSPENDED, LPTHREAD_START_ROUTINE,
    PROCESS_ALL_ACCESS, PROCESS_INFORMATION, STARTUPINFOW,
};

/// Remote thread wait budget.
const REMOTE_TIMEOUT_MS: u32 = 10_000;
/// Default capture duration for `--capture-hook <pid>` with no seconds.
pub const DEFAULT_CAPTURE_SECS: u64 = 10;
/// Frames read back per sample.
const READBACK_FRAMES: usize = 512;

/// Modules whose presence means "do not inject" (substring, case-insensitive).
const DENY_MODULES: &[&str] = &[
    "easyanticheat",
    "eac_",
    "battleye",
    "bedaisy",
    "beservice",
    "vanguard",
    "vgtray",
    "vgk",
    "denuvo",
    "irdeto",
    "faceit",
    "esea",
    "equ8",
    "xigncode",
    "nprotect",
    "gameguard",
];

/// True when a loaded module name marks a protected/anti-cheat target.
pub fn is_denied(module: &str) -> bool {
    let m = module.to_ascii_lowercase();
    DENY_MODULES.iter().any(|d| m.contains(d))
}

/// True when the target must be refused: a WOW64 (32-bit) process cannot host
/// our 64-bit hook DLL.
pub fn should_refuse_wow64(is_wow64: bool) -> bool {
    is_wow64
}

/// Parsed `--capture-hook <pid> [secs]`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CaptureArgs {
    pub pid: u32,
    pub secs: u64,
}

/// Parse `--capture-hook <pid> [secs]` from the process arguments.
pub fn parse_capture_hook(args: &[String]) -> Option<CaptureArgs> {
    let i = args.iter().position(|a| a == "--capture-hook")?;
    let pid = args.get(i + 1)?.parse().ok()?;
    let secs = args
        .get(i + 2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_CAPTURE_SECS);
    Some(CaptureArgs { pid, secs })
}

/// Parsed `--launch <exe> [args...]`: the target to start suspended and inject.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchArgs {
    pub exe: String,
    pub args: Vec<String>,
}

/// Parsed `--read-frames <pid> [secs]`: observe an existing ring read-only.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ReadFramesArgs {
    pub pid: u32,
    pub secs: u64,
}

/// Parse `--read-frames <pid> [secs]` from the process arguments. The observer
/// never injects; it opens a ring another process (the injected recorder or the
/// Vulkan implicit layer) created.
pub fn parse_read_frames(args: &[String]) -> Option<ReadFramesArgs> {
    let i = args.iter().position(|a| a == "--read-frames")?;
    let pid = args.get(i + 1)?.parse().ok()?;
    let secs = args
        .get(i + 2)
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_CAPTURE_SECS);
    Some(ReadFramesArgs { pid, secs })
}

/// Parse `--launch <exe> [args...]` from the process arguments.
///
/// Everything after the exe belongs to the child, so `--launch` must be the
/// last minihud option on the command line. Returns `None` when `--launch` is
/// absent or has no (non-empty) exe.
pub fn parse_launch(args: &[String]) -> Option<LaunchArgs> {
    let i = args.iter().position(|a| a == "--launch")?;
    let exe = args.get(i + 1)?;
    if exe.is_empty() {
        return None;
    }
    Some(LaunchArgs {
        exe: exe.clone(),
        args: args[i + 2..].to_vec(),
    })
}

/// Quote one token for a Windows `CreateProcessW` command line.
///
/// A plain token is passed through unchanged; otherwise the Microsoft argv
/// quoting rules apply: wrap in `"`, double every backslash that precedes a
/// (literal or closing) quote, and escape embedded quotes.
fn quote_arg(arg: &str) -> String {
    if !arg.is_empty() && !arg.contains(' ') && !arg.contains('\t') && !arg.contains('"') {
        return arg.to_string();
    }
    let mut out = String::with_capacity(arg.len() + 2);
    out.push('"');
    let mut chars = arg.chars().peekable();
    loop {
        let mut backslashes = 0;
        while chars.peek() == Some(&'\\') {
            chars.next();
            backslashes += 1;
        }
        match chars.peek() {
            None => {
                // Trailing backslashes must be doubled so the closing quote
                // stays a quote rather than escaping a preceding backslash.
                for _ in 0..backslashes * 2 {
                    out.push('\\');
                }
                break;
            }
            Some(&'"') => {
                for _ in 0..backslashes * 2 + 1 {
                    out.push('\\');
                }
                out.push('"');
                chars.next();
            }
            Some(&c) => {
                for _ in 0..backslashes {
                    out.push('\\');
                }
                out.push(c);
                chars.next();
            }
        }
    }
    out.push('"');
    out
}

/// Build the command line for `CreateProcessW` from an exe path and its
/// arguments, quoting each token with [`quote_arg`].
pub fn build_command_line(exe: &str, args: &[String]) -> String {
    let mut out = quote_arg(exe);
    for a in args {
        out.push(' ');
        out.push_str(&quote_arg(a));
    }
    out
}

/// Re-quote a full argument vector into one parameter string, quoting each token
/// with [`quote_arg`].
///
/// Used when relaunching elevated: the child re-parses the string, so a token
/// with spaces (e.g. a `--launch` target under `Program Files`) must keep its
/// quotes or it splits into two arguments.
pub fn requote_args(args: &[String]) -> String {
    args.iter()
        .map(|a| quote_arg(a))
        .collect::<Vec<_>>()
        .join(" ")
}

/// An owned target-process handle closed on drop.
struct ProcHandle(HANDLE);

impl Drop for ProcHandle {
    fn drop(&mut self) {
        // SAFETY: the handle was returned by `OpenProcess` and is closed once.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// A buffer allocated in the target with `VirtualAllocEx`, freed on drop.
///
/// A remote allocation that outlives a failed injection leaks in the target for
/// its whole life (the target keeps a page it never asked for). Owning it in a
/// guard frees it on every error path — `WriteProcessMemory`, `CreateRemoteThread`
/// or the remote `mh_install` — not just on success.
struct RemoteAlloc {
    process: HANDLE,
    addr: *mut core::ffi::c_void,
}

impl Drop for RemoteAlloc {
    fn drop(&mut self) {
        // SAFETY: `addr` was returned by `VirtualAllocEx` in `process` and is
        // released exactly once here.
        unsafe {
            let _ = VirtualFreeEx(self.process, self.addr, 0, MEM_RELEASE);
        }
    }
}

/// Path of the recorder DLL: next to the running `minihud.exe`.
///
/// The cargo artifact is `hook_rt.dll` (the package name with `-` → `_`); we
/// also accept a hyphenated spelling.
pub fn hook_dll_path() -> Result<PathBuf, String> {
    let exe = std::env::current_exe().map_err(|e| format!("current_exe: {e}"))?;
    let dir = exe.parent().ok_or("exe has no parent directory")?;
    for name in ["hook-rt.dll", "hook_rt.dll"] {
        let dll = dir.join(name);
        if dll.exists() {
            return Ok(dll);
        }
    }
    Err(format!(
        "hook_rt.dll not found next to the exe ({})",
        dir.display()
    ))
}

/// True for the recorder DLL's module name (either spelling).
fn is_recorder_dll(name: &str) -> bool {
    name.eq_ignore_ascii_case("hook-rt.dll") || name.eq_ignore_ascii_case("hook_rt.dll")
}

/// Null-terminated UTF-16.
fn wide(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// Inject the recorder into `pid` and call `mh_install`. Returns the
/// installed-API bitmask reported by the DLL.
pub fn inject(pid: u32) -> Result<u32, String> {
    let dll = hook_dll_path()?;
    if pid == 0 {
        return Err("refusing to inject into pid 0".into());
    }
    // SAFETY: `OpenProcess` with a pid is a normal Win32 call.
    let process = unsafe { OpenProcess(PROCESS_ALL_ACCESS, false, pid) }
        .map_err(|e| format!("OpenProcess({pid}): {e}"))?;
    let process = ProcHandle(process);

    // SAFETY: `process.0` is a live process handle.
    let mut wow64 = windows::core::BOOL(0);
    unsafe { IsWow64Process(process.0, &mut wow64) }.map_err(|e| format!("IsWow64Process: {e}"))?;
    if should_refuse_wow64(wow64.as_bool()) {
        return Err("target is WOW64 (32-bit); the recorder is 64-bit only".into());
    }

    for name in module_names(pid) {
        if is_denied(&name) {
            return Err(format!(
                "refusing to inject: protected/anti-cheat module {name:?} is loaded"
            ));
        }
    }

    // Remote LoadLibraryW of the recorder DLL.
    let path_w = wide(&dll.to_string_lossy());
    let bytes = path_w.len() * 2;
    // SAFETY: allocating memory in the target for the wide path.
    let remote_addr = unsafe {
        VirtualAllocEx(
            process.0,
            None,
            bytes,
            MEM_COMMIT | MEM_RESERVE,
            PAGE_READWRITE,
        )
    };
    if remote_addr.is_null() {
        return Err("VirtualAllocEx failed".into());
    }
    // Owned from here on: every return below (including error paths) frees the
    // target's buffer instead of leaking it.
    let remote = RemoteAlloc {
        process: process.0,
        addr: remote_addr,
    };
    // SAFETY: `remote.addr` is `bytes`-sized writable memory in the target.
    unsafe {
        WriteProcessMemory(
            process.0,
            remote.addr,
            path_w.as_ptr() as *const core::ffi::c_void,
            bytes,
            None,
        )
    }
    .map_err(|e| format!("WriteProcessMemory: {e}"))?;

    // SAFETY: kernel32 is loaded in every process.
    let kernel32 = unsafe { GetModuleHandleW(windows::core::w!("kernel32.dll")) }
        .map_err(|e| format!("GetModuleHandleW(kernel32): {e}"))?;
    // SAFETY: `kernel32` is the loaded kernel32 module.
    let load_library = unsafe { GetProcAddress(kernel32, s!("LoadLibraryW")) }
        .ok_or("GetProcAddress(LoadLibraryW) failed")?;

    // SAFETY: `load_library` is LoadLibraryW and `remote.addr` its argument.
    unsafe {
        remote_call(
            process.0,
            load_library as usize as *const core::ffi::c_void,
            remote.addr,
        )
    }?;

    // Find the freshly-loaded module's base by name (robust on 64-bit, where
    // the thread exit code cannot carry a full HMODULE).
    let remote_base = module_bases(pid)
        .into_iter()
        .find(|(_, name)| is_recorder_dll(name))
        .map(|(base, _)| base)
        .ok_or("the recorder DLL did not load in the target")?;

    let rva = export_rva(&dll, "mh_install")?;
    let install = remote_base + rva;

    // SAFETY: `install` is the remote `mh_install`; null argument.
    let mask = unsafe {
        remote_call(
            process.0,
            install as *const core::ffi::c_void,
            core::ptr::null(),
        )
    }?;

    if mask == u32::MAX {
        return Err("mh_install panicked in the target".into());
    }
    Ok(mask)
}

/// Remote-call the injected runtime's `mh_uninstall` in `pid`, restoring every
/// patch. Bounded (the remote call has a fixed wait budget) and fail-open: the
/// caller logs and keeps going on any error.
pub fn unhook(pid: u32) -> Result<(), String> {
    if pid == 0 {
        return Err("refusing to unhook pid 0".into());
    }
    let dll = hook_dll_path()?;
    // SAFETY: `OpenProcess` with a pid is a normal Win32 call.
    let process = unsafe { OpenProcess(PROCESS_ALL_ACCESS, false, pid) }
        .map_err(|e| format!("OpenProcess({pid}): {e}"))?;
    let process = ProcHandle(process);

    let remote_base = module_bases(pid)
        .into_iter()
        .find(|(_, name)| is_recorder_dll(name))
        .map(|(base, _)| base)
        .ok_or("the recorder DLL is not loaded in the target")?;

    let rva = export_rva(&dll, "mh_uninstall")?;
    let uninstall = remote_base + rva;

    // SAFETY: `uninstall` is the remote `mh_uninstall`; null argument.
    let code = unsafe {
        remote_call(
            process.0,
            uninstall as *const core::ffi::c_void,
            core::ptr::null(),
        )
    }?;
    if code == u32::MAX {
        return Err("mh_uninstall panicked in the target".into());
    }
    Ok(())
}

/// True only when a bounded wait reached a terminal state.
///
/// `GetExitCodeThread` on a still-running thread returns `STILL_ACTIVE` (259),
/// which is indistinguishable from a genuine exit code. A timeout (or failed
/// wait) must therefore be rejected before reading the exit code, or `inject`
/// would publish `259` as the installed mask and the host would believe a
/// half-patched target was hooked.
fn remote_call_completed(wait: WAIT_EVENT) -> bool {
    wait == WAIT_OBJECT_0
}

/// Call a remote function with one pointer argument; return its exit code.
///
/// # Safety
/// `addr` must be the address of an `LPTHREAD_START_ROUTINE`-shaped function
/// inside the target, and `arg` a valid argument for it.
unsafe fn remote_call(
    process: HANDLE,
    addr: *const core::ffi::c_void,
    arg: *const core::ffi::c_void,
) -> Result<u32, String> {
    let start: LPTHREAD_START_ROUTINE = Some(core::mem::transmute::<
        *const core::ffi::c_void,
        unsafe extern "system" fn(*mut core::ffi::c_void) -> u32,
    >(addr));
    // SAFETY: `start` is a live remote entry point; `arg` is valid for it.
    let thread = unsafe { CreateRemoteThread(process, None, 0, start, Some(arg), 0, None) }
        .map_err(|e| format!("CreateRemoteThread: {e}"))?;
    // SAFETY: `thread` is a live handle.
    let wait = unsafe { WaitForSingleObject(thread, REMOTE_TIMEOUT_MS) };
    if !remote_call_completed(wait) {
        // The remote thread is still running; closing the handle does not stop
        // it, but the call is reported as failed rather than as its exit code.
        // SAFETY: `thread` is closed once.
        unsafe {
            let _ = CloseHandle(thread);
        }
        return Err(format!(
            "remote call did not complete within {REMOTE_TIMEOUT_MS} ms"
        ));
    }
    let mut code: u32 = 0;
    // SAFETY: `thread` is live and `code` a valid out-pointer.
    let res = unsafe { GetExitCodeThread(thread, &mut code) };
    // SAFETY: `thread` is closed once.
    unsafe {
        let _ = CloseHandle(thread);
    }
    res.map_err(|e| format!("GetExitCodeThread: {e}"))?;
    Ok(code)
}

/// Byte offset of an exported symbol within a DLL.
///
/// Fails when the symbol is not exported, or when it is a **forwarder**:
/// `GetProcAddress` follows a forward into the implementing module, so the
/// resolved address would not belong to `dll` and `addr - base` would be a bogus
/// RVA. The recorder's own `mh_install`/`mh_uninstall` are local exports, so this
/// only guards against a future forwarded lookup silently pointing at garbage.
fn export_rva(dll: &Path, name: &str) -> Result<usize, String> {
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::System::LibraryLoader::{
        GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
        GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
    };
    let w = wide(&dll.to_string_lossy());
    // SAFETY: `w` is a NUL-terminated path to a valid DLL.
    let module = unsafe { LoadLibraryW(PCWSTR(w.as_ptr())) }
        .map_err(|e| format!("LoadLibraryW(local): {e}"))?;
    let base = module.0 as usize;
    let cname = std::ffi::CString::new(name).map_err(|_| "bad export name".to_string())?;
    // SAFETY: `module` is the loaded DLL; `cname` is a NUL-terminated name.
    let proc = unsafe { GetProcAddress(module, windows::core::PCSTR(cname.as_ptr() as *const u8)) }
        .ok_or_else(|| format!("export {name} not found in {}", dll.display()))?;
    let addr = proc as usize;
    // A forwarded export resolves into another module; confirm the address still
    // belongs to `module` before treating `addr - base` as its RVA.
    let mut owner = HMODULE::default();
    let flags =
        GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT;
    // SAFETY: `addr` is a live code address inside a loaded module.
    let resolved = unsafe { GetModuleHandleExW(flags, PCWSTR(addr as *const u16), &mut owner) };
    if resolved.is_err() || owner.0 != module.0 {
        return Err(format!(
            "export {name} in {} is forwarded to another module",
            dll.display()
        ));
    }
    // SAFETY: the module stays loaded for the process (we do not FreeLibrary).
    Ok(addr.saturating_sub(base))
}

/// QueryPerformanceFrequency as `u64` (0 on failure). QPC frequency is
/// system-wide, so a host-written header matches the recorder's.
fn qpc_frequency() -> u64 {
    let mut v: i64 = 0;
    // SAFETY: `v` is a valid out-pointer for a QPF read.
    match unsafe { QueryPerformanceFrequency(&mut v) } {
        Ok(()) if v > 0 => v as u64,
        _ => 0,
    }
}

/// Ensure the frame ring for `pid` exists and is initialized, and return the
/// host-owned mapping (kept alive by the caller so the recorder attaches to a
/// mapping the host created rather than racing to create its own).
///
/// An already-initialized block is left untouched; a fresh or foreign block is
/// initialized with a valid header.
pub fn ensure_ring(pid: u32) -> Result<FrameMapping, String> {
    if pid == 0 {
        return Err("refusing to ensure a ring for pid 0".into());
    }
    let mut mapping = FrameMapping::open_or_create(pid).map_err(|e| format!("open ring: {e}"))?;
    if RingReader::new(mapping.as_slice()).is_err() {
        RingWriter::init(mapping.as_mut_slice(), qpc_frequency(), pid)
            .map_err(|e| format!("init ring: {e}"))?;
    }
    Ok(mapping)
}

/// Loaded module names of a process.
fn module_names(pid: u32) -> Vec<String> {
    module_bases(pid).into_iter().map(|(_, n)| n).collect()
}

/// Attempts for a Toolhelp module snapshot. `CreateToolhelp32Snapshot` can fail
/// with `ERROR_BAD_LENGTH` while the target is still loading its modules, so a
/// single failure is retried rather than reported as "the DLL did not load".
const SNAPSHOT_ATTEMPTS: u32 = 10;
/// Delay between snapshot attempts, milliseconds.
const SNAPSHOT_RETRY_MS: u64 = 20;

/// Run `attempt` up to `tries` times, sleeping `retry_ms` between failures, and
/// return its first `Some`. `None` when every attempt fails.
fn retry<T>(tries: u32, retry_ms: u64, mut attempt: impl FnMut() -> Option<T>) -> Option<T> {
    for i in 0..tries {
        if let Some(v) = attempt() {
            return Some(v);
        }
        if i + 1 < tries {
            std::thread::sleep(std::time::Duration::from_millis(retry_ms));
        }
    }
    None
}

/// Loaded module `(base, name)` of a process, retrying the transient snapshot
/// failures described on [`SNAPSHOT_ATTEMPTS`].
fn module_bases(pid: u32) -> Vec<(usize, String)> {
    retry(SNAPSHOT_ATTEMPTS, SNAPSHOT_RETRY_MS, || {
        try_module_list(pid)
    })
    .unwrap_or_default()
}

/// One Toolhelp module snapshot; `None` when it could not be taken, so the
/// caller can retry a transient failure.
fn try_module_list(pid: u32) -> Option<Vec<(usize, String)>> {
    // SAFETY: snapshotting the module list of `pid`.
    let snap = unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPMODULE, pid) }.ok()?;
    let mut out = Vec::new();
    let mut entry = MODULEENTRY32W {
        dwSize: std::mem::size_of::<MODULEENTRY32W>() as u32,
        ..Default::default()
    };
    if unsafe { Module32FirstW(snap, &mut entry) }.is_ok() {
        loop {
            out.push((entry.modBaseAddr as usize, module_name(&entry)));
            entry.dwSize = std::mem::size_of::<MODULEENTRY32W>() as u32;
            if unsafe { Module32NextW(snap, &mut entry) }.is_err() {
                break;
            }
        }
    } else {
        // SAFETY: `snap` is closed once.
        unsafe {
            let _ = CloseHandle(snap);
        }
        return None;
    }
    // SAFETY: `snap` is closed once.
    unsafe {
        let _ = CloseHandle(snap);
    }
    Some(out)
}

/// UTF-16 module name from a snapshot entry.
fn module_name(e: &MODULEENTRY32W) -> String {
    let len = e
        .szModule
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(e.szModule.len());
    String::from_utf16_lossy(&e.szModule[..len])
}

/// A reader over a target's frame ring.
pub struct FrameReader {
    mapping: FrameMapping,
}

impl FrameReader {
    /// Open (or create) the ring for `pid`.
    pub fn open(pid: u32) -> Result<Self, String> {
        let mapping = FrameMapping::open_or_create(pid).map_err(|e| format!("open ring: {e}"))?;
        Ok(Self { mapping })
    }

    /// Open an **existing** ring for `pid`; never creates one.
    ///
    /// The read-only observer uses this so observing a pid with no ring errors
    /// instead of fabricating an empty mapping.
    pub fn open_existing(pid: u32) -> Result<Self, String> {
        let mapping = FrameMapping::open(pid).map_err(|e| format!("open ring: {e}"))?;
        Ok(Self { mapping })
    }

    /// The block header.
    pub fn header(&self) -> Option<Header> {
        RingReader::new(self.mapping.as_slice())
            .ok()
            .map(|r| r.header())
    }

    /// Trailing fps/frametime over `window_ms`.
    pub fn trailing(&self, window_ms: f64) -> Option<Trailing> {
        let r = RingReader::new(self.mapping.as_slice()).ok()?;
        let freq = r.header().qpc_freq;
        let mut recent = Vec::new();
        r.read_recent(READBACK_FRAMES, &mut recent);
        hook_ipc::trailing(&recent, freq, window_ms)
    }

    /// The installed-API bitmask published by the recorder.
    pub fn installed_mask(&self) -> Option<u32> {
        RingReader::new(self.mapping.as_slice())
            .ok()
            .map(|r| r.installed_mask())
    }

    /// Counts of the records in the trailing `window_ms`, keyed by the host
    /// label (e.g. `vk.queuepresent`), most frequent first.
    ///
    /// A single present can produce more than one record: a Vulkan present is
    /// recorded by the recorder *and* by the ICD's nested DXGI present, so the
    /// newest record alone would misreport the capture as DXGI. The breakdown
    /// shows every API the ring captured. Empty when the ring is unreadable or
    /// has no records.
    pub fn api_counts(&self, window_ms: f64) -> Vec<(&'static str, usize)> {
        let Ok(r) = RingReader::new(self.mapping.as_slice()) else {
            return Vec::new();
        };
        let freq = r.header().qpc_freq;
        let mut recent = Vec::new();
        r.read_recent(READBACK_FRAMES, &mut recent);
        let Some(newest) = recent.last().map(|rec| rec.qpc_start) else {
            return Vec::new();
        };
        let window_ticks = (window_ms * freq as f64 / 1000.0).round() as u64;
        let cutoff = newest.saturating_sub(window_ticks);
        let mut counts: Vec<(&'static str, usize)> = Vec::new();
        for rec in recent.iter().filter(|rec| rec.qpc_start >= cutoff) {
            let label = rec.api.as_str();
            match counts.iter_mut().find(|(l, _)| *l == label) {
                Some((_, n)) => *n += 1,
                None => counts.push((label, 1)),
            }
        }
        counts.sort_by_key(|a| std::cmp::Reverse(a.1));
        counts
    }
}

/// Inject into `pid`, then print fps/frametime for `secs` seconds.
pub fn capture_hook(pid: u32, secs: u64) -> Result<(), String> {
    let mask = inject(pid)?;
    let reader = FrameReader::open(pid)?;
    tracing::info!("injected into pid {pid}: installed mask {mask:#x}");
    println!("minihud: injected into pid {pid} (installed mask {mask:#x})");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs.max(1));
    while std::time::Instant::now() < deadline {
        print_sample(pid, &reader);
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    Ok(())
}

/// `--read-frames`: open an **existing** ring for `pid` (no injection — the
/// Vulkan implicit layer or an injected recorder created it) and print
/// fps/frametime for `secs` seconds.
pub fn read_frames(pid: u32, secs: u64) -> Result<(), String> {
    let reader = FrameReader::open_existing(pid)?;
    println!("minihud: observing pid {pid} (read-only, no injection)");
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(secs.max(1));
    while std::time::Instant::now() < deadline {
        print_sample(pid, &reader);
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    Ok(())
}

/// `label x count, ...` for a per-API breakdown, or `none` when empty.
fn api_summary(counts: &[(&'static str, usize)]) -> String {
    if counts.is_empty() {
        return "none".to_string();
    }
    counts
        .iter()
        .map(|(label, n)| format!("{label} x{n}"))
        .collect::<Vec<_>>()
        .join(", ")
}

/// Print one fps/frametime (or waiting) line for a reader.
fn print_sample(pid: u32, reader: &FrameReader) {
    match (reader.header(), reader.trailing(1000.0)) {
        (Some(h), Some(t)) => println!(
            "pid {pid}: {:.1} fps  {:.2} ms  ({} frames in {:.0} ms, {} records, {} errors, api {})",
            t.fps,
            t.frametime_ms,
            t.frames,
            t.span_ms,
            h.next_seq,
            h.errors,
            api_summary(&reader.api_counts(1000.0))
        ),
        (Some(h), None) => println!(
            "pid {pid}: waiting for presents ({} records, {} calls, {} errors, installed {:#x})",
            h.next_seq,
            h.calls,
            h.errors,
            reader.installed_mask().unwrap_or(0)
        ),
        _ => println!("pid {pid}: ring not initialized yet"),
    }
}

/// Default capture window for `--launch` (the child may exit earlier).
pub const LAUNCH_CAPTURE_SECS: u64 = 8;

/// An owned child process created suspended; closed on drop and terminated by
/// [`ChildProcess::terminate`] on a failed launch.
struct ChildProcess {
    process: HANDLE,
    thread: HANDLE,
    pid: u32,
}

impl ChildProcess {
    /// Kill the (possibly still suspended) child.
    fn terminate(&self) {
        // SAFETY: `process` is a live handle to the child we created.
        unsafe {
            let _ = TerminateProcess(self.process, 1);
        }
    }
}

impl Drop for ChildProcess {
    fn drop(&mut self) {
        // SAFETY: both handles were returned by `CreateProcessW` and are closed
        // exactly once here.
        unsafe {
            let _ = CloseHandle(self.thread);
            let _ = CloseHandle(self.process);
        }
    }
}

/// Launch `exe` with `args` **suspended**, inject the recorder, then resume it.
///
/// Injecting before the target runs its own code lets the recorder hook device
/// creation (D3D9 in particular), which a late `--capture-hook` misses. The
/// ring is created and initialized by the host first, so the recorder attaches
/// to it. On any failure the suspended child is terminated — it is never left
/// suspended or running half-injected. Returns the child pid.
pub fn launch_and_inject(exe: &str, args: &[String]) -> Result<u32, String> {
    if exe.is_empty() {
        return Err("empty exe path".into());
    }
    let exe_w = wide(exe);
    let cmdline = build_command_line(exe, args);
    let mut cmdline_w = wide(&cmdline);
    let si = STARTUPINFOW {
        cb: std::mem::size_of::<STARTUPINFOW>() as u32,
        ..Default::default()
    };
    let mut pi = PROCESS_INFORMATION::default();
    // SAFETY: `exe_w`/`cmdline_w` are NUL-terminated buffers live for the
    // call; `si` is a sized STARTUPINFOW; `pi` a valid out-struct. The child is
    // created suspended and does not run until we resume it.
    unsafe {
        CreateProcessW(
            PCWSTR(exe_w.as_ptr()),
            Some(PWSTR(cmdline_w.as_mut_ptr())),
            None,
            None,
            false,
            CREATE_SUSPENDED,
            None,
            PCWSTR::null(),
            &si,
            &mut pi,
        )
    }
    .map_err(|e| format!("CreateProcessW({exe}): {e}"))?;

    if pi.hProcess.is_invalid() || pi.hThread.is_invalid() {
        // SAFETY: closing whatever `CreateProcessW` returned; closing an
        // invalid handle is a harmless failure.
        unsafe {
            let _ = CloseHandle(pi.hProcess);
            let _ = CloseHandle(pi.hThread);
        }
        return Err("CreateProcessW returned no handles".into());
    }
    let child = ChildProcess {
        process: pi.hProcess,
        thread: pi.hThread,
        pid: pi.dwProcessId,
    };

    // Hold the host-owned mapping across injection so the recorder attaches to
    // the block the host created rather than racing to create its own.
    let result = ensure_ring(child.pid)
        .and_then(|_ring| inject(child.pid))
        .and_then(|_mask| {
            // SAFETY: `child.thread` is the suspended primary thread handle.
            let count = unsafe { ResumeThread(child.thread) };
            if count == u32::MAX {
                Err("ResumeThread failed".into())
            } else {
                Ok(child.pid)
            }
        });

    match result {
        Ok(pid) => Ok(pid),
        Err(e) => {
            child.terminate();
            Err(e)
        }
    }
}

/// `--launch`: start `exe` suspended, inject the recorder, print fps for a
/// bounded window (or until the child exits), then unhook and return.
pub fn launch_capture(exe: &str, args: &[String]) -> Result<(), String> {
    let pid = launch_and_inject(exe, args)?;
    println!("minihud: launched and injected {exe} (pid {pid})");
    let reader = FrameReader::open(pid)?;
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(LAUNCH_CAPTURE_SECS);
    while std::time::Instant::now() < deadline && proc::process_alive(pid) {
        print_sample(pid, &reader);
        std::thread::sleep(std::time::Duration::from_millis(500));
    }
    match unhook(pid) {
        Ok(()) => println!("minihud: unhooked pid {pid}"),
        Err(e) => eprintln!("minihud: unhook pid {pid}: {e}"),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn denies_known_anti_cheat_modules() {
        assert!(is_denied("EasyAntiCheat_EOS.exe"));
        assert!(is_denied("BEDaisy.sys"));
        assert!(is_denied("BEService.exe"));
        assert!(is_denied("vgk.sys"));
        assert!(is_denied("vgtray.exe"));
        assert!(is_denied("DenuvoAC.dll"));
        assert!(is_denied("FACEIT_AC.sys"));
        assert!(is_denied("ESEACheat.exe"));
    }

    #[test]
    fn allows_ordinary_modules() {
        assert!(!is_denied("d3d11.dll"));
        assert!(!is_denied("dxgi.dll"));
        assert!(!is_denied("game.exe"));
        assert!(!is_denied("opengl32.dll"));
    }

    #[test]
    fn refuses_wow64_targets() {
        assert!(should_refuse_wow64(true));
        assert!(!should_refuse_wow64(false));
    }

    #[test]
    fn parses_capture_hook_arguments() {
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            parse_capture_hook(&args(&["--capture-hook", "1234"])),
            Some(CaptureArgs {
                pid: 1234,
                secs: DEFAULT_CAPTURE_SECS
            })
        );
        assert_eq!(
            parse_capture_hook(&args(&["--capture-hook", "42", "5"])),
            Some(CaptureArgs { pid: 42, secs: 5 })
        );
        assert_eq!(parse_capture_hook(&args(&["--capture-hook"])), None);
        assert_eq!(parse_capture_hook(&args(&["--capture-hook", "abc"])), None);
        assert_eq!(parse_capture_hook(&args(&["--other"])), None);
    }

    #[test]
    fn parses_read_frames_arguments() {
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            parse_read_frames(&args(&["--read-frames", "1234"])),
            Some(ReadFramesArgs {
                pid: 1234,
                secs: DEFAULT_CAPTURE_SECS
            })
        );
        assert_eq!(
            parse_read_frames(&args(&["--read-frames", "42", "4"])),
            Some(ReadFramesArgs { pid: 42, secs: 4 })
        );
        assert_eq!(parse_read_frames(&args(&["--read-frames"])), None);
        assert_eq!(parse_read_frames(&args(&["--read-frames", "abc"])), None);
        assert_eq!(parse_read_frames(&args(&["--capture-hook", "1"])), None);
    }

    #[test]
    fn frame_reader_open_existing_does_not_create_the_ring() {
        // Catches: the read-only observer silently creating a zeroed mapping for
        // a pid that has no ring, then reporting a bogus "not initialized".
        let missing = 0x5653_FFFF ^ (std::process::id() & 0xFFFF);
        assert!(
            FrameReader::open_existing(missing).is_err(),
            "an absent ring must not be created by the observer"
        );
    }

    #[test]
    fn retry_returns_the_first_success_and_stops_trying() {
        let mut calls = 0;
        let got = retry(5, 0, || {
            calls += 1;
            (calls >= 3).then_some(calls)
        });
        assert_eq!(got, Some(3));
        assert_eq!(calls, 3, "must stop after the first success");
    }

    #[test]
    fn retry_returns_none_after_exhausting_every_attempt() {
        let mut calls = 0;
        let got: Option<u32> = retry(4, 0, || {
            calls += 1;
            None
        });
        assert_eq!(got, None);
        assert_eq!(calls, 4, "must try exactly `tries` times");
    }

    #[test]
    fn export_rva_finds_a_known_kernel32_export() {
        // Catches: a wrong base/RVA computation, which would make the computed
        // remote `mh_install` address point at the wrong code — injection would
        // silently call garbage instead of the recorder.
        let dll = std::path::Path::new(r"C:\Windows\System32\kernel32.dll");
        let rva = export_rva(dll, "LoadLibraryW").expect("kernel32!LoadLibraryW");
        assert!(rva > 0, "a real export lives past the module base");
        assert!(
            export_rva(dll, "NoSuchExportNameZzz").is_err(),
            "an absent export must be an error, not a bogus RVA"
        );
    }

    #[test]
    fn export_rva_rejects_a_forwarded_export() {
        // Catches: computing `addr - base` for a *forwarded* export.
        // `GetProcAddress` follows the forward into the target module (kernel32
        // forwards `AcquireSRWLockExclusive` to ntdll), so the difference is a
        // bogus RVA; using it as a remote address would call garbage. It must be
        // rejected instead of silently returned.
        let dll = std::path::Path::new(r"C:\Windows\System32\kernel32.dll");
        let err = export_rva(dll, "AcquireSRWLockExclusive")
            .expect_err("a forwarded export must not yield an RVA");
        assert!(err.contains("forward"), "error names the cause: {err}");
    }

    #[test]
    fn build_command_line_quotes_only_when_needed() {
        assert_eq!(build_command_line("app.exe", &[]), "app.exe");
        assert_eq!(
            build_command_line(r"C:\Program Files\app.exe", &[]),
            r#""C:\Program Files\app.exe""#
        );
        let args = ["--api".to_string(), "d3d9".to_string()];
        assert_eq!(build_command_line("app.exe", &args), "app.exe --api d3d9");
    }

    #[test]
    fn build_command_line_quotes_an_empty_argument() {
        let args = [String::new()];
        assert_eq!(build_command_line("app.exe", &args), "app.exe \"\"");
    }

    #[test]
    fn build_command_line_quotes_tabs_and_leading_spaces() {
        // Catches: a token containing a tab or a leading space being passed
        // through unquoted, so the child splits it into separate arguments.
        assert_eq!(build_command_line("x", &["a\tb".to_string()]), "x \"a\tb\"");
        assert_eq!(build_command_line(" x", &[]), "\" x\"");
    }

    #[test]
    fn build_command_line_escapes_embedded_quotes_and_trailing_backslashes() {
        // Catches: quoting that drops the backslash before a `"`, which would
        // close the quote early and split one argument into garbage.
        let args = [r#"a"b"#.to_string()];
        assert_eq!(build_command_line("x", &args), "x \"a\\\"b\"");
        let trailing = [r"C:\a b\".to_string()];
        assert_eq!(build_command_line("x", &trailing), "x \"C:\\a b\\\\\"");
    }

    #[test]
    fn requote_args_preserves_a_spaced_launch_target() {
        // Catches: the elevated relaunch joining args with a bare space, which
        // splits `--launch "C:\Program Files\game.exe"` into two tokens so the
        // elevated instance targets the wrong exe (CreateProcessW fails).
        let args = vec![
            "--launch".to_string(),
            r"C:\Program Files\game.exe".to_string(),
            "--api".to_string(),
            "d3d9".to_string(),
        ];
        assert_eq!(
            requote_args(&args),
            r#"--launch "C:\Program Files\game.exe" --api d3d9"#
        );
    }

    #[test]
    fn parse_launch_collects_the_exe_and_every_trailing_arg() {
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let got = parse_launch(&args(&[
            "--launch",
            "hook-test.exe",
            "--api",
            "d3d9",
            "--frames",
            "2000",
        ]))
        .expect("parsed");
        assert_eq!(got.exe, "hook-test.exe");
        assert_eq!(
            got.args,
            vec![
                "--api".to_string(),
                "d3d9".to_string(),
                "--frames".to_string(),
                "2000".to_string()
            ]
        );
    }

    #[test]
    fn parse_launch_is_none_when_incomplete_or_absent() {
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(parse_launch(&args(&["--launch"])), None);
        assert_eq!(parse_launch(&args(&["--launch", ""])), None);
        assert_eq!(parse_launch(&args(&["--capture-hook", "1"])), None);
    }

    #[test]
    fn frame_reader_summarizes_the_apis_in_the_window() {
        // Catches: the transcript naming only the newest record — a Vulkan
        // capture whose present is immediately followed by the ICD's nested
        // DXGI present would read as "dxgi.present1" and hide the real capture.
        use hook_ipc::{Api, FrameRecord, RingWriter, FLAG_PRESENT};
        let pid = 0x4D48_0002 | (std::process::id() & 0xFFFF);
        let mut mapping = FrameMapping::create(pid).expect("create");
        {
            let mut w = RingWriter::init(mapping.as_mut_slice(), 1_000_000_000, pid).expect("init");
            for i in 0..3u64 {
                w.publish(&FrameRecord {
                    qpc_start: i * 8_000_000,
                    api: Api::VkQueuePresentKHR,
                    flags: FLAG_PRESENT,
                    ..Default::default()
                });
            }
            for i in 0..2u64 {
                w.publish(&FrameRecord {
                    qpc_start: i * 8_000_000 + 1_000_000,
                    api: Api::DxgiPresent1,
                    flags: FLAG_PRESENT,
                    ..Default::default()
                });
            }
        }
        let reader = FrameReader::open(pid).expect("open");
        assert_eq!(
            reader.api_counts(1000.0),
            vec![("vk.queuepresent", 3), ("dxgi.present1", 2)],
            "the window is summarized by api, most frequent first"
        );
    }

    #[test]
    fn remote_call_rejects_a_timed_out_or_failed_wait() {
        // Catches: treating a bounded wait that did not signal the thread as a
        // completed remote call. `GetExitCodeThread` on a still-running thread
        // returns `STILL_ACTIVE` (259), so a timed-out `mh_install` would be
        // reported as an installed mask (259) and the host would believe a
        // half-patched target was successfully hooked.
        use windows::Win32::Foundation::{WAIT_FAILED, WAIT_OBJECT_0, WAIT_TIMEOUT};
        assert!(remote_call_completed(WAIT_OBJECT_0));
        assert!(!remote_call_completed(WAIT_TIMEOUT));
        assert!(!remote_call_completed(WAIT_FAILED));
    }

    #[test]
    fn remote_alloc_frees_the_target_buffer_on_drop() {
        // Catches: leaking the remote path buffer when injection fails after
        // `VirtualAllocEx` (WriteProcessMemory / CreateRemoteThread / the remote
        // install). The target would keep a page it never asked for until it
        // exits. Exercised against our own process so the real `VirtualFreeEx`
        // runs.
        use windows::Win32::System::Memory::{
            VirtualAllocEx, VirtualQuery, MEMORY_BASIC_INFORMATION, MEM_COMMIT, MEM_FREE,
            MEM_RESERVE, PAGE_READWRITE,
        };
        use windows::Win32::System::Threading::{OpenProcess, PROCESS_ALL_ACCESS};
        let process =
            unsafe { OpenProcess(PROCESS_ALL_ACCESS, false, std::process::id()) }.expect("self");
        // SAFETY: allocating one page in our own process.
        let addr =
            unsafe { VirtualAllocEx(process, None, 64, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE) };
        assert!(!addr.is_null(), "VirtualAllocEx failed");
        {
            // The guard frees the buffer when it goes out of scope.
            let _owned = RemoteAlloc { process, addr };
        }
        let mut mbi = MEMORY_BASIC_INFORMATION::default();
        // SAFETY: `addr` was just allocated and freed; querying it is valid.
        let written = unsafe {
            VirtualQuery(
                Some(addr as *const core::ffi::c_void),
                &mut mbi,
                std::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
            )
        };
        // SAFETY: `process` is our own handle, closed once here.
        unsafe {
            let _ = CloseHandle(process);
        }
        assert!(written > 0, "VirtualQuery failed");
        assert_eq!(
            mbi.State, MEM_FREE,
            "the remote buffer must be freed when the guard drops"
        );
    }

    #[test]
    fn ensure_ring_does_not_zero_a_live_ring_on_a_second_call() {
        // Catches: a second `ensure_ring` re-initializing (zeroing) a block a
        // live recorder is already publishing into. `--launch` calls `ensure_ring`
        // while the target's recorder may already have attached; zeroing the
        // header would reset the sequence and drop every frame.
        use hook_ipc::{Api, FrameRecord, RingReader, RingWriter, FLAG_PRESENT};
        let pid = 0x4D48_0020 | (std::process::id() & 0xFFFF);
        let mut first = ensure_ring(pid).expect("first ensure creates and initializes");
        {
            let mut w = RingWriter::resume(first.as_mut_slice()).expect("resume the host ring");
            w.publish(&FrameRecord {
                qpc_start: 1,
                api: Api::DxgiPresent,
                flags: FLAG_PRESENT,
                ..Default::default()
            });
        }
        // A second ensure must attach to the live, initialized block unchanged.
        let again = ensure_ring(pid).expect("second ensure");
        let r = RingReader::new(again.as_slice()).expect("attach");
        assert_eq!(
            r.header().next_seq,
            1,
            "a live ring must not be re-initialized by a second ensure"
        );
        drop(first);
    }

    #[test]
    fn module_bases_lists_our_own_module_and_is_empty_for_an_impossible_pid() {
        // Catches: the retry loop returning stale/partial data (must be a fresh
        // snapshot each attempt) and a persistent failure not degrading to an
        // empty list — `inject` would then scan a stale module list for the
        // deny-list and the recorder base.
        let exe = std::env::current_exe().expect("current_exe");
        let exe_name = exe
            .file_name()
            .expect("exe file name")
            .to_string_lossy()
            .into_owned();
        let ours = module_bases(std::process::id());
        assert!(
            ours.iter()
                .any(|(base, name)| *base != 0 && name.eq_ignore_ascii_case(&exe_name)),
            "our own module must be listed with a non-zero base: {ours:?}"
        );
        assert!(
            module_bases(u32::MAX).is_empty(),
            "a persistent snapshot failure must yield an empty list, never stale data"
        );
    }

    #[test]
    fn frame_reader_reads_a_published_ring() {
        use hook_ipc::{Api, FrameRecord, RingWriter, FLAG_PRESENT};
        let pid = 0x4D48_0000 | (std::process::id() & 0xFFFF);
        let mut mapping = FrameMapping::create(pid).expect("create");
        {
            let mut w = RingWriter::init(mapping.as_mut_slice(), 1_000_000_000, pid).expect("init");
            for i in 0..5u64 {
                w.publish(&FrameRecord {
                    qpc_start: i * 16_666_667,
                    api: Api::DxgiPresent,
                    flags: FLAG_PRESENT,
                    ..Default::default()
                });
            }
        }
        let reader = FrameReader::open(pid).expect("open");
        assert_eq!(reader.header().expect("header").next_seq, 5);
        let t = reader.trailing(1000.0).expect("trailing");
        assert_eq!(t.frames, 5);
        assert!((t.fps - 60.0).abs() < 0.5, "{t:?}");
    }
}
