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
pub fn install() -> u32 {
    UNINSTALLED.store(false, Ordering::Release);
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
    // Serialize with an in-flight rescan pass: hold the lock across the restore
    // so a pass that already passed the flag check either finishes first (and
    // its patches are restored here) or waits, then sees `UNINSTALLED`.
    let _guard = patch_lock();
    // SAFETY: the patched modules are still loaded (uninstall runs in-process).
    unsafe { detour::restore_all_patches() };
    if let Ok(mut s) = PATCHED_SLOTS.lock() {
        s.clear();
    }
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
fn bootstrap_vtables() {
    let _ = std::panic::catch_unwind(bootstrap_dxgi);
    let _ = std::panic::catch_unwind(bootstrap_d3d9);
}

/// Create a minimal D3D11 swapchain and patch the shared DXGI swapchain vtable.
fn bootstrap_dxgi() {
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
        }
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
    use std::sync::Once;
    static ONCE: Once = Once::new();
    ONCE.call_once(|| {
        std::thread::spawn(|| {
            let start = Instant::now();
            let mut generation: u32 = 0;
            loop {
                std::thread::sleep(rescan_interval(start.elapsed()));
                generation = generation.wrapping_add(1);
                let found = rescan_once();
                detour::with_recorder(|r| {
                    r.note_rescan(generation);
                    if found == 0 {
                        r.note_call();
                    }
                });
            }
        });
    });
}

#[cfg(test)]
mod tests {
    /// The installer must not crash the host process, even without a GPU.
    /// It is intentionally not asserting a mask (headless CI may have none).
    #[test]
    fn install_and_uninstall_smoke() {
        let _mask = super::install();
        super::uninstall();
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
