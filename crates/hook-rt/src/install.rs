//! Installer: open the ring, hook loaded modules, bootstrap shared vtables,
//! and rescan for late-loaded graphics DLLs.
//!
//! Everything is fail-open: any failure records an error and returns rather
//! than unwinding into the host.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use hook_ipc::{Api, FrameMapping};
use windows::core::Interface;

use crate::api::{self, VTABLE_HOOKS};
use crate::detour::{self, IatOriginal};
use crate::patch::VtableSlotPatch;
use crate::pe;
use crate::record::{qpc_frequency, Recorder};

/// How often the rescan thread re-scans loaded modules once the burst ends.
pub const RESCAN_PERIOD_MS: u64 = 500;

/// The fast cadence the rescan thread uses for the first [`RESCAN_BURST_MS`]
/// after install.
pub const RESCAN_BURST_PERIOD_MS: u64 = 50;

/// How long after install the rescan stays on the fast burst cadence.
pub const RESCAN_BURST_MS: u64 = 2000;

/// How long after install the rescan keeps retrying the vtable bootstrap while
/// no present hook is installed. A target that already owns the display can
/// refuse the dummy device creation (`E_ACCESSDENIED`) — observed when injecting
/// a game at launch — and the display settles within a second or two, so
/// retrying there self-heals it instead of leaving the target permanently
/// unhooked.
pub const BOOTSTRAP_RETRY_MS: u64 = 15_000;

/// IAT slots already patched, so a rescan does not double-hook.
static PATCHED_SLOTS: Mutex<Vec<usize>> = Mutex::new(Vec::new());

/// Set once [`uninstall`] has restored every patch. The rescan thread sleeps
/// between passes, so it can wake after uninstall; without this it would find
/// `PATCHED_SLOTS` cleared and re-install every hook, silently undoing the
/// uninstall. [`install`] clears it so a fresh install starts clean.
static UNINSTALLED: AtomicBool = AtomicBool::new(false);

/// True once [`uninstall`] has run for this process.
fn is_uninstalled() -> bool {
    UNINSTALLED.load(Ordering::Acquire)
}

/// Set by [`uninstall`] to tell the rescan thread to **exit** (not merely stop
/// patching), so the host can `FreeLibrary` the module and load a fresh one.
/// Cleared by [`install`]. This is what makes the recorder replaceable in place
/// — no target restart.
static STOP: AtomicBool = AtomicBool::new(false);

/// True while a rescan thread is running, so [`start_rescan_thread`] spawns at
/// most one and can respawn after the previous one exited.
static RESCAN_RUNNING: AtomicBool = AtomicBool::new(false);

/// Sentinel published as `rescan_gen` when the rescan thread exits, so the host
/// can wait for the thread to be gone before unloading the module.
pub const RESCAN_STOPPED: u32 = 0xDEAD_BEEF;

/// Serializes a rescan pass against [`uninstall`].
///
/// [`UNINSTALLED`] alone is check-then-act: a rescan that already read it
/// `false` and is inside `from_slot` would `keep_patch` *after*
/// `restore_all_patches` cleared the list, leaving one patch live once
/// `mh_uninstall` has returned. Holding this lock for the whole pass and for the
/// restore makes the two mutually exclusive: whichever wins runs to completion,
/// and the loser observes a consistent state. Never taken on the present hot
/// path (only the install/rescan/uninstall paths), so the recorder stays
/// lock-free per present.
static PATCH_LOCK: Mutex<()> = Mutex::new(());

/// Take the patch lock, recovering from poisoning: a panic by a previous holder
/// must not wedge the installer (the guarded state is the OS patch tables, which
/// remain consistent).
fn patch_lock() -> std::sync::MutexGuard<'static, ()> {
    PATCH_LOCK.lock().unwrap_or_else(|e| e.into_inner())
}

/// Install the recorder and every hook. Returns the installed-API bitmask
/// (0 when the recorder could not be created).
///
/// Re-runnable: a second call (or a call after [`uninstall`]) reuses the existing
/// recorder and re-installs the hooks, so the host can re-inject a target that
/// already has the recorder loaded without loading a second module.
pub fn install(swapchain_rva: usize) -> u32 {
    UNINSTALLED.store(false, Ordering::Release);
    STOP.store(false, Ordering::Release);
    if !detour::has_recorder() {
        let pid = unsafe { windows::Win32::System::Threading::GetCurrentProcessId() };
        let Ok(mapping) = FrameMapping::open_or_create(pid) else {
            return 0;
        };
        let created = mapping.is_created();
        // Leak the view: the recorder holds a `'static` slice for the process life.
        let mapping = Box::leak(Box::new(mapping));
        let ptr = mapping.as_mut_slice().as_mut_ptr();
        let len = mapping.len();
        // SAFETY: the leaked mapping is writable and outlives the process.
        let rec = unsafe {
            if created {
                Recorder::create(ptr, len, qpc_frequency(), pid)
            } else {
                Recorder::attach(ptr, len)
            }
        };
        let Some(rec) = rec else {
            return 0;
        };
        if !detour::set_recorder(rec) {
            return 0;
        }
    }

    // Patch the shared swapchain vtable by the host-supplied RVA — the
    // fullscreen-exclusive path, where the target refuses a dummy swapchain.
    patch_swapchain_vtable_rva(swapchain_rva);

    rescan_once();
    bootstrap_vtables();

    let mask = installed_mask();
    detour::with_recorder(|r| {
        r.set_installed(mask);
        r.note_attempt();
    });
    mask
}

/// Restore every vtable and IAT patch.
pub fn uninstall() {
    // Set before restoring so a rescan that wakes mid-uninstall sees the flag
    // and does not re-patch what we are about to restore.
    UNINSTALLED.store(true, Ordering::Release);
    // Tell the rescan thread to exit so the host may unload this module.
    STOP.store(true, Ordering::Release);
    // Serialize with an in-flight rescan pass: hold the lock across the restore
    // so a pass that already passed the flag check either finishes first (and
    // its patches are restored here) or waits, then sees `UNINSTALLED`.
    let _guard = patch_lock();
    // SAFETY: the patched modules are still loaded (uninstall runs in-process).
    unsafe { detour::restore_all_patches() };
    if let Ok(mut s) = PATCHED_SLOTS.lock() {
        s.clear();
    }
    // Publish "nothing installed" so the host's status reflects the unhook
    // instead of the stale mask from before it.
    detour::with_recorder(|r| r.set_installed(0));
}

/// One rescan pass: hook the application's own import table. Returns the
/// number of slots patched this pass.
///
/// Only the main module is patched. Its import table is where the app's
/// `d3d11.dll!D3D11CreateDeviceAndSwapChain`, `dxgi.dll!CreateDXGIFactory*`,
/// `d3d9.dll!Direct3DCreate9*`, `opengl32.dll!wglSwapBuffers`,
/// `gdi32.dll!SwapBuffers`, `libEGL.dll!eglSwapBuffers` and
/// `vulkan-1.dll!vkQueuePresentKHR` entries live. Patching a *graphics DLL's
/// own* import table is not required and was observed to destabilise the
/// target, so it is deliberately excluded.
pub fn rescan_once() -> usize {
    // Hold the patch lock for the *whole* pass (including the own-module check)
    // so it cannot interleave with `uninstall`'s restore. See `PATCH_LOCK`.
    let _guard = patch_lock();
    let own = own_module_base();
    let main = unsafe { windows::Win32::System::LibraryLoader::GetModuleHandleW(None) }
        .map(|h| h.0 as *mut u8)
        .unwrap_or(core::ptr::null_mut());
    if main.is_null() || main == own {
        return 0;
    }
    // SAFETY: `main` is this process's main-module base.
    unsafe { install_iat_in_module(main) }
}

/// The sleep before the next rescan, given `elapsed` since the rescan thread
/// started.
///
/// The thread bursts at [`RESCAN_BURST_PERIOD_MS`] for the first
/// [`RESCAN_BURST_MS`] after install, then settles to the steady
/// [`RESCAN_PERIOD_MS`]. The burst bounds the one-time delay-load swap window:
/// `__delayLoadHelper2` overwrites a delay-loaded swap slot (e.g.
/// `opengl32!wglSwapBuffers`) on its first call, and a steady-only cadence
/// could wait a full period to re-patch it — measured 14–18 frames lost.
/// Monotonic (never decreases as `elapsed` grows) and bounded to
/// `[RESCAN_BURST_PERIOD_MS, RESCAN_PERIOD_MS]`, so the burst cannot restart and
/// the thread cannot spin on a zero interval.
fn rescan_interval(elapsed: Duration) -> Duration {
    if elapsed < Duration::from_millis(RESCAN_BURST_MS) {
        Duration::from_millis(RESCAN_BURST_PERIOD_MS)
    } else {
        Duration::from_millis(RESCAN_PERIOD_MS)
    }
}

/// True when `key` (an IAT slot address) is a not-yet-patched entry for a hooked
/// `dll!func`.
///
/// A standard import is skipped once patched. A **delay-load** import is
/// re-attempted on every rescan: `__delayLoadHelper2` overwrites its slot with
/// the resolved pointer the first time the function is called, which would
/// otherwise un-hook it permanently. Re-patching is idempotent (the slot patch
/// refuses a slot that already holds the detour), so a stable slot is a no-op.
/// Once `uninstalled`, nothing is patched — the rescan thread must never undo
/// `mh_uninstall`. Pure so the rescan's skip decision is tested directly.
fn should_patch(
    patched: &[usize],
    key: usize,
    dll: &str,
    func: &str,
    delay: bool,
    uninstalled: bool,
) -> bool {
    !uninstalled && (delay || !patched.contains(&key)) && api::iat_spec(dll, func).is_some()
}

/// True when a refused slot patch is a genuine failure rather than the normal
/// steady state of a re-attempted delay slot.
///
/// A delay slot that already holds our detour makes the slot patch refuse
/// (idempotent), which is expected on every rescan — not an error. Only a
/// refusal with the slot *not* holding the detour (e.g. the page could not be
/// made writable) is a real failure. Pure, so the decision is tested.
fn patch_refusal_is_an_error(current: usize, detour: usize) -> bool {
    current != detour
}

/// Patch the IAT entries of one module that match our hook targets.
///
/// # Safety
/// `base` must be a valid mapped PE image base.
unsafe fn install_iat_in_module(base: *mut u8) -> usize {
    let mut n = 0;
    // SAFETY: `base` is a module base supplied by the caller.
    for imp in unsafe { pe::imports(base) } {
        let key = imp.slot as usize;
        let wanted = PATCHED_SLOTS
            .lock()
            .map(|s| should_patch(&s, key, &imp.dll, &imp.func, imp.delay, is_uninstalled()))
            .unwrap_or(false);
        if !wanted {
            continue;
        }
        let Some(target) = detour::iat_target(&imp.dll, &imp.func) else {
            continue;
        };
        // SAFETY: `imp.slot` is a live IAT entry in the module.
        let Some(p) = (unsafe { VtableSlotPatch::from_slot(imp.slot, target.detour) }) else {
            // A delay slot re-attempted on a later rescan already holds our
            // detour; that refusal is the steady state, not a failure.
            // SAFETY: `imp.slot` is a live IAT entry in the module.
            if patch_refusal_is_an_error(unsafe { *imp.slot }, target.detour) {
                detour::with_recorder(|r| r.note_error(-1));
            }
            continue;
        };
        match target.original {
            IatOriginal::Api(api) => detour::set_original(api, p.original()),
            IatOriginal::Slot(slot) => slot.store(p.original(), Ordering::Release),
        }
        detour::keep_patch(p);
        // A delay slot is deliberately not deduplicated: it is re-attempted each
        // rescan so a `__delayLoadHelper2` overwrite self-heals. Recording it
        // would only grow without bound and block the re-attempt.
        if !imp.delay {
            if let Ok(mut s) = PATCHED_SLOTS.lock() {
                s.push(key);
            }
        }
        n += 1;
    }
    n
}

/// Create dummy DXGI and D3D9 objects once to reach the shared vtables, patch
/// them, then release the dummies. Best-effort and panic-guarded.
/// Patch the shared DXGI swapchain vtable reached by `rva` (relative to the
/// target's `dxgi.dll` base).
///
/// The host derives the RVA from a dummy swapchain in its own process (where one
/// is allowed) and passes it in; `dxgi.dll` and its vtable are identical on this
/// machine, so the RVA matches even though the target — fullscreen-exclusive —
/// refuses to create a swapchain itself. This is what reaches `Present` there.
fn patch_swapchain_vtable_rva(rva: usize) -> bool {
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    if rva == 0 {
        return false;
    }
    // SAFETY: querying a loaded module by name; no side effects.
    let Ok(dxgi) = (unsafe { GetModuleHandleW(windows::core::w!("dxgi.dll")) }) else {
        return false;
    };
    if dxgi.is_invalid() {
        return false;
    }
    let vtable = dxgi.0 as usize + rva;
    // SAFETY: the host validated this RVA as the swapchain vtable on the same
    // dxgi.dll; its slots are the IDXGISwapChain present/resize/fullscreen set.
    unsafe { detour::patch_swapchain_vtable(vtable) };
    true
}

fn bootstrap_vtables() {
    // The factory vtable needs no display → always safe and non-invasive.
    let _ = std::panic::catch_unwind(bootstrap_dxgi_factory);
    if invasive_bootstrap_allowed(d3d_fullscreen()) && !skip_invasive_for_test() {
        let _ = std::panic::catch_unwind(bootstrap_dxgi);
        let _ = std::panic::catch_unwind(bootstrap_d3d9);
    }
}

/// Whether the *invasive* dummy-device bootstraps may run.
///
/// They create a graphics device, which **blocks (~8 s) and crashes** a target
/// that owns the display in **exclusive fullscreen** (verified against a live
/// fullscreen-exclusive game), so they are skipped there. The factory hook still
/// catches a swapchain recreation, and the rescan retry runs them once the
/// target is windowed again.
fn invasive_bootstrap_allowed(fullscreen: bool) -> bool {
    !fullscreen
}

/// Test/diagnostic escape: `MINIHUD_SKIP_INVASIVE=1` makes the recorder skip the
/// dummy-device bootstraps, so the RVA-derived swapchain-vtable patch can be
/// verified in isolation.
fn skip_invasive_for_test() -> bool {
    std::env::var_os("MINIHUD_SKIP_INVASIVE").is_some()
}

/// True when a Direct3D application is running in exclusive fullscreen (the
/// shell reports `QUNS_RUNNING_D3D_FULL_SCREEN`).
fn d3d_fullscreen() -> bool {
    use windows::Win32::UI::Shell::{SHQueryUserNotificationState, QUNS_RUNNING_D3D_FULL_SCREEN};
    // SAFETY: querying the shell notification state; takes no arguments.
    matches!(
        unsafe { SHQueryUserNotificationState() },
        Ok(s) if s == QUNS_RUNNING_D3D_FULL_SCREEN
    )
}

/// Patch the shared DXGI **factory** vtable by creating a dummy factory.
///
/// Creating a factory needs **no display**, so this works even when the target
/// owns the display (fullscreen) and a dummy *swapchain* is refused
/// (`E_ACCESSDENIED`). It hooks `CreateSwapChain*` on the (shared) factory
/// vtable, so when the target (re)creates its swapchain — a fullscreen toggle,
/// resolution change, or device-lost — the new swapchain's vtable is patched.
/// Since the runtime's swapchain vtable is shared, that reaches the target's
/// already-existing swapchain too.
fn bootstrap_dxgi_factory() {
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory2, IDXGIFactory2, DXGI_CREATE_FACTORY_FLAGS,
    };
    // SAFETY: creating a factory needs no display; the out-interface is valid.
    let Ok(factory) =
        (unsafe { CreateDXGIFactory2::<IDXGIFactory2>(DXGI_CREATE_FACTORY_FLAGS(0)) })
    else {
        return;
    };
    // SAFETY: `factory` is a live IDXGIFactory2*.
    unsafe { detour::patch_factory(factory.as_raw()) };
}

/// Trivial window procedure for the bootstrap window: defer to the default.
unsafe extern "system" fn bootstrap_wndproc(
    hwnd: windows::Win32::Foundation::HWND,
    msg: u32,
    wparam: windows::Win32::Foundation::WPARAM,
    lparam: windows::Win32::Foundation::LPARAM,
) -> windows::Win32::Foundation::LRESULT {
    // SAFETY: forwarding the window's own message unchanged.
    unsafe { windows::Win32::UI::WindowsAndMessaging::DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// A dedicated hidden popup window for the dummy swapchain, created once.
///
/// `GetDesktopWindow()` can be refused (`E_ACCESSDENIED`) by a target that
/// already owns the display, which leaves DXGI capture entirely absent. A window
/// owned by this process is the reliable swapchain target (the same approach
/// `hook-test` uses). Returns `None` if the window could not be created, so the
/// caller can fall back.
fn bootstrap_window() -> Option<windows::Win32::Foundation::HWND> {
    use windows::Win32::Foundation::{HINSTANCE, HWND};
    use windows::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows::Win32::UI::WindowsAndMessaging::{
        CreateWindowExW, RegisterClassW, CW_USEDEFAULT, WNDCLASSW, WS_POPUP,
    };

    static ONCE: std::sync::Once = std::sync::Once::new();
    static mut HWND_SLOT: isize = 0;

    ONCE.call_once(|| {
        let Ok(instance) = (unsafe { GetModuleHandleW(None) }) else {
            return;
        };
        let class = windows::core::w!("minihud_bootstrap");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(bootstrap_wndproc),
            hInstance: HINSTANCE(instance.0),
            lpszClassName: class,
            ..Default::default()
        };
        // SAFETY: `wc` is a fully initialized class description.
        if unsafe { RegisterClassW(&wc) } == 0 {
            return;
        }
        // SAFETY: the class is registered; a hidden 8x8 popup is a valid target.
        let hwnd = unsafe {
            CreateWindowExW(
                Default::default(),
                class,
                windows::core::w!("minihud bootstrap"),
                WS_POPUP,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                8,
                8,
                None,
                None,
                Some(HINSTANCE(instance.0)),
                None,
            )
        };
        if let Ok(h) = hwnd {
            // SAFETY: written once, under `Once`.
            unsafe { HWND_SLOT = h.0 as isize };
        }
    });

    // SAFETY: written once under `Once` before any read here.
    let v = unsafe { HWND_SLOT };
    if v != 0 {
        Some(HWND(v as *mut core::ffi::c_void))
    } else {
        None
    }
}

/// Create a dummy DXGI swapchain and patch the shared swapchain vtable.
///
/// Tries a **flip-model** swapchain first — the path real overlays use, and the
/// one a display-owning (fullscreen) target accepts — then falls back to the
/// legacy bitblt `D3D11CreateDeviceAndSwapChain`. Never needs a target restart.
fn bootstrap_dxgi() {
    use windows::Win32::UI::WindowsAndMessaging::GetDesktopWindow;
    let hwnd = bootstrap_window().unwrap_or_else(|| unsafe { GetDesktopWindow() });
    if let Err(flip_code) = bootstrap_dxgi_flip(hwnd) {
        // Flip-model failed; try the legacy bitblt path, then re-record the flip
        // reason **last** so the host sees why the modern path failed instead of
        // the bitblt HRESULT masking it.
        bootstrap_dxgi_bitblt(hwnd);
        detour::with_recorder(|r| r.note_error(flip_code));
    }
}

/// Flip-model dummy swapchain via `IDXGIFactory2::CreateSwapChainForHwnd`.
///
/// Returns `Err(marker)` naming the failing step so a bootstrap failure is
/// diagnosable (`0x7F01` device, `0x7F02` IDXGIDevice, `0x7F03` adapter,
/// `0x7F04` factory, `0x7F05xxxx` CreateSwapChainForHwnd with the HRESULT low
/// bits).
fn bootstrap_dxgi_flip(hwnd: windows::Win32::Foundation::HWND) -> Result<(), i32> {
    use windows::Win32::Foundation::{HMODULE, TRUE};
    use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
    use windows::Win32::Graphics::Direct3D11::{
        D3D11CreateDevice, ID3D11Device, D3D11_CREATE_DEVICE_FLAG,
    };
    use windows::Win32::Graphics::Dxgi::Common::{
        DXGI_ALPHA_MODE_IGNORE, DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_SAMPLE_DESC,
    };
    use windows::Win32::Graphics::Dxgi::{
        IDXGIDevice, IDXGIFactory2, DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC1,
        DXGI_SWAP_EFFECT_FLIP_DISCARD, DXGI_USAGE_RENDER_TARGET_OUTPUT,
    };

    let levels = [D3D_FEATURE_LEVEL_11_0];
    let mut device: Option<ID3D11Device> = None;
    // SAFETY: standard device creation; the out-pointer is valid.
    let created = unsafe {
        D3D11CreateDevice(
            None::<&windows::Win32::Graphics::Dxgi::IDXGIAdapter>,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_FLAG(0),
            Some(&levels),
            7,
            Some(&mut device),
            None,
            None,
        )
    };
    let Some(device) = device.filter(|_| created.is_ok()) else {
        return Err(0x7F01);
    };

    // device -> IDXGIDevice -> adapter -> IDXGIFactory2.
    let dxgi_dev = device.cast::<IDXGIDevice>().map_err(|_| 0x7F02)?;
    let adapter = (unsafe { dxgi_dev.GetAdapter() }).map_err(|_| 0x7F03)?;
    let factory = (unsafe { adapter.GetParent::<IDXGIFactory2>() }).map_err(|_| 0x7F04)?;

    let desc = DXGI_SWAP_CHAIN_DESC1 {
        Width: 8,
        Height: 8,
        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
        Stereo: TRUE,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
        BufferCount: 2,
        Scaling: DXGI_SCALING_STRETCH,
        SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
        AlphaMode: DXGI_ALPHA_MODE_IGNORE,
        Flags: 0,
    };
    // SAFETY: live device/window; `desc` is a fully initialized DESC1.
    let made = unsafe {
        factory.CreateSwapChainForHwnd(
            &device,
            hwnd,
            &desc,
            None::<*const windows::Win32::Graphics::Dxgi::DXGI_SWAP_CHAIN_FULLSCREEN_DESC>,
            None::<&windows::Win32::Graphics::Dxgi::IDXGIOutput>,
        )
    };
    let sc = made.map_err(|e| 0x7F05_0000 | (e.code().0 & 0xFFFF))?;
    // SAFETY: `sc` is a live IDXGISwapChain1*.
    unsafe { detour::patch_swapchain(sc.as_raw()) };
    Ok(())
}

/// Legacy bitblt dummy swapchain (`D3D11CreateDeviceAndSwapChain`).
fn bootstrap_dxgi_bitblt(hwnd: windows::Win32::Foundation::HWND) {
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
        OutputWindow: hwnd,
        Windowed: TRUE,
        SwapEffect: DXGI_SWAP_EFFECT_DISCARD,
        Flags: 0,
    };
    let levels = [D3D_FEATURE_LEVEL_11_0];
    let mut swapchain: Option<IDXGISwapChain> = None;
    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;
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
    if ok.is_ok() {
        if let Some(sc) = &swapchain {
            // SAFETY: `sc` is a live IDXGISwapChain*.
            unsafe { detour::patch_swapchain(sc.as_raw()) };
        } else {
            // Success HRESULT but no swapchain out-pointer: record a marker so
            // the host can tell this apart from a hard creation failure.
            detour::with_recorder(|r| r.note_error(0x7001));
        }
    } else {
        // Record the HRESULT: a target that already owns the GPU/DXGI state can
        // refuse the dummy device+swapchain, which leaves DXGI capture absent.
        let code = ok.as_ref().err().map(|e| e.code().0).unwrap_or(-1);
        detour::with_recorder(|r| r.note_error(code));
    }
    drop((swapchain, device, context));
}

/// Create a minimal D3D9 device and patch the shared D3D9 device vtable.
fn bootstrap_d3d9() {
    use windows::Win32::Foundation::{FALSE, TRUE};
    use windows::Win32::Graphics::Direct3D9::{
        Direct3DCreate9, IDirect3DDevice9, D3DADAPTER_DEFAULT, D3DCREATE_SOFTWARE_VERTEXPROCESSING,
        D3DDEVTYPE_HAL, D3DFMT_UNKNOWN, D3DFMT_X8R8G8B8, D3DMULTISAMPLE_NONE,
        D3DPRESENT_INTERVAL_IMMEDIATE, D3DPRESENT_PARAMETERS, D3DSWAPEFFECT_DISCARD,
        D3D_SDK_VERSION,
    };
    use windows::Win32::UI::WindowsAndMessaging::GetDesktopWindow;

    let Some(d3d) = (unsafe { Direct3DCreate9(D3D_SDK_VERSION) }) else {
        return;
    };
    // SAFETY: `d3d` is a live IDirect3D9*.
    unsafe { detour::patch_d3d9(d3d.as_raw()) };

    let window = unsafe { GetDesktopWindow() };
    let mut params = D3DPRESENT_PARAMETERS {
        BackBufferWidth: 8,
        BackBufferHeight: 8,
        BackBufferFormat: D3DFMT_X8R8G8B8,
        BackBufferCount: 1,
        MultiSampleType: D3DMULTISAMPLE_NONE,
        MultiSampleQuality: 0,
        SwapEffect: D3DSWAPEFFECT_DISCARD,
        hDeviceWindow: window,
        Windowed: TRUE,
        EnableAutoDepthStencil: FALSE,
        AutoDepthStencilFormat: D3DFMT_UNKNOWN,
        Flags: 0,
        FullScreen_RefreshRateInHz: 0,
        PresentationInterval: D3DPRESENT_INTERVAL_IMMEDIATE as u32,
    };
    let mut device: Option<IDirect3DDevice9> = None;
    let ok = unsafe {
        d3d.CreateDevice(
            D3DADAPTER_DEFAULT,
            D3DDEVTYPE_HAL,
            window,
            D3DCREATE_SOFTWARE_VERTEXPROCESSING as u32,
            &mut params,
            &mut device,
        )
    };
    if ok.is_ok() {
        if let Some(dev) = &device {
            // SAFETY: `dev` is a live IDirect3DDevice9*.
            unsafe { detour::patch_d3d9_device(dev.as_raw()) };
        }
    }
    drop(device);
}

/// The installed-API bitmask (bit `api - 1` set when its original is stored).
fn installed_mask() -> u32 {
    let mut mask = 0u32;
    let mut mark = |api: Api| mask |= 1u32 << (api.as_u16() - 1);
    for spec in VTABLE_HOOKS {
        if detour::original(spec.api) != 0 {
            mark(spec.api);
        }
    }
    for api in [
        Api::WglSwapBuffers,
        Api::WglSwapLayerBuffers,
        Api::GdiSwapBuffers,
        Api::EglSwapBuffers,
        Api::VkQueuePresentKHR,
    ] {
        if detour::original(api) != 0 {
            mark(api);
        }
    }
    mask
}

/// True when at least one frame-producing (present/swap) hook is installed.
///
/// The bootstrap retry stops as soon as this is true — a dummy device creation
/// that succeeded patched the shared vtable, so the target can be recorded.
fn any_present_hook() -> bool {
    [
        Api::DxgiPresent,
        Api::DxgiPresent1,
        Api::D3d9Present,
        Api::WglSwapBuffers,
        Api::VkQueuePresentKHR,
    ]
    .iter()
    .any(|a| detour::original(*a) != 0)
}

/// Base address of this module, so the rescan never hooks the recorder itself.
fn own_module_base() -> *mut u8 {
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::System::LibraryLoader::{
        GetModuleHandleExW, GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS,
        GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
    };
    let mut h = HMODULE::default();
    let addr = install as *const () as *const u16;
    let flags =
        GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT;
    // SAFETY: `addr` points into this module; the handle is unchanged-refcount.
    let ok = unsafe { GetModuleHandleExW(flags, windows::core::PCWSTR(addr), &mut h) };
    if ok.is_ok() {
        h.0 as *mut u8
    } else {
        core::ptr::null_mut()
    }
}

/// Spawn the adaptive rescan thread: a fast burst for the first
/// [`RESCAN_BURST_MS`] after install (to re-patch delay-load slots as soon as
/// `__delayLoadHelper2` overwrites them), then the steady [`RESCAN_PERIOD_MS`]
/// (to pick up late-loaded modules). Idempotent: only the first call spawns.
pub fn start_rescan_thread() {
    if RESCAN_RUNNING.swap(true, Ordering::AcqRel) {
        return; // a rescan thread is already running
    }
    std::thread::spawn(|| {
        let start = Instant::now();
        let mut generation: u32 = 0;
        while !STOP.load(Ordering::Acquire) {
            std::thread::sleep(rescan_interval(start.elapsed()));
            if STOP.load(Ordering::Acquire) {
                break;
            }
            generation = generation.wrapping_add(1);
            rescan_once();
            // Retry the vtable bootstrap while nothing present-producing is in
            // and the retry window is open: a launch-time `E_ACCESSDENIED` from
            // the dummy device creation self-heals once the display settles.
            if !is_uninstalled()
                && start.elapsed() < Duration::from_millis(BOOTSTRAP_RETRY_MS)
                && !any_present_hook()
            {
                bootstrap_vtables();
            }
            detour::with_recorder(|r| {
                r.note_rescan(generation);
                // Republish the mask so a successful retry is visible to the
                // host (the installed set can grow after `install`).
                r.set_installed(installed_mask());
            });
        }
        // The thread is gone: signal the host so it can unload this module.
        detour::with_recorder(|r| r.note_rescan(RESCAN_STOPPED));
        RESCAN_RUNNING.store(false, Ordering::Release);
    });
}

#[cfg(test)]
mod tests {
    #[test]
    fn invasive_bootstraps_are_skipped_in_fullscreen() {
        // Catches: running the dummy-device bootstraps while a target owns the
        // display in exclusive fullscreen — they block ~8 s and crash the target
        // (verified live). The factory bootstrap (no device) still runs.
        assert!(
            !super::invasive_bootstrap_allowed(true),
            "invasive bootstraps must be skipped in exclusive fullscreen"
        );
        assert!(
            super::invasive_bootstrap_allowed(false),
            "invasive bootstraps run when the target does not own the display"
        );
    }

    /// The installer must not crash the host process, even without a GPU.
    /// It is intentionally not asserting a mask (headless CI may have none).
    #[test]
    fn install_and_uninstall_smoke() {
        let _mask = super::install(0);
        super::uninstall();
    }

    /// Live GPU probe: does `bootstrap_dxgi` actually reach the shared swapchain
    /// vtable in a normal process? (Not a gate — needs a GPU.)
    #[test]
    #[ignore = "live GPU probe; run with --ignored --nocapture"]
    fn probe_bootstrap_dxgi_reaches_the_swapchain_vtable() {
        use hook_ipc::Api;
        let before = crate::detour::original(Api::DxgiPresent);
        super::bootstrap_dxgi();
        let after = crate::detour::original(Api::DxgiPresent);
        eprintln!("bootstrap_dxgi: DxgiPresent original before={before:#x} after={after:#x}");
        assert_ne!(
            after, 0,
            "bootstrap_dxgi must patch the DXGI swapchain vtable"
        );
    }

    #[test]
    fn installed_mask_marks_the_bit_for_each_stored_original() {
        use hook_ipc::Api;
        // Catches: an off-by-one in `1 << (api - 1)` that misreports which hooks
        // installed (the host prints and trusts this mask). Only asserts the
        // bits we set, so a same-process smoke test that set others is fine.
        let chosen = [Api::DxgiPresent, Api::D3d9Present, Api::VkQueuePresentKHR];
        for api in chosen {
            crate::detour::set_original(api, 0x1234);
        }
        let mask = super::installed_mask();
        for api in chosen {
            assert_ne!(
                mask & (1u32 << (api.as_u16() - 1)),
                0,
                "{api:?} bit missing from mask {mask:#x}"
            );
        }
        // Leave the shared globals as we found them.
        for api in chosen {
            crate::detour::set_original(api, 0);
        }
    }

    #[test]
    fn should_patch_skips_already_patched_slots_and_non_targets() {
        // Catches: a rescan re-patching a *standard* slot it already hooked (the
        // rescan thread runs every 500 ms; a missing dedup would re-enter
        // `from_slot` and error-spam), and patching an import we do not hook.
        let patched = [0x1000usize, 0x2000];
        assert!(
            !super::should_patch(
                &patched,
                0x1000,
                "dxgi.dll",
                "CreateDXGIFactory",
                false,
                false
            ),
            "an already-patched standard slot must be skipped"
        );
        assert!(
            super::should_patch(&[], 0x3000, "dxgi.dll", "CreateDXGIFactory", false, false),
            "a fresh hook target must be patched"
        );
        assert!(
            !super::should_patch(&[], 0x4000, "dxgi.dll", "NotAFunction", false, false),
            "a non-target import must be skipped"
        );
        assert!(
            !super::should_patch(&[], 0x5000, "other.dll", "CreateDXGIFactory", false, false),
            "an import from an unhooked DLL must be skipped"
        );
    }

    #[test]
    fn should_patch_reattempts_a_delay_slot_even_when_already_patched() {
        // Catches: deduplicating a delay-load slot. `__delayLoadHelper2` writes
        // the resolved pointer over our detour the first time the function is
        // called; if the rescan skipped the slot as "already patched" the hook
        // would be lost permanently. A delay slot must be re-attempted (the
        // patch itself is idempotent when the slot still holds the detour).
        let patched = [0x1000usize];
        assert!(
            super::should_patch(&patched, 0x1000, "d3d9.dll", "Direct3DCreate9", true, false),
            "a delay slot must be re-attempted on the rescan"
        );
        assert!(
            super::should_patch(
                &patched,
                0x1000,
                "opengl32.dll",
                "wglSwapBuffers",
                true,
                false
            ),
            "the delay-loaded wglSwapBuffers swap slot must be re-attempted too"
        );
        assert!(
            !super::should_patch(
                &patched,
                0x1000,
                "other.dll",
                "Direct3DCreate9",
                true,
                false
            ),
            "a delay slot is still only re-attempted for a hooked import"
        );
    }

    #[test]
    fn should_patch_never_repairs_a_slot_after_uninstall() {
        // Catches: the rescan thread re-patching after `mh_uninstall`. The thread
        // sleeps between passes, so an uninstall can land mid-window; with no
        // guard the next pass finds `PATCHED_SLOTS` cleared and re-installs every
        // hook — silently undoing the uninstall and leaving the target hooked.
        // The delay re-attempt path is the sharpest case: it patches
        // *unconditionally*, so it does not even need the cleared dedup list.
        assert!(
            !super::should_patch(&[], 0x3000, "dxgi.dll", "CreateDXGIFactory", false, true),
            "a standard slot must not be re-patched after uninstall"
        );
        assert!(
            !super::should_patch(&[], 0x1000, "d3d9.dll", "Direct3DCreate9", true, true),
            "a delay-acquisition slot must not be re-patched after uninstall"
        );
        assert!(
            !super::should_patch(&[], 0x1000, "opengl32.dll", "wglSwapBuffers", true, true),
            "the delay-loaded swap slot (unconditional re-attempt) must not be re-patched after uninstall"
        );
    }

    #[test]
    fn a_refused_patch_of_a_slot_already_holding_the_detour_is_not_an_error() {
        // Catches: treating the delay re-attempt's normal steady state as an
        // error. The slot patch refuses a slot that already holds the detour, so
        // every rescan of an intact delay slot would otherwise record
        // `note_error(-1)` and the recorder would report a stream of false
        // errors (observed: 18 "errors" in a clean d3d11 run with no real fault).
        assert!(
            !super::patch_refusal_is_an_error(0xDEAD, 0xDEAD),
            "a slot already holding our detour is not a failure"
        );
        assert!(
            super::patch_refusal_is_an_error(0xBEEF, 0xDEAD),
            "a slot that does not hold the detour is a genuine failure"
        );
    }

    #[test]
    fn rescan_once_never_hooks_the_recorders_own_module() {
        // Catches: dropping the `main == own` guard, which would make the
        // recorder patch its own import table — observed to destabilise the
        // target. In the test binary the main module IS the module holding
        // `install`, so the guard must short-circuit to 0.
        assert_eq!(
            super::rescan_once(),
            0,
            "the recorder must never hook its own module"
        );
    }

    #[test]
    fn rescan_interval_bursts_fast_then_settles_to_the_steady_cadence() {
        use std::time::Duration;
        // Catches: a fixed rescan period (no burst). A delay-loaded swap slot
        // (`opengl32!wglSwapBuffers`) is overwritten by `__delayLoadHelper2` on
        // its first call; with only the 500 ms cadence the re-patch could wait a
        // full period (measured 14-18 frames lost). Also catches a burst that
        // never ends (the rescan thread would run fast for the process life).
        let fast = Duration::from_millis(super::RESCAN_BURST_PERIOD_MS);
        let steady = Duration::from_millis(super::RESCAN_PERIOD_MS);

        assert_eq!(
            super::rescan_interval(Duration::ZERO),
            fast,
            "the first rescan after install must be on the fast burst cadence"
        );
        assert_eq!(
            super::rescan_interval(Duration::from_millis(super::RESCAN_BURST_MS - 1)),
            fast,
            "the last instant inside the burst window must still be fast"
        );
        assert_eq!(
            super::rescan_interval(Duration::from_millis(super::RESCAN_BURST_MS)),
            steady,
            "the first instant at/after the burst window must be on the steady cadence"
        );
        assert_eq!(
            super::rescan_interval(Duration::from_secs(600)),
            steady,
            "the steady cadence must hold for the life of the process"
        );
        const {
            assert!(
                super::RESCAN_BURST_PERIOD_MS < super::RESCAN_PERIOD_MS,
                "the burst must actually be faster than the steady cadence"
            )
        }
    }

    #[test]
    fn rescan_interval_is_monotonic_and_bounded() {
        use std::time::Duration;
        // Catches: a non-monotonic cadence (a later `elapsed` yielding a shorter
        // interval would restart the burst) and an out-of-range interval (a 0 ms
        // sleep would spin the rescan thread; an unbounded one would let a
        // delay-loaded swap slot stay un-hooked past the burst).
        let lo = Duration::from_millis(super::RESCAN_BURST_PERIOD_MS);
        let hi = Duration::from_millis(super::RESCAN_PERIOD_MS);
        let mut prev = Duration::ZERO;
        let mut t = Duration::ZERO;
        while t <= Duration::from_secs(5) {
            let d = super::rescan_interval(t);
            assert!(d >= prev, "interval decreased at {t:?}: {d:?} < {prev:?}");
            assert!(
                d >= lo && d <= hi,
                "interval out of [{lo:?}, {hi:?}]: {d:?}"
            );
            prev = d;
            t += Duration::from_millis(25);
        }
    }

    #[test]
    fn a_rescan_pass_and_uninstall_are_mutually_exclusive() {
        // Catches: the residual uninstall race. `uninstall()` sets the flag and
        // restores, while a rescan that already read the flag `false` is inside
        // `from_slot`; its `keep_patch` lands after the restore cleared the list,
        // leaving one patch live *after* `mh_uninstall` returned. Both the pass
        // and the restore must take the same patch lock so they cannot
        // interleave — a pass that wins runs to completion (and is then
        // restored); a pass that loses sees `UNINSTALLED` and writes nothing.
        use std::sync::mpsc;
        use std::time::Duration;

        let guard = super::patch_lock();

        let (done_tx, done_rx) = mpsc::channel();
        let h_rescan = {
            let done_tx = done_tx.clone();
            std::thread::spawn(move || {
                super::rescan_once();
                let _ = done_tx.send(());
            })
        };
        assert!(
            done_rx.recv_timeout(Duration::from_millis(250)).is_err(),
            "rescan_once must wait for the in-flight patch holder"
        );

        let h_uninstall = std::thread::spawn(move || {
            super::uninstall();
            let _ = done_tx.send(());
        });
        assert!(
            done_rx.recv_timeout(Duration::from_millis(250)).is_err(),
            "uninstall must wait for the in-flight patch holder"
        );

        drop(guard);
        done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("rescan proceeds once the lock is free");
        done_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("uninstall proceeds once the lock is free");
        h_rescan.join().unwrap();
        h_uninstall.join().unwrap();
    }

    #[test]
    fn rescan_interval_is_steady_immediately_after_the_burst() {
        use std::time::Duration;
        // Catches: an off-by-one that keeps the fast cadence one step past
        // `RESCAN_BURST_MS` (or flips early). The boundary is exactly at
        // `RESCAN_BURST_MS`; just past it must be the steady cadence.
        let steady = Duration::from_millis(super::RESCAN_PERIOD_MS);
        assert_eq!(
            super::rescan_interval(Duration::from_millis(super::RESCAN_BURST_MS + 1)),
            steady,
            "one millisecond past the burst is steady"
        );
        assert_eq!(
            super::rescan_interval(Duration::from_millis(super::RESCAN_BURST_MS * 10)),
            steady
        );
    }
}
