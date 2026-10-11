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
use std::sync::atomic::{AtomicBool, Ordering};

use hook_ipc::{FrameMapping, Header, RingReader, RingWriter, Trailing};
use windows::core::{s, BOOL, PCWSTR, PWSTR};
use windows::Win32::Foundation::{CloseHandle, HANDLE, WAIT_EVENT, WAIT_OBJECT_0};
use windows::Win32::System::Console::SetConsoleCtrlHandler;
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
/// Frames read back per sample.
const READBACK_FRAMES: usize = 512;

/// Cleared by the console Ctrl+C handler so the capture/read/follow loops exit
/// cleanly (finishing the status line) instead of being killed. A command with
/// no `[secs]` runs until this is cleared.
static RUNNING: AtomicBool = AtomicBool::new(true);

/// Console control handler: ask the loops to stop. Returns TRUE (handled).
unsafe extern "system" fn ctrl_handler(_ctrl_type: u32) -> BOOL {
    RUNNING.store(false, Ordering::SeqCst);
    BOOL(1)
}

/// Register [`ctrl_handler`]; failure is ignored (loops still end on their own
/// bound or when the process is killed).
pub(crate) fn install_ctrl_handler() {
    // SAFETY: registering a process-global handler with a valid signature.
    let _ = unsafe { SetConsoleCtrlHandler(Some(ctrl_handler), true) };
}

/// True until Ctrl+C / window close was requested.
pub(crate) fn should_run() -> bool {
    RUNNING.load(Ordering::SeqCst)
}

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

/// Parsed `--capture-hook <pid|name> [secs]`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CaptureArgs {
    /// A numeric pid, or a process image name (case-insensitive, `.exe` optional).
    pub target: String,
    /// Capture window in seconds; `None` runs until Ctrl+C.
    pub secs: Option<u64>,
}

/// Parse `--capture-hook <pid|name> [secs]` from the process arguments.
pub fn parse_capture_hook(args: &[String]) -> Option<CaptureArgs> {
    let i = args.iter().position(|a| a == "--capture-hook")?;
    let target = args.get(i + 1)?;
    if target.is_empty() {
        return None;
    }
    // A missing/unparseable `[secs]` means "run until Ctrl+C".
    let secs = args.get(i + 2).and_then(|s| s.parse().ok());
    Some(CaptureArgs {
        target: target.clone(),
        secs,
    })
}

/// Parsed `--launch <exe> [args...]`: the target to start suspended and inject.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LaunchArgs {
    pub exe: String,
    pub args: Vec<String>,
    /// `--launch-secs <n>`: bound the capture window (default: until it exits).
    pub secs: Option<u64>,
}

/// Parsed `--read-frames <pid|name> [secs]`: observe an existing ring read-only.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReadFramesArgs {
    /// A numeric pid, or a process image name (case-insensitive, `.exe` optional).
    pub target: String,
    /// Observe window in seconds; `None` runs until Ctrl+C.
    pub secs: Option<u64>,
}

/// Parse `--read-frames <pid|name> [secs]` from the process arguments. The
/// observer never injects; it opens a ring another process (the injected
/// recorder or the Vulkan implicit layer) created.
pub fn parse_read_frames(args: &[String]) -> Option<ReadFramesArgs> {
    let i = args.iter().position(|a| a == "--read-frames")?;
    let target = args.get(i + 1)?;
    if target.is_empty() {
        return None;
    }
    // A missing/unparseable `[secs]` means "run until Ctrl+C".
    let secs = args.get(i + 2).and_then(|s| s.parse().ok());
    Some(ReadFramesArgs {
        target: target.clone(),
        secs,
    })
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
    // `--launch-secs` is a minihud flag, so it must precede `--launch`
    // (everything after the exe belongs to the child).
    let secs = args[..i]
        .windows(2)
        .find(|w| w[0] == "--launch-secs")
        .and_then(|w| w[1].parse().ok());
    Some(LaunchArgs {
        exe: exe.clone(),
        args: args[i + 2..].to_vec(),
        secs,
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
    // `MINIHUD_HOOK_DLL` names an alternate recorder build next to the exe. It
    // lets an operator inject a fresh build while a live target still holds the
    // default file name loaded (a running target locks the DLL it loaded).
    if let Some(name) = std::env::var_os("MINIHUD_HOOK_DLL") {
        let dll = dir.join(&name);
        if dll.exists() {
            return Ok(dll);
        }
        return Err(format!(
            "MINIHUD_HOOK_DLL={:?} not found next to the exe ({})",
            name,
            dir.display()
        ));
    }
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

/// True for the recorder DLL's module name: the plain spellings (`hook_rt.dll`,
/// `hook-rt.dll`) and the content-addressed staged copies
/// (`hook_rt-<hash>.dll`).
fn is_recorder_dll(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    (n.starts_with("hook_rt") || n.starts_with("hook-rt")) && n.ends_with(".dll")
}

/// Pick the recorder module base from `bases` (`(base, name)`), preferring the
/// exact file name `want` we injected over any other recorder spelling already
/// loaded in the target.
///
/// Catches: a target that already has the *other* recorder spelling loaded (e.g.
/// a previous injection under a different name). Matching only by
/// [`is_recorder_dll`] could pick that module and compute the wrong remote entry
/// point, calling an address that is not our `mh_install`.
fn pick_recorder_base(bases: &[(usize, String)], want: &str) -> Option<usize> {
    bases
        .iter()
        .find(|(_, name)| name.eq_ignore_ascii_case(want))
        .or_else(|| bases.iter().find(|(_, name)| is_recorder_dll(name)))
        .map(|(base, _)| *base)
}

/// Content-addressed file name for a staged recorder copy.
///
/// The name encodes a hash of the DLL bytes, so a rebuilt recorder gets a new
/// name and a still-running target's locked older copy is never overwritten.
fn staged_name(bytes: &[u8]) -> String {
    use std::collections::hash_map::DefaultHasher;
    use std::hash::{Hash, Hasher};
    let mut h = DefaultHasher::new();
    bytes.hash(&mut h);
    format!("hook_rt-{:016x}.dll", h.finish())
}

/// Directory for staged recorder copies: `%LOCALAPPDATA%\minihud`, else temp.
fn staging_dir() -> PathBuf {
    std::env::var_os("LOCALAPPDATA")
        .map(PathBuf::from)
        .unwrap_or_else(std::env::temp_dir)
        .join("minihud")
}

/// Copy the recorder DLL to a per-build path **outside the build tree** and
/// return it.
///
/// A target locks the DLL it loaded for its whole life. Injecting the build
/// artifact directly (`target/<profile>/hook_rt.dll`) then makes `cargo build` /
/// `cargo clean` fail with "file is in use by <game>" until the target exits.
/// Staging by content hash keeps the build tree free and independent of any
/// running application. Stale copies are pruned best-effort (a locked one is
/// left until its target exits).
fn staged_recorder(src: &Path) -> Result<PathBuf, String> {
    let bytes = std::fs::read(src).map_err(|e| format!("read {}: {e}", src.display()))?;
    let dir = staging_dir();
    std::fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    let dest = dir.join(staged_name(&bytes));
    if !dest.exists() {
        std::fs::write(&dest, &bytes).map_err(|e| format!("write {}: {e}", dest.display()))?;
    }
    prune_staged(&dir, &dest);
    Ok(dest)
}

/// Best-effort removal of staged recorder copies other than `keep`.
fn prune_staged(dir: &Path, keep: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let p = entry.path();
        let is_staged = p
            .file_name()
            .and_then(|n| n.to_str())
            .is_some_and(|n| n.starts_with("hook_rt-") && n.ends_with(".dll"));
        if is_staged && p != keep {
            // Ignore failures: a copy a live target holds cannot be removed yet.
            let _ = std::fs::remove_file(&p);
        }
    }
}

/// Null-terminated UTF-16.
fn wide(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// The base and module name of an already-loaded recorder in `pid`, so
/// [`inject`] reuses it instead of loading a second recorder. `None` when no
/// recorder is present.
fn find_loaded_recorder(pid: u32) -> Option<(usize, String)> {
    module_bases(pid)
        .into_iter()
        .find(|(_, n)| is_recorder_dll(n))
}

/// Inject the recorder into `pid` and call `mh_install`. Returns the
/// installed-API bitmask reported by the DLL.
pub fn inject(pid: u32) -> Result<u32, String> {
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

    // Stage the current build (content-addressed name).
    let dll = staged_recorder(&hook_dll_path()?)?;
    // Derive the swapchain-vtable RVA in this process (the target may refuse a
    // dummy swapchain if it is fullscreen-exclusive); the recorder patches it.
    let swapchain_rva = swapchain_vtable_rva();
    let want = dll
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("")
        .to_string();

    // A target that already has a recorder must not get a second one (two
    // recorders on one ring double-count). If it is the *current* build, re-run
    // `mh_install` and reuse it; if it is a different (older) build, **replace**
    // it — unload + load the fresh one — so a running target gets the latest
    // recorder in place, never needing a restart.
    if let Some((base, name)) = find_loaded_recorder(pid) {
        let local = staging_dir().join(&name);
        if name.eq_ignore_ascii_case(&want) && local.exists() {
            let mask = call_mh_install(&process, base, &local, swapchain_rva)?;
            if has_present_hook(mask) {
                tracing::debug!("inject: pid {pid} reused recorder {name} (mask {mask:#x})");
                return Ok(mask);
            }
            tracing::debug!(
                "inject: pid {pid} recorder {name} has no present hook (mask {mask:#x}); replacing it"
            );
        } else {
            tracing::debug!("inject: pid {pid} has recorder {name}; replacing with {want}");
        }
        if local.exists() {
            match unload_recorder(&process, pid, base, &local) {
                Ok(()) => {} // unloaded; the fresh build is loaded below
                Err(e) => {
                    // Not safely replaceable (older build): reuse what is loaded.
                    tracing::debug!("inject: pid {pid} not replacing {name}: {e}; reusing it");
                    return call_mh_install(&process, base, &local, swapchain_rva);
                }
            }
        }
    }

    tracing::debug!("inject: pid {pid}; recorder staged at {}", dll.display());
    let remote_base = load_recorder(&process, pid, &dll)?;
    call_mh_install(&process, remote_base, &dll, swapchain_rva)
}

/// Remote-call `mh_install` on a module at `base` (its local file `dll` gives the
/// export RVA). `swapchain_rva` is the host-derived swapchain-vtable RVA in
/// `dxgi.dll`, carried to the recorder as the call's param. Returns the
/// installed mask; errors on a target panic.
fn call_mh_install(
    process: &ProcHandle,
    base: usize,
    dll: &Path,
    swapchain_rva: usize,
) -> Result<u32, String> {
    let rva = export_rva(dll, "mh_install")?;
    let install = base + rva;
    tracing::debug!(
        "inject: recorder base {base:#x}; mh_install at {install:#x} (rva {rva:#x}); swapchain vtable rva {swapchain_rva:#x}"
    );
    // SAFETY: `install` is the remote `mh_install`; its param carries the
    // host-derived swapchain-vtable RVA.
    let mask = unsafe {
        remote_call(
            process.0,
            install as *const core::ffi::c_void,
            swapchain_rva as *const core::ffi::c_void,
        )
    }?;
    if mask == u32::MAX {
        return Err("mh_install panicked in the target".into());
    }
    tracing::debug!("inject: mh_install returned mask {mask:#x}");
    Ok(mask)
}

/// The RVA of the shared DXGI swapchain vtable in `dxgi.dll`, derived by creating
/// a dummy swapchain **in this process** (where one is allowed). The recorder
/// patches `dxgi_base + rva` in the target, reaching `Present` even when the
/// target is fullscreen-exclusive and refuses a swapchain of its own. 0 on
/// failure.
fn swapchain_vtable_rva() -> usize {
    use windows::Win32::Foundation::{HMODULE, TRUE};
    use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
    use windows::Win32::Graphics::Direct3D11::{
        D3D11CreateDeviceAndSwapChain, ID3D11Device, ID3D11DeviceContext, D3D11_CREATE_DEVICE_FLAG,
    };
    use windows::Win32::Graphics::Dxgi::Common::{
        DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_MODE_DESC, DXGI_MODE_SCALING_UNSPECIFIED,
        DXGI_MODE_SCANLINE_ORDER_UNSPECIFIED, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
    };
    use windows::Win32::Graphics::Dxgi::{
        IDXGIAdapter, IDXGISwapChain, DXGI_SWAP_CHAIN_DESC, DXGI_SWAP_EFFECT_DISCARD,
        DXGI_USAGE_RENDER_TARGET_OUTPUT,
    };
    use windows::Win32::UI::WindowsAndMessaging::GetDesktopWindow;

    let desc = DXGI_SWAP_CHAIN_DESC {
        BufferDesc: DXGI_MODE_DESC {
            Width: 8,
            Height: 8,
            RefreshRate: DXGI_RATIONAL {
                Numerator: 0,
                Denominator: 0,
            },
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            ScanlineOrdering: DXGI_MODE_SCANLINE_ORDER_UNSPECIFIED,
            Scaling: DXGI_MODE_SCALING_UNSPECIFIED,
        },
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
        BufferCount: 1,
        OutputWindow: unsafe { GetDesktopWindow() },
        Windowed: TRUE,
        SwapEffect: DXGI_SWAP_EFFECT_DISCARD,
        Flags: 0,
    };
    let levels = [D3D_FEATURE_LEVEL_11_0];
    let mut swapchain: Option<IDXGISwapChain> = None;
    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;
    // SAFETY: all out-pointers are valid; `desc` is initialized.
    let ok = unsafe {
        D3D11CreateDeviceAndSwapChain(
            None::<&IDXGIAdapter>,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_FLAG(0),
            Some(&levels),
            7,
            Some(&desc),
            Some(&mut swapchain),
            Some(&mut device),
            None,
            Some(&mut context),
        )
    };
    let Some(sc) = swapchain.filter(|_| ok.is_ok()) else {
        return 0;
    };
    // SAFETY: `sc` is a live COM interface; its first word is the vtable.
    let vtable = unsafe { *(windows::core::Interface::as_raw(&sc) as *mut *mut usize) } as usize;
    // SAFETY: querying a loaded module by name; no side effects.
    let Ok(dxgi) = (unsafe { GetModuleHandleW(windows::core::w!("dxgi.dll")) }) else {
        return 0;
    };
    if dxgi.is_invalid() || vtable < dxgi.0 as usize {
        return 0;
    }
    vtable - dxgi.0 as usize
}

/// True when `mask` has a frame-producing (present/swap) hook installed.
fn has_present_hook(mask: u32) -> bool {
    use hook_ipc::Api;
    [
        Api::DxgiPresent,
        Api::DxgiPresent1,
        Api::D3d9Present,
        Api::WglSwapBuffers,
        Api::VkQueuePresentKHR,
    ]
    .iter()
    .any(|a| mask & (1u32 << (a.as_u16() - 1)) != 0)
}

/// The `rescan_gen` sentinel a recorder publishes when its rescan thread exits
/// (mirrors `hook_rt::install::RESCAN_STOPPED`).
const RECORDER_STOPPED_GEN: u32 = 0xDEAD_BEEF;

/// Unload the recorder module at `base` so a fresh build can replace it: run
/// `mh_uninstall` (which stops the rescan thread), wait for it to exit, then
/// `FreeLibrary` the module.
fn unload_recorder(
    process: &ProcHandle,
    pid: u32,
    base: usize,
    local: &Path,
) -> Result<(), String> {
    let rva = export_rva(local, "mh_uninstall")?;
    let uninstall = base + rva;
    // SAFETY: `uninstall` is the remote `mh_uninstall`; null argument.
    unsafe {
        remote_call(
            process.0,
            uninstall as *const core::ffi::c_void,
            core::ptr::null(),
        )
    }?;
    // Wait for the rescan thread to publish the stopped sentinel.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    let mut stopped = false;
    while std::time::Instant::now() < deadline {
        stopped = FrameReader::open_existing(pid)
            .ok()
            .and_then(|r| r.status())
            .map(|s| s.rescan_gen == RECORDER_STOPPED_GEN)
            .unwrap_or(false);
        if stopped {
            break;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    if !stopped {
        // The module predates the stop protocol (its rescan thread cannot be
        // told to exit). `FreeLibrary` would unmap code the thread is still
        // running → target crash. Do NOT unload; the caller reuses it instead.
        return Err("recorder did not stop (older build); leaving it in place".into());
    }
    // SAFETY: kernel32 is loaded in every process.
    let kernel32 = unsafe { GetModuleHandleW(windows::core::w!("kernel32.dll")) }
        .map_err(|e| format!("GetModuleHandleW(kernel32): {e}"))?;
    let free_library = unsafe { GetProcAddress(kernel32, s!("FreeLibrary")) }
        .ok_or("GetProcAddress(FreeLibrary) failed")?;
    // SAFETY: `free_library` is FreeLibrary and `base` its HMODULE argument.
    unsafe {
        remote_call(
            process.0,
            free_library as usize as *const core::ffi::c_void,
            base as *const core::ffi::c_void,
        )
    }?;
    tracing::debug!("inject: unloaded old recorder at {base:#x}");
    Ok(())
}

/// Remote-load the recorder DLL into the target and return its base address.
fn load_recorder(process: &ProcHandle, pid: u32, dll: &Path) -> Result<usize, String> {
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
    // the thread exit code cannot carry a full HMODULE). Prefer the exact name
    // we injected.
    let want = dll.file_name().and_then(|n| n.to_str()).unwrap_or("");
    pick_recorder_base(&module_bases(pid), want)
        .ok_or_else(|| "the recorder DLL did not load in the target".to_string())
}

/// Remote-call the injected runtime's `mh_uninstall` in `pid`, restoring every
/// patch. Bounded (the remote call has a fixed wait budget) and fail-open: the
/// caller logs and keeps going on any error.
pub fn unhook(pid: u32) -> Result<(), String> {
    if pid == 0 {
        return Err("refusing to unhook pid 0".into());
    }
    let dll = staged_recorder(&hook_dll_path()?)?;
    // SAFETY: `OpenProcess` with a pid is a normal Win32 call.
    let process = unsafe { OpenProcess(PROCESS_ALL_ACCESS, false, pid) }
        .map_err(|e| format!("OpenProcess({pid}): {e}"))?;
    let process = ProcHandle(process);

    let want = dll.file_name().and_then(|n| n.to_str()).unwrap_or("");
    let remote_base = pick_recorder_base(&module_bases(pid), want)
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

    /// The decoded status block (install + rescan tracking).
    pub fn status(&self) -> Option<hook_ipc::Status> {
        RingReader::new(self.mapping.as_slice())
            .ok()
            .map(|r| r.status())
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

/// Inject into the target process, then show fps/frametime for `secs` seconds
/// (or until Ctrl+C when `secs` is `None`).
pub fn capture_hook(target: &str, secs: Option<u64>, interval: u64) -> Result<(), String> {
    let pid = resolve_target(target)?;
    let mask = inject(pid)?;
    let reader = FrameReader::open(pid)?;
    let label = target_label(pid);
    let window = capture_window(secs);
    tracing::info!(
        "injected into {label} (pid {pid}) — installed hooks {} (mask {mask:#x}); capturing {window}",
        describe_installed(mask)
    );
    install_ctrl_handler();
    let deadline = capture_deadline(secs);
    let mut last_seq: Option<u64> = None;
    while should_run() && deadline.is_none_or(|d| std::time::Instant::now() < d) {
        print_sample(pid, &reader, &mut last_seq);
        std::thread::sleep(std::time::Duration::from_millis(interval.max(1)));
    }
    Ok(())
}

/// `--read-frames`: open an **existing** ring for the target (no injection — the
/// Vulkan implicit layer or an injected recorder created it) and show
/// fps/frametime for `secs` seconds (or until Ctrl+C when `secs` is `None`).
pub fn read_frames(target: &str, secs: Option<u64>, interval: u64) -> Result<(), String> {
    let pid = resolve_target(target)?;
    let reader = FrameReader::open_existing(pid)?;
    let installed = reader
        .status()
        .map(|s| describe_installed(s.installed))
        .unwrap_or_else(|| "none".to_string());
    tracing::info!(
        "observing {} (pid {pid}, read-only) — installed hooks {installed}; capturing {}",
        target_label(pid),
        capture_window(secs)
    );
    install_ctrl_handler();
    let deadline = capture_deadline(secs);
    let mut last_seq: Option<u64> = None;
    while should_run() && deadline.is_none_or(|d| std::time::Instant::now() < d) {
        print_sample(pid, &reader, &mut last_seq);
        std::thread::sleep(std::time::Duration::from_millis(interval.max(1)));
    }
    Ok(())
}

/// A human window description for the injection log line.
fn capture_window(secs: Option<u64>) -> String {
    match secs {
        Some(s) => format!("for {s}s"),
        None => "until Ctrl+C".to_string(),
    }
}

/// The capture deadline, or `None` to run until Ctrl+C.
fn capture_deadline(secs: Option<u64>) -> Option<std::time::Instant> {
    secs.map(|s| std::time::Instant::now() + std::time::Duration::from_secs(s.max(1)))
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

/// `n: label, label, ...` (or `none`) for an installed-API mask.
fn describe_installed(mask: u32) -> String {
    let labels = hook_ipc::installed_labels(mask);
    if labels.is_empty() {
        "none".to_string()
    } else {
        format!("{}: {}", labels.len(), labels.join(", "))
    }
}

/// The image file name for `pid` (basename), or `pid <n>` when unknown.
fn target_label(pid: u32) -> String {
    match proc::process_name(pid) {
        Some(path) => std::path::Path::new(&path)
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or(path),
        None => format!("pid {pid}"),
    }
}

/// Resolve a CLI target — a numeric pid or a process image name — to a pid.
fn resolve_target(target: &str) -> Result<u32, String> {
    proc::find_pid(target).ok_or_else(|| format!("no running process matches {target:?}"))
}

/// Update the capture segment of the status line (concise fps/frametime + api)
/// and log the full tracking state at `DEBUG` (verbose, for debugging) so the
/// live line stays short enough to fit one console line.
///
/// `last_seq` carries the previous sample's ring sequence so a ring that has
/// stopped advancing (the recorder is gone or never installed a present hook) is
/// reported as **stale**, not as a frozen fps.
pub(crate) fn print_sample(pid: u32, reader: &FrameReader, last_seq: &mut Option<u64>) {
    let s = reader.status().unwrap_or_default();
    let installed = describe_installed(s.installed);
    let header = reader.header();
    let seq = header.map(|h| h.next_seq);
    let stale = matches!((seq, *last_seq), (Some(now), Some(prev)) if now == prev);
    if seq.is_some() {
        *last_seq = seq;
    }
    let line = match header {
        Some(h) if stale => format!(
            "pid {pid}: no new frames (stale) — ring frozen at {} rec, {} err (last {:#010x}) | installed: {installed}",
            h.next_seq, s.errors, s.last_error as u32
        ),
        Some(h) => match reader.trailing(1000.0) {
            Some(t) => {
                let api = api_summary(&reader.api_counts(1000.0));
                tracing::debug!(
                    "capture: pid {pid} {} rec, {} calls, {} err (last {:#010x}) | rescan gen {} x{}, attempts {} | installed {} | api {}",
                    h.next_seq,
                    h.calls,
                    s.errors,
                    s.last_error as u32,
                    s.rescan_gen,
                    s.rescan_count,
                    s.attempted,
                    installed,
                    api
                );
                format!(
                    "pid {pid}: {:.1} fps  {:.2} ms  ({api})",
                    t.fps, t.frametime_ms
                )
            }
            None => {
                tracing::debug!(
                    "capture: pid {pid} waiting | {} rec, {} calls, {} err | installed {}",
                    h.next_seq,
                    h.calls,
                    s.errors,
                    installed
                );
                format!("pid {pid}: waiting for presents")
            }
        },
        None => format!("pid {pid}: ring not initialized"),
    };
    crate::console::status_capture(line);
}

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

/// `--launch`: start `exe` suspended, inject the recorder, then show fps until
/// the child exits (or for `secs` if `--launch-secs` was given), then unhook.
pub fn launch_capture(
    exe: &str,
    args: &[String],
    secs: Option<u64>,
    interval: u64,
) -> Result<(), String> {
    let pid = launch_and_inject(exe, args)?;
    let reader = FrameReader::open(pid)?;
    let installed = reader
        .status()
        .map(|s| describe_installed(s.installed))
        .unwrap_or_else(|| "none".to_string());
    let window = match secs {
        Some(s) => format!("for {s}s"),
        None => "until it exits".to_string(),
    };
    tracing::info!(
        "launched and injected {exe} (pid {pid}) — installed hooks {installed}; capturing {window}"
    );
    let deadline = secs.map(|s| std::time::Instant::now() + std::time::Duration::from_secs(s));
    let mut last_seq: Option<u64> = None;
    while proc::process_alive(pid) && deadline.is_none_or(|d| std::time::Instant::now() < d) {
        print_sample(pid, &reader, &mut last_seq);
        std::thread::sleep(std::time::Duration::from_millis(interval.max(1)));
    }
    if proc::process_alive(pid) {
        match unhook(pid) {
            Ok(()) => tracing::info!("unhooked {} (pid {pid})", target_label(pid)),
            Err(e) => tracing::warn!("unhook pid {pid}: {e}"),
        }
    } else {
        tracing::info!("pid {pid} exited; nothing to unhook");
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
    fn describe_installed_formats_the_hook_set() {
        // Catches: a decoded hook set that is empty for a non-zero mask (the
        // injection log would claim nothing installed) or a stray count.
        assert_eq!(describe_installed(0), "none");
        assert_eq!(describe_installed(1), "1: dxgi.present");
        assert_eq!(describe_installed(0b11), "2: dxgi.present, dxgi.present1");
    }

    #[test]
    fn pick_recorder_base_prefers_the_exact_injected_name() {
        // Catches: injecting `hook-rt.dll` into a target that already has
        // `hook_rt.dll` loaded and then resolving the *other* module — the
        // remote `mh_install` address would be computed against the wrong base.
        let bases = vec![
            (0x1000usize, "hook_rt.dll".to_string()),
            (0x2000, "hook-rt.dll".to_string()),
        ];
        assert_eq!(pick_recorder_base(&bases, "hook-rt.dll"), Some(0x2000));
        assert_eq!(pick_recorder_base(&bases, "hook_rt.dll"), Some(0x1000));
        // Falls back to any recorder spelling when the exact name is absent.
        assert_eq!(pick_recorder_base(&bases, "other.dll"), Some(0x1000));
        assert_eq!(pick_recorder_base(&[], "hook_rt.dll"), None);
    }

    #[test]
    fn staged_name_is_content_addressed_and_recorder_shaped() {
        // Catches: a staged name that collides across builds (a rebuilt DLL
        // would overwrite a copy a live target still holds, re-locking the build
        // tree) or one that no longer looks like the recorder module.
        let a = staged_name(b"build one");
        let b = staged_name(b"build two");
        assert_ne!(a, b, "different content must get different names");
        assert_eq!(a, staged_name(b"build one"), "same content -> same name");
        assert!(a.starts_with("hook_rt-") && a.ends_with(".dll"), "{a}");
    }

    #[test]
    fn has_present_hook_detects_present_bits() {
        // Catches: the exclusive-fullscreen path (factory hooks only, no present
        // hook) being mistaken for a working capture, or a real present hook
        // being missed so a good recorder is needlessly replaced.
        assert!(!has_present_hook(0));
        assert!(has_present_hook(1), "dxgi.present");
        assert!(has_present_hook(1 << 1), "dxgi.present1");
        assert!(has_present_hook(1 << 8), "d3d9.present");
        assert!(has_present_hook(1 << 19), "vk.queuepresent");
        assert!(
            !has_present_hook(0xf0),
            "the factory hooks alone are not a present hook"
        );
    }

    #[test]
    fn parses_capture_hook_arguments() {
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        // No [secs] means "run until Ctrl+C".
        assert_eq!(
            parse_capture_hook(&args(&["--capture-hook", "1234"])),
            Some(CaptureArgs {
                target: "1234".into(),
                secs: None
            })
        );
        assert_eq!(
            parse_capture_hook(&args(&["--capture-hook", "42", "5"])),
            Some(CaptureArgs {
                target: "42".into(),
                secs: Some(5)
            })
        );
        // A process name is a valid target too.
        assert_eq!(
            parse_capture_hook(&args(&["--capture-hook", "Overwatch.exe"])),
            Some(CaptureArgs {
                target: "Overwatch.exe".into(),
                secs: None
            })
        );
        assert_eq!(parse_capture_hook(&args(&["--capture-hook"])), None);
        assert_eq!(parse_capture_hook(&args(&["--capture-hook", ""])), None);
        assert_eq!(parse_capture_hook(&args(&["--other"])), None);
    }

    #[test]
    fn parses_read_frames_arguments() {
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            parse_read_frames(&args(&["--read-frames", "1234"])),
            Some(ReadFramesArgs {
                target: "1234".into(),
                secs: None
            })
        );
        assert_eq!(
            parse_read_frames(&args(&["--read-frames", "42", "4"])),
            Some(ReadFramesArgs {
                target: "42".into(),
                secs: Some(4)
            })
        );
        assert_eq!(parse_read_frames(&args(&["--read-frames"])), None);
        assert_eq!(parse_read_frames(&args(&["--read-frames", ""])), None);
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
        assert_eq!(got.secs, None, "no --launch-secs means until-exit");
    }

    #[test]
    fn parse_launch_reads_launch_secs_before_the_exe() {
        // Catches: `--launch-secs` (a minihud flag) leaking into the child's
        // args, or being ignored so the capture runs unbounded.
        let args = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let got = parse_launch(&args(&[
            "--launch-secs",
            "30",
            "--launch",
            "game.exe",
            "--x",
        ]))
        .expect("parsed");
        assert_eq!(got.exe, "game.exe");
        assert_eq!(got.secs, Some(30));
        assert_eq!(got.args, vec!["--x".to_string()]);
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
        // Distinct *high* bytes keep these test pids from colliding under the
        // shared `cargo test` process (an `|` on the low id bits would merge
        // `0x4D48_0000`, `0x4D48_0002`, `0x4D48_0020`).
        let pid = 0x4D48_0000 | (std::process::id() & 0xFFFF);
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
        let pid = 0x4D49_0000 | (std::process::id() & 0xFFFF);
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
        let pid = 0x4D4A_0000 | (std::process::id() & 0xFFFF);
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
