//! Detour functions and the global hook state.
//!
//! Every detour is `extern "system"`, fail-open (it calls the original, or
//! returns a benign value if the original is missing), and records a QPC-stamped
//! frame on present/swap entry+exit. Acquisition detours forward and patch the
//! returned object's vtable. No detour renders anything.

use core::ffi::{c_char, c_void, CStr};
use core::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use hook_ipc::Api;

use crate::patch::VtableSlotPatch;
use crate::record::{qpc_now, Recorder};
use crate::vk::{vk_target, Detour};

/// Original function pointers for vtable/frame hooks, indexed by [`Api`].
static ORIGINALS: [AtomicUsize; 32] = [const { AtomicUsize::new(0) }; 32];

/// Store the original pointer for an API (set when a slot is patched).
pub fn set_original(api: Api, ptr: usize) {
    ORIGINALS[api.as_u16() as usize].store(ptr, Ordering::Release);
}

/// Load the original pointer for an API (0 when not installed).
pub fn original(api: Api) -> usize {
    ORIGINALS[api.as_u16() as usize].load(Ordering::Acquire)
}

// -- per-vtable-slot originals ----------------------------------------------

/// One patched COM vtable slot's original, keyed by the object's *class vtable*
/// pointer and the slot index.
///
/// A single per-[`Api`] original is not enough: two objects of *distinct*
/// classes patched for the same API (e.g. two D3D9 devices with separate device
/// vtables) would clobber each other's original, so the first detour would
/// forward to the second object's function. Keying on the class vtable keeps
/// each slot's original its own. Objects of the *same* class share a vtable and
/// therefore a key, which is correct: the slot holds the same function.
#[derive(Clone, Copy)]
struct VtableOriginal {
    vtable: usize,
    index: usize,
    original: usize,
}

/// How many distinct (class vtable, slot) pairs we expect to patch in one
/// target: a handful of swapchain/factory/device/queue classes. Fixed capacity
/// keeps the lookup allocation-free on the present hot path.
const MAX_VTABLE_ORIGINALS: usize = 64;

static VTABLE_ORIGINALS: Mutex<[Option<VtableOriginal>; MAX_VTABLE_ORIGINALS]> =
    Mutex::new([None; MAX_VTABLE_ORIGINALS]);

/// Insert into a registry array: replace an existing key's original, else take
/// the first free slot. Returns false when full. Pure, so it is tested directly.
fn registry_insert(
    reg: &mut [Option<VtableOriginal>],
    vtable: usize,
    index: usize,
    original: usize,
) -> bool {
    for v in reg.iter_mut().flatten() {
        if v.vtable == vtable && v.index == index {
            v.original = original;
            return true;
        }
    }
    for e in reg.iter_mut() {
        if e.is_none() {
            *e = Some(VtableOriginal {
                vtable,
                index,
                original,
            });
            return true;
        }
    }
    false
}

/// Look up a `(vtable, index)` pair in a registry array. Allocation-free.
fn registry_lookup(reg: &[Option<VtableOriginal>], vtable: usize, index: usize) -> Option<usize> {
    reg.iter()
        .flatten()
        .find(|v| v.vtable == vtable && v.index == index)
        .map(|v| v.original)
}

/// Remember the original for one patched (vtable, slot) pair. Returns false when
/// the lock is poisoned or the registry is full (the per-[`Api`] fallback still
/// stands).
fn register_vtable_original(vtable: usize, index: usize, original: usize) -> bool {
    match VTABLE_ORIGINALS.lock() {
        Ok(mut reg) => registry_insert(&mut reg[..], vtable, index, original),
        Err(_) => false,
    }
}

/// Look up the original for one patched (vtable, slot) pair. `None` on a
/// contended lock or a miss; the caller falls back to the per-[`Api`] original
/// (fail-open).
fn vtable_original(vtable: usize, index: usize) -> Option<usize> {
    let reg = VTABLE_ORIGINALS.try_lock().ok()?;
    registry_lookup(&reg[..], vtable, index)
}

/// The original pointer a detour should forward to.
///
/// For a COM vtable hook, `obj` is the interface and `slot` its vtable index;
/// the original is recovered from the object's *own* class vtable. For a
/// free-function (IAT) hook `slot` is `None` and the shared per-[`Api`] original
/// is used. Falls back to the per-[`Api`] original whenever the registry misses
/// or is contended (fail-open).
fn resolve_original(obj: *mut c_void, slot: Option<usize>, api: Api) -> usize {
    if let (Some(index), false) = (slot, obj.is_null()) {
        // SAFETY: for a COM vtable hook `obj` is a live interface pointer whose
        // first word is the class vtable pointer.
        let vtable = unsafe { crate::patch::vtable_of(obj) };
        if !vtable.is_null() {
            if let Some(orig) = vtable_original(vtable as usize, index) {
                return orig;
            }
        }
    }
    original(api)
}

static RECORDER: OnceLock<Mutex<Recorder>> = OnceLock::new();

/// Install the recorder. Returns false if one is already installed.
pub fn set_recorder(rec: Recorder) -> bool {
    RECORDER.set(Mutex::new(rec)).is_ok()
}

/// Run `f` against the recorder if it is free; a contended lock drops the
/// sample rather than blocking the present (fail-open).
pub fn with_recorder<F: FnOnce(&mut Recorder)>(f: F) {
    if let Some(m) = RECORDER.get() {
        if let Ok(mut g) = m.try_lock() {
            f(&mut g);
        }
    }
}

/// Record one frame (enter/exit QPC + handle).
fn record_frame(api: Api, start: u64, stop: u64, handle: u64, flags: u32) {
    // Test-only: let a unit test observe exactly which `Api`/handle each detour
    // stamps, without a live target. Compiled out of production builds.
    #[cfg(test)]
    tests::note_frame(api, start, stop, handle, flags);
    with_recorder(|r| r.record_present(api, start, stop, handle, flags));
}

/// Every slot patch installed, restored on uninstall.
static PATCHES: Mutex<Vec<VtableSlotPatch>> = Mutex::new(Vec::new());

/// Remember a patch for uninstall. Returns false if the lock is poisoned.
pub fn keep_patch(p: VtableSlotPatch) -> bool {
    match PATCHES.lock() {
        Ok(mut v) => {
            v.push(p);
            true
        }
        Err(_) => false,
    }
}

/// Restore every patch installed so far.
///
/// # Safety
/// The patched modules must still be loaded.
pub unsafe fn restore_all_patches() {
    if let Ok(mut v) = PATCHES.lock() {
        for p in v.iter_mut() {
            // SAFETY: patches were installed against live module memory and
            // the caller guarantees the modules are still loaded.
            unsafe { p.restore() };
        }
        v.clear();
    }
}

// -- free-function IAT originals --------------------------------------------

static ORIG_FACTORY0: AtomicUsize = AtomicUsize::new(0);
static ORIG_FACTORY1: AtomicUsize = AtomicUsize::new(0);
static ORIG_FACTORY2: AtomicUsize = AtomicUsize::new(0);
static ORIG_D3D11_CREATE: AtomicUsize = AtomicUsize::new(0);
static ORIG_D3D9_CREATE: AtomicUsize = AtomicUsize::new(0);
static ORIG_D3D9_CREATE_EX: AtomicUsize = AtomicUsize::new(0);
static ORIG_VK_CREATE_SWAPCHAIN: AtomicUsize = AtomicUsize::new(0);
static ORIG_VK_GET_IMAGES: AtomicUsize = AtomicUsize::new(0);
static ORIG_VK_CREATE_DEVICE: AtomicUsize = AtomicUsize::new(0);
static ORIG_VK_DESTROY_DEVICE: AtomicUsize = AtomicUsize::new(0);
static ORIG_VK_GET_INSTANCE_PROC: AtomicUsize = AtomicUsize::new(0);
static ORIG_VK_GET_DEVICE_PROC: AtomicUsize = AtomicUsize::new(0);

/// Where an IAT hook's original pointer is stored.
pub enum IatOriginal {
    /// Shared with a vtable/frame hook, keyed by API.
    Api(Api),
    /// A dedicated slot for a free-function acquisition detour.
    Slot(&'static AtomicUsize),
}

/// An IAT hook target: the detour address and where to save the old pointer.
pub struct IatTarget {
    pub detour: usize,
    pub original: IatOriginal,
}

/// Resolve an imported `dll!func` to its detour, or `None` if we do not hook it.
pub fn iat_target(dll: &str, func: &str) -> Option<IatTarget> {
    let d = dll.to_ascii_lowercase();
    let f = func;
    let t = |detour: usize, original: IatOriginal| Some(IatTarget { detour, original });
    match (d.as_str(), f) {
        ("dxgi.dll", "CreateDXGIFactory") => t(
            create_dxgi_factory as *const () as usize,
            IatOriginal::Slot(&ORIG_FACTORY0),
        ),
        ("dxgi.dll", "CreateDXGIFactory1") => t(
            create_dxgi_factory1 as *const () as usize,
            IatOriginal::Slot(&ORIG_FACTORY1),
        ),
        ("dxgi.dll", "CreateDXGIFactory2") => t(
            create_dxgi_factory2 as *const () as usize,
            IatOriginal::Slot(&ORIG_FACTORY2),
        ),
        ("d3d11.dll", "D3D11CreateDeviceAndSwapChain") => t(
            d3d11_create_device_and_swap_chain as *const () as usize,
            IatOriginal::Slot(&ORIG_D3D11_CREATE),
        ),
        ("d3d9.dll", "Direct3DCreate9") => t(
            direct3d_create9 as *const () as usize,
            IatOriginal::Slot(&ORIG_D3D9_CREATE),
        ),
        ("d3d9.dll", "Direct3DCreate9Ex") => t(
            direct3d_create9_ex as *const () as usize,
            IatOriginal::Slot(&ORIG_D3D9_CREATE_EX),
        ),
        ("opengl32.dll", "wglSwapBuffers") => t(
            wgl_swap_buffers as *const () as usize,
            IatOriginal::Api(Api::WglSwapBuffers),
        ),
        ("opengl32.dll", "wglSwapLayerBuffers") => t(
            wgl_swap_layer_buffers as *const () as usize,
            IatOriginal::Api(Api::WglSwapLayerBuffers),
        ),
        ("gdi32.dll", "SwapBuffers") => t(
            gdi_swap_buffers as *const () as usize,
            IatOriginal::Api(Api::GdiSwapBuffers),
        ),
        ("libegl.dll", "eglSwapBuffers") => t(
            egl_swap_buffers as *const () as usize,
            IatOriginal::Api(Api::EglSwapBuffers),
        ),
        ("vulkan-1.dll", "vkQueuePresentKHR") => t(
            vk_queue_present as *const () as usize,
            IatOriginal::Api(Api::VkQueuePresentKHR),
        ),
        ("vulkan-1.dll", "vkCreateSwapchainKHR") => t(
            vk_create_swapchain as *const () as usize,
            IatOriginal::Slot(&ORIG_VK_CREATE_SWAPCHAIN),
        ),
        ("vulkan-1.dll", "vkGetSwapchainImagesKHR") => t(
            vk_get_swapchain_images as *const () as usize,
            IatOriginal::Slot(&ORIG_VK_GET_IMAGES),
        ),
        ("vulkan-1.dll", "vkCreateDevice") => t(
            vk_create_device as *const () as usize,
            IatOriginal::Slot(&ORIG_VK_CREATE_DEVICE),
        ),
        ("vulkan-1.dll", "vkDestroyDevice") => t(
            vk_destroy_device as *const () as usize,
            IatOriginal::Slot(&ORIG_VK_DESTROY_DEVICE),
        ),
        ("vulkan-1.dll", "vkGetInstanceProcAddr") => t(
            vk_get_instance_proc_addr as *const () as usize,
            IatOriginal::Slot(&ORIG_VK_GET_INSTANCE_PROC),
        ),
        ("vulkan-1.dll", "vkGetDeviceProcAddr") => t(
            vk_get_device_proc_addr as *const () as usize,
            IatOriginal::Slot(&ORIG_VK_GET_DEVICE_PROC),
        ),
        _ => None,
    }
}

// -- vtable patch helpers ---------------------------------------------------

/// True when `obj` answers `QueryInterface` for interface `T`.
///
/// # Safety
/// `obj` must be a live COM interface pointer (or null).
unsafe fn qi_supports<T: windows::core::Interface>(obj: *mut c_void) -> bool {
    if obj.is_null() {
        return false;
    }
    let raw = obj;
    // SAFETY: `obj` is a live COM interface pointer per the caller's contract.
    match unsafe { T::from_raw_borrowed(&raw) } {
        // A real QueryInterface: only an object implementing `T` answers.
        Some(x) => x.cast::<T>().is_ok(),
        None => false,
    }
}

/// Install one vtable slot, remembering the original under `api`.
///
/// # Safety
/// `obj` must be a live COM interface and `index` a valid slot for its vtable.
unsafe fn patch_slot(obj: *mut c_void, index: usize, api: Api, detour: usize) {
    // SAFETY: caller guarantees `index` is within the object's vtable.
    if let Some(p) = unsafe { VtableSlotPatch::install(obj, index, detour) } {
        set_original(api, p.original());
        // SAFETY: `obj` is a live COM interface pointer (install succeeded, so
        // its vtable pointer is non-null).
        let vtable = unsafe { crate::patch::vtable_of(obj) };
        register_vtable_original(vtable as usize, index, p.original());
        keep_patch(p);
    }
}

/// Patch the shared swapchain vtable on `obj` (Present/Present1/SetFullscreen/
/// ResizeBuffers).
///
/// `Present1` (slot 22) belongs to `IDXGISwapChain1` and is patched only when
/// the object answers for that interface, so a shorter vtable is never written
/// past its end.
///
/// # Safety
/// `obj` must be a live `IDXGISwapChain*`.
pub unsafe fn patch_swapchain(obj: *mut c_void) {
    use windows::Win32::Graphics::Dxgi::IDXGISwapChain1;
    // SAFETY: `obj` is a live COM interface; these slots are on the base vtable.
    unsafe {
        patch_slot(obj, 8, Api::DxgiPresent, dxgi_present as *const () as usize);
        patch_slot(
            obj,
            10,
            Api::DxgiSetFullscreenState,
            dxgi_setfullscreen as *const () as usize,
        );
        patch_slot(
            obj,
            13,
            Api::DxgiResizeBuffers,
            dxgi_resizebuffers as *const () as usize,
        );
        if qi_supports::<IDXGISwapChain1>(obj) {
            patch_slot(
                obj,
                22,
                Api::DxgiPresent1,
                dxgi_present1 as *const () as usize,
            );
        }
    }
}

/// Patch an `IDXGIFactory*`/`IDXGIFactory2*` vtable (creation slots).
///
/// `CreateSwapChain` (slot 10) is on the base interface; the `IDXGIFactory2`
/// slots (15/16/24) are patched only when the object answers for it.
///
/// # Safety
/// `obj` must be a live `IDXGIFactory*`.
pub unsafe fn patch_factory(obj: *mut c_void) {
    use windows::Win32::Graphics::Dxgi::IDXGIFactory2;
    // SAFETY: `obj` is a live COM interface; slot 10 is on the base vtable.
    unsafe {
        patch_slot(
            obj,
            10,
            Api::DxgiCreateSwapChain,
            create_swap_chain as *const () as usize,
        );
        if qi_supports::<IDXGIFactory2>(obj) {
            patch_slot(
                obj,
                15,
                Api::DxgiCreateSwapChainForHwnd,
                create_swap_chain_for_hwnd as *const () as usize,
            );
            patch_slot(
                obj,
                16,
                Api::DxgiCreateSwapChainForCoreWindow,
                create_swap_chain_for_core_window as *const () as usize,
            );
            patch_slot(
                obj,
                24,
                Api::DxgiCreateSwapChainForComposition,
                create_swap_chain_for_composition as *const () as usize,
            );
        }
    }
}

/// Patch an `IDirect3DDevice9*`/`9Ex*` vtable (Present/EndScene/Ex).
///
/// `Present`/`EndScene` (17/42) are on the base interface; the `Ex` slots
/// (121/132) are patched only when the object answers for `IDirect3DDevice9Ex`.
///
/// # Safety
/// `obj` must be a live `IDirect3DDevice9*`.
pub unsafe fn patch_d3d9_device(obj: *mut c_void) {
    use windows::Win32::Graphics::Direct3D9::IDirect3DDevice9Ex;
    // SAFETY: `obj` is a live COM interface; 17/42 are on the base vtable.
    unsafe {
        patch_slot(
            obj,
            17,
            Api::D3d9Present,
            d3d9_present as *const () as usize,
        );
        patch_slot(
            obj,
            42,
            Api::D3d9EndScene,
            d3d9_endscene as *const () as usize,
        );
        if qi_supports::<IDirect3DDevice9Ex>(obj) {
            patch_slot(
                obj,
                121,
                Api::D3d9PresentEx,
                d3d9_present_ex as *const () as usize,
            );
            patch_slot(
                obj,
                132,
                Api::D3d9ResetEx,
                d3d9_reset_ex as *const () as usize,
            );
        }
    }
}

/// Patch an `IDirect3D9*`/`9Ex*` vtable (CreateDevice/Ex).
///
/// `CreateDevice` (slot 16) is on the base interface; `CreateDeviceEx` (slot
/// 20) is patched only when the object answers for `IDirect3D9Ex`.
///
/// # Safety
/// `obj` must be a live `IDirect3D9*`.
pub unsafe fn patch_d3d9(obj: *mut c_void) {
    use windows::Win32::Graphics::Direct3D9::IDirect3D9Ex;
    // SAFETY: `obj` is a live COM interface; slot 16 is on the base vtable.
    unsafe {
        patch_slot(
            obj,
            16,
            Api::D3d9CreateDevice,
            d3d9_create_device as *const () as usize,
        );
        if qi_supports::<IDirect3D9Ex>(obj) {
            patch_slot(
                obj,
                20,
                Api::D3d9CreateDeviceEx,
                d3d9_create_device_ex as *const () as usize,
            );
        }
    }
}

/// Patch an `ID3D12CommandQueue*` vtable (ExecuteCommandLists).
///
/// # Safety
/// `obj` must be a live `ID3D12CommandQueue*`.
pub unsafe fn patch_d3d12_queue(obj: *mut c_void) {
    // SAFETY: `obj` is a live COM interface; index 10 is ExecuteCommandLists.
    unsafe {
        patch_slot(
            obj,
            10,
            Api::D3d12ExecuteCommandLists,
            d3d12_execute_command_lists as *const () as usize,
        );
    }
}

// -- frame detours ----------------------------------------------------------

/// Calls the stored original (or a benign default) and records the frame.
///
/// `$slot` is `Some(index)` for a COM vtable hook (the original is recovered
/// from `$obj`'s own class vtable) and `None` for a free-function IAT hook
/// (where `$obj` is an opaque handle and only the shared per-`Api` original
/// applies).
macro_rules! frame_detour {
    ($name:ident, $api:expr, $slot:expr, $obj:expr, $ty:ty, ($($arg:ident : $t:ty),*), $handle:expr) => {
        /// Detour that records a frame and forwards to the original.
        pub extern "system" fn $name($($arg: $t),*) -> i32 {
            let start = qpc_now();
            let orig = resolve_original($obj, $slot, $api);
            let ret = if orig == 0 {
                0
            } else {
                // SAFETY: `orig` was stored as a pointer to this exact
                // signature when the slot was patched.
                let f: $ty = unsafe { core::mem::transmute(orig) };
                // SAFETY: forwarding the caller's own arguments unchanged.
                unsafe { f($($arg),*) }
            };
            let stop = qpc_now();
            record_frame($api, start, stop, $handle, 0);
            ret
        }
    };
}

type PresentFn = unsafe extern "system" fn(*mut c_void, u32, u32) -> i32;
type Present1Fn = unsafe extern "system" fn(*mut c_void, u32, u32, *const c_void) -> i32;
type SetFullscreenFn = unsafe extern "system" fn(*mut c_void, i32, *mut c_void) -> i32;
type ResizeBuffersFn = unsafe extern "system" fn(*mut c_void, u32, u32, u32, u32, u32) -> i32;
type D3d9PresentFn = unsafe extern "system" fn(
    *mut c_void,
    *const c_void,
    *const c_void,
    *mut c_void,
    *const c_void,
) -> i32;
type D3d9PresentExFn = unsafe extern "system" fn(
    *mut c_void,
    *const c_void,
    *const c_void,
    *mut c_void,
    *const c_void,
    u32,
) -> i32;
type EndSceneFn = unsafe extern "system" fn(*mut c_void) -> i32;
type ResetExFn = unsafe extern "system" fn(*mut c_void, *mut c_void, *mut c_void) -> i32;
type ExecuteCommandListsFn = unsafe extern "system" fn(*mut c_void, u32, *const *const c_void);
type WglSwapFn = unsafe extern "system" fn(*mut c_void) -> i32;
type WglSwapLayerFn = unsafe extern "system" fn(*mut c_void, u32) -> i32;
type EglSwapFn = unsafe extern "system" fn(*mut c_void, *mut c_void) -> i32;
type VkQueuePresentFn = unsafe extern "system" fn(*mut c_void, *const c_void) -> i32;

frame_detour!(
    dxgi_present,
    Api::DxgiPresent,
    Some(8),
    this,
    PresentFn,
    (this: *mut c_void, sync: u32, flags: u32),
    this as u64
);
frame_detour!(
    dxgi_present1,
    Api::DxgiPresent1,
    Some(22),
    this,
    Present1Fn,
    (this: *mut c_void, sync: u32, flags: u32, params: *const c_void),
    this as u64
);
frame_detour!(
    dxgi_setfullscreen,
    Api::DxgiSetFullscreenState,
    Some(10),
    this,
    SetFullscreenFn,
    (this: *mut c_void, fullscreen: i32, target: *mut c_void),
    this as u64
);
frame_detour!(
    dxgi_resizebuffers,
    Api::DxgiResizeBuffers,
    Some(13),
    this,
    ResizeBuffersFn,
    (this: *mut c_void, count: u32, w: u32, h: u32, fmt: u32, flags: u32),
    this as u64
);
frame_detour!(
    d3d9_present,
    Api::D3d9Present,
    Some(17),
    this,
    D3d9PresentFn,
    (this: *mut c_void, src: *const c_void, dst: *const c_void, hwnd: *mut c_void, dirty: *const c_void),
    this as u64
);
frame_detour!(
    d3d9_endscene,
    Api::D3d9EndScene,
    Some(42),
    this,
    EndSceneFn,
    (this: *mut c_void),
    this as u64
);
frame_detour!(
    d3d9_present_ex,
    Api::D3d9PresentEx,
    Some(121),
    this,
    D3d9PresentExFn,
    (this: *mut c_void, src: *const c_void, dst: *const c_void, hwnd: *mut c_void, dirty: *const c_void, flags: u32),
    this as u64
);
frame_detour!(
    d3d9_reset_ex,
    Api::D3d9ResetEx,
    Some(132),
    this,
    ResetExFn,
    (this: *mut c_void, pp: *mut c_void, mode: *mut c_void),
    this as u64
);
frame_detour!(
    wgl_swap_buffers,
    Api::WglSwapBuffers,
    None,
    hdc,
    WglSwapFn,
    (hdc: *mut c_void),
    hdc as u64
);
frame_detour!(
    wgl_swap_layer_buffers,
    Api::WglSwapLayerBuffers,
    None,
    hdc,
    WglSwapLayerFn,
    (hdc: *mut c_void, planes: u32),
    hdc as u64
);
frame_detour!(
    gdi_swap_buffers,
    Api::GdiSwapBuffers,
    None,
    hdc,
    WglSwapFn,
    (hdc: *mut c_void),
    hdc as u64
);
frame_detour!(
    egl_swap_buffers,
    Api::EglSwapBuffers,
    None,
    dpy,
    EglSwapFn,
    (dpy: *mut c_void, surface: *mut c_void),
    surface as u64
);
frame_detour!(
    vk_queue_present,
    Api::VkQueuePresentKHR,
    None,
    queue,
    VkQueuePresentFn,
    (queue: *mut c_void, info: *const c_void),
    queue as u64
);

/// `ID3D12CommandQueue::ExecuteCommandLists` (returns void).
pub extern "system" fn d3d12_execute_command_lists(
    this: *mut c_void,
    num: u32,
    lists: *const *const c_void,
) {
    let start = qpc_now();
    let orig = resolve_original(this, Some(10), Api::D3d12ExecuteCommandLists);
    if orig != 0 {
        // SAFETY: `orig` is a stored `ExecuteCommandLists` pointer.
        let f: ExecuteCommandListsFn = unsafe { core::mem::transmute(orig) };
        // SAFETY: forwarding the caller's own arguments unchanged.
        unsafe { f(this, num, lists) };
    }
    let stop = qpc_now();
    record_frame(Api::D3d12ExecuteCommandLists, start, stop, this as u64, 0);
}

// -- acquisition detours ----------------------------------------------------

type CreateSwapChainFn =
    unsafe extern "system" fn(*mut c_void, *mut c_void, *const c_void, *mut *mut c_void) -> i32;
type CreateSwapChainForHwndFn = unsafe extern "system" fn(
    *mut c_void,
    *mut c_void,
    *mut c_void,
    *const c_void,
    *const c_void,
    *mut c_void,
    *mut *mut c_void,
) -> i32;
type CreateSwapChainForCoreWindowFn = unsafe extern "system" fn(
    *mut c_void,
    *mut c_void,
    *mut c_void,
    *const c_void,
    *mut c_void,
    *mut *mut c_void,
) -> i32;
type CreateSwapChainForCompositionFn = unsafe extern "system" fn(
    *mut c_void,
    *mut c_void,
    *const c_void,
    *mut c_void,
    *mut *mut c_void,
) -> i32;
type D3d9CreateDeviceFn = unsafe extern "system" fn(
    *mut c_void,
    u32,
    u32,
    *mut c_void,
    u32,
    *mut c_void,
    *mut *mut c_void,
) -> i32;
type D3d9CreateDeviceExFn = unsafe extern "system" fn(
    *mut c_void,
    u32,
    u32,
    *mut c_void,
    u32,
    *mut c_void,
    *mut c_void,
    *mut *mut c_void,
) -> i32;

/// After a swapchain is created, patch its (shared) vtable.
///
/// # Safety
/// `out` must be the out-pointer the creator filled.
unsafe fn patch_created_swapchain(out: *mut *mut c_void) {
    if out.is_null() {
        return;
    }
    // SAFETY: `out` is the creator's out-pointer; on success it holds a live
    // IDXGISwapChain*.
    let sc = unsafe { *out };
    if !sc.is_null() {
        // SAFETY: `sc` is a live IDXGISwapChain*.
        unsafe { patch_swapchain(sc) };
    }
}

/// If `device` is a D3D12 command queue, patch its ExecuteCommandLists slot.
///
/// The `pDevice` argument of a swapchain-creation call is an
/// `ID3D12CommandQueue` for D3D12 but an `ID3D11Device` for D3D11, so a
/// `QueryInterface` for `ID3D12CommandQueue` decides which one this is.
///
/// # Safety
/// `device` is the `pDevice` argument of a live creation call.
pub unsafe fn maybe_patch_d3d12_queue(device: *mut c_void) {
    use windows::core::Interface;
    use windows::Win32::Graphics::Direct3D12::ID3D12CommandQueue;
    if device.is_null() {
        return;
    }
    let raw = device;
    // SAFETY: `device` is a live COM interface pointer.
    let Some(candidate) = (unsafe { ID3D12CommandQueue::from_raw_borrowed(&raw) }) else {
        return;
    };
    // A real QueryInterface: only a queue answers for IID_ID3D12CommandQueue.
    if candidate.cast::<ID3D12CommandQueue>().is_ok() {
        // SAFETY: the object is a live ID3D12CommandQueue.
        unsafe { patch_d3d12_queue(device) };
    }
}

/// `IDXGIFactory::CreateSwapChain` detour.
pub extern "system" fn create_swap_chain(
    this: *mut c_void,
    device: *mut c_void,
    desc: *const c_void,
    out: *mut *mut c_void,
) -> i32 {
    let orig = resolve_original(this, Some(10), Api::DxgiCreateSwapChain);
    let hr = if orig == 0 {
        0
    } else {
        // SAFETY: `orig` is a stored `CreateSwapChain` pointer.
        let f: CreateSwapChainFn = unsafe { core::mem::transmute(orig) };
        // SAFETY: forwarding the caller's own arguments unchanged.
        unsafe { f(this, device, desc, out) }
    };
    if hr >= 0 {
        // SAFETY: `out` is this call's out-pointer.
        unsafe { patch_created_swapchain(out) };
        // SAFETY: `device` is this call's pDevice argument.
        unsafe { maybe_patch_d3d12_queue(device) };
    }
    hr
}

/// `IDXGIFactory2::CreateSwapChainForHwnd` detour.
pub extern "system" fn create_swap_chain_for_hwnd(
    this: *mut c_void,
    device: *mut c_void,
    hwnd: *mut c_void,
    desc: *const c_void,
    fsdesc: *const c_void,
    restrict: *mut c_void,
    out: *mut *mut c_void,
) -> i32 {
    let orig = resolve_original(this, Some(15), Api::DxgiCreateSwapChainForHwnd);
    let hr = if orig == 0 {
        0
    } else {
        // SAFETY: `orig` is a stored `CreateSwapChainForHwnd` pointer.
        let f: CreateSwapChainForHwndFn = unsafe { core::mem::transmute(orig) };
        // SAFETY: forwarding the caller's own arguments unchanged.
        unsafe { f(this, device, hwnd, desc, fsdesc, restrict, out) }
    };
    if hr >= 0 {
        // SAFETY: `out` is this call's out-pointer.
        unsafe { patch_created_swapchain(out) };
        // SAFETY: `device` is this call's pDevice argument.
        unsafe { maybe_patch_d3d12_queue(device) };
    }
    hr
}

/// `IDXGIFactory2::CreateSwapChainForCoreWindow` detour.
pub extern "system" fn create_swap_chain_for_core_window(
    this: *mut c_void,
    device: *mut c_void,
    window: *mut c_void,
    desc: *const c_void,
    restrict: *mut c_void,
    out: *mut *mut c_void,
) -> i32 {
    let orig = resolve_original(this, Some(16), Api::DxgiCreateSwapChainForCoreWindow);
    let hr = if orig == 0 {
        0
    } else {
        // SAFETY: `orig` is a stored `CreateSwapChainForCoreWindow` pointer.
        let f: CreateSwapChainForCoreWindowFn = unsafe { core::mem::transmute(orig) };
        // SAFETY: forwarding the caller's own arguments unchanged.
        unsafe { f(this, device, window, desc, restrict, out) }
    };
    if hr >= 0 {
        // SAFETY: `out` is this call's out-pointer.
        unsafe { patch_created_swapchain(out) };
        // SAFETY: `device` is this call's pDevice argument.
        unsafe { maybe_patch_d3d12_queue(device) };
    }
    hr
}

/// `IDXGIFactory2::CreateSwapChainForComposition` detour.
pub extern "system" fn create_swap_chain_for_composition(
    this: *mut c_void,
    device: *mut c_void,
    desc: *const c_void,
    restrict: *mut c_void,
    out: *mut *mut c_void,
) -> i32 {
    let orig = resolve_original(this, Some(24), Api::DxgiCreateSwapChainForComposition);
    let hr = if orig == 0 {
        0
    } else {
        // SAFETY: `orig` is a stored `CreateSwapChainForComposition` pointer.
        let f: CreateSwapChainForCompositionFn = unsafe { core::mem::transmute(orig) };
        // SAFETY: forwarding the caller's own arguments unchanged.
        unsafe { f(this, device, desc, restrict, out) }
    };
    if hr >= 0 {
        // SAFETY: `out` is this call's out-pointer.
        unsafe { patch_created_swapchain(out) };
        // SAFETY: `device` is this call's pDevice argument.
        unsafe { maybe_patch_d3d12_queue(device) };
    }
    hr
}

/// `IDirect3D9::CreateDevice` detour.
pub extern "system" fn d3d9_create_device(
    this: *mut c_void,
    adapter: u32,
    dtype: u32,
    hwnd: *mut c_void,
    flags: u32,
    pp: *mut c_void,
    out: *mut *mut c_void,
) -> i32 {
    let orig = resolve_original(this, Some(16), Api::D3d9CreateDevice);
    let hr = if orig == 0 {
        0
    } else {
        // SAFETY: `orig` is a stored `CreateDevice` pointer.
        let f: D3d9CreateDeviceFn = unsafe { core::mem::transmute(orig) };
        // SAFETY: forwarding the caller's own arguments unchanged.
        unsafe { f(this, adapter, dtype, hwnd, flags, pp, out) }
    };
    if hr >= 0 && !out.is_null() {
        // SAFETY: `out` is the creator's out-pointer.
        let dev = unsafe { *out };
        if !dev.is_null() {
            // SAFETY: `dev` is a live IDirect3DDevice9*.
            unsafe { patch_d3d9_device(dev) };
        }
    }
    hr
}

/// `IDirect3D9Ex::CreateDeviceEx` detour.
pub extern "system" fn d3d9_create_device_ex(
    this: *mut c_void,
    adapter: u32,
    dtype: u32,
    hwnd: *mut c_void,
    flags: u32,
    pp: *mut c_void,
    mode: *mut c_void,
    out: *mut *mut c_void,
) -> i32 {
    let orig = resolve_original(this, Some(20), Api::D3d9CreateDeviceEx);
    let hr = if orig == 0 {
        0
    } else {
        // SAFETY: `orig` is a stored `CreateDeviceEx` pointer.
        let f: D3d9CreateDeviceExFn = unsafe { core::mem::transmute(orig) };
        // SAFETY: forwarding the caller's own arguments unchanged.
        unsafe { f(this, adapter, dtype, hwnd, flags, pp, mode, out) }
    };
    if hr >= 0 && !out.is_null() {
        // SAFETY: `out` is the creator's out-pointer.
        let dev = unsafe { *out };
        if !dev.is_null() {
            // SAFETY: `dev` is a live IDirect3DDevice9Ex*.
            unsafe { patch_d3d9_device(dev) };
        }
    }
    hr
}

// -- free-function acquisition detours --------------------------------------

type CreateDxgiFactoryFn = unsafe extern "system" fn(*const c_void, *mut *mut c_void) -> i32;
type CreateDxgiFactory2Fn = unsafe extern "system" fn(u32, *const c_void, *mut *mut c_void) -> i32;
type D3d11CreateDeviceAndSwapChainFn = unsafe extern "system" fn(
    *mut c_void,
    u32,
    *mut c_void,
    u32,
    *const c_void,
    u32,
    u32,
    *const c_void,
    *mut *mut c_void,
    *mut *mut c_void,
    *mut c_void,
    *mut *mut c_void,
) -> i32;
type Direct3DCreate9Fn = unsafe extern "system" fn(u32) -> *mut c_void;
type Direct3DCreate9ExFn = unsafe extern "system" fn(u32, *mut *mut c_void) -> i32;

/// `CreateDXGIFactory` detour: patch the returned factory's vtable.
pub extern "system" fn create_dxgi_factory(riid: *const c_void, out: *mut *mut c_void) -> i32 {
    let orig = ORIG_FACTORY0.load(Ordering::Acquire);
    let hr = if orig == 0 {
        0
    } else {
        // SAFETY: `orig` is the saved `CreateDXGIFactory` pointer.
        let f: CreateDxgiFactoryFn = unsafe { core::mem::transmute(orig) };
        // SAFETY: forwarding the caller's own arguments unchanged.
        unsafe { f(riid, out) }
    };
    if hr >= 0 && !out.is_null() {
        // SAFETY: `out` holds a live IDXGIFactory*.
        let factory = unsafe { *out };
        if !factory.is_null() {
            // SAFETY: `factory` is a live IDXGIFactory*.
            unsafe { patch_factory(factory) };
        }
    }
    hr
}

/// `CreateDXGIFactory1` detour.
pub extern "system" fn create_dxgi_factory1(riid: *const c_void, out: *mut *mut c_void) -> i32 {
    let orig = ORIG_FACTORY1.load(Ordering::Acquire);
    let hr = if orig == 0 {
        0
    } else {
        // SAFETY: `orig` is the saved `CreateDXGIFactory1` pointer.
        let f: CreateDxgiFactoryFn = unsafe { core::mem::transmute(orig) };
        // SAFETY: forwarding the caller's own arguments unchanged.
        unsafe { f(riid, out) }
    };
    if hr >= 0 && !out.is_null() {
        // SAFETY: `out` holds a live IDXGIFactory1*.
        let factory = unsafe { *out };
        if !factory.is_null() {
            // SAFETY: `factory` is a live IDXGIFactory*.
            unsafe { patch_factory(factory) };
        }
    }
    hr
}

/// `CreateDXGIFactory2` detour.
pub extern "system" fn create_dxgi_factory2(
    flags: u32,
    riid: *const c_void,
    out: *mut *mut c_void,
) -> i32 {
    let orig = ORIG_FACTORY2.load(Ordering::Acquire);
    let hr = if orig == 0 {
        0
    } else {
        // SAFETY: `orig` is the saved `CreateDXGIFactory2` pointer.
        let f: CreateDxgiFactory2Fn = unsafe { core::mem::transmute(orig) };
        // SAFETY: forwarding the caller's own arguments unchanged.
        unsafe { f(flags, riid, out) }
    };
    if hr >= 0 && !out.is_null() {
        // SAFETY: `out` holds a live IDXGIFactory2*.
        let factory = unsafe { *out };
        if !factory.is_null() {
            // SAFETY: `factory` is a live IDXGIFactory*.
            unsafe { patch_factory(factory) };
        }
    }
    hr
}

/// `D3D11CreateDeviceAndSwapChain` detour.
pub extern "system" fn d3d11_create_device_and_swap_chain(
    adapter: *mut c_void,
    driver_type: u32,
    software: *mut c_void,
    flags: u32,
    feature_levels: *const c_void,
    num_levels: u32,
    sdk_version: u32,
    sc_desc: *const c_void,
    pp_swapchain: *mut *mut c_void,
    pp_device: *mut *mut c_void,
    p_feature_level: *mut c_void,
    pp_context: *mut *mut c_void,
) -> i32 {
    let orig = ORIG_D3D11_CREATE.load(Ordering::Acquire);
    let hr = if orig == 0 {
        0
    } else {
        // SAFETY: `orig` is the saved `D3D11CreateDeviceAndSwapChain` pointer.
        let f: D3d11CreateDeviceAndSwapChainFn = unsafe { core::mem::transmute(orig) };
        // SAFETY: forwarding the caller's own arguments unchanged.
        unsafe {
            f(
                adapter,
                driver_type,
                software,
                flags,
                feature_levels,
                num_levels,
                sdk_version,
                sc_desc,
                pp_swapchain,
                pp_device,
                p_feature_level,
                pp_context,
            )
        }
    };
    if hr >= 0 {
        // SAFETY: `pp_swapchain` is this call's out-pointer.
        unsafe { patch_created_swapchain(pp_swapchain) };
    }
    hr
}

/// `Direct3DCreate9` detour: patch the returned IDirect3D9's CreateDevice slot.
pub extern "system" fn direct3d_create9(sdk_version: u32) -> *mut c_void {
    let orig = ORIG_D3D9_CREATE.load(Ordering::Acquire);
    if orig == 0 {
        return core::ptr::null_mut();
    }
    // SAFETY: `orig` is the saved `Direct3DCreate9` pointer.
    let f: Direct3DCreate9Fn = unsafe { core::mem::transmute(orig) };
    // SAFETY: forwarding the caller's own argument unchanged.
    let d3d = unsafe { f(sdk_version) };
    if !d3d.is_null() {
        // SAFETY: `d3d` is a live IDirect3D9*.
        unsafe { patch_d3d9(d3d) };
    }
    d3d
}

/// `Direct3DCreate9Ex` detour.
pub extern "system" fn direct3d_create9_ex(sdk_version: u32, out: *mut *mut c_void) -> i32 {
    let orig = ORIG_D3D9_CREATE_EX.load(Ordering::Acquire);
    let hr = if orig == 0 {
        0
    } else {
        // SAFETY: `orig` is the saved `Direct3DCreate9Ex` pointer.
        let f: Direct3DCreate9ExFn = unsafe { core::mem::transmute(orig) };
        // SAFETY: forwarding the caller's own arguments unchanged.
        unsafe { f(sdk_version, out) }
    };
    if hr >= 0 && !out.is_null() {
        // SAFETY: `out` holds a live IDirect3D9Ex*.
        let d3d = unsafe { *out };
        if !d3d.is_null() {
            // SAFETY: `d3d` is a live IDirect3D9Ex*.
            unsafe { patch_d3d9(d3d) };
        }
    }
    hr
}

// -- Vulkan acquisition (forward-only) --------------------------------------

type VkCreateSwapchainFn =
    unsafe extern "system" fn(*mut c_void, *const c_void, *const c_void, *mut *mut c_void) -> i32;
type VkGetSwapchainImagesFn =
    unsafe extern "system" fn(*mut c_void, *mut c_void, *mut u32, *mut *mut c_void) -> i32;
type VkCreateDeviceFn =
    unsafe extern "system" fn(*mut c_void, *const c_void, *const c_void, *mut *mut c_void) -> i32;
type VkDestroyDeviceFn = unsafe extern "system" fn(*mut c_void, *const c_void);

/// `vkCreateSwapchainKHR` detour (forward-only; present is hooked separately).
pub extern "system" fn vk_create_swapchain(
    device: *mut c_void,
    create_info: *const c_void,
    allocator: *const c_void,
    out: *mut *mut c_void,
) -> i32 {
    let orig = ORIG_VK_CREATE_SWAPCHAIN.load(Ordering::Acquire);
    if orig == 0 {
        return 0;
    }
    // SAFETY: `orig` is the saved `vkCreateSwapchainKHR` pointer.
    let f: VkCreateSwapchainFn = unsafe { core::mem::transmute(orig) };
    // SAFETY: forwarding the caller's own arguments unchanged.
    unsafe { f(device, create_info, allocator, out) }
}

/// `vkGetSwapchainImagesKHR` detour (forward-only).
pub extern "system" fn vk_get_swapchain_images(
    device: *mut c_void,
    swapchain: *mut c_void,
    count: *mut u32,
    images: *mut *mut c_void,
) -> i32 {
    let orig = ORIG_VK_GET_IMAGES.load(Ordering::Acquire);
    if orig == 0 {
        return 0;
    }
    // SAFETY: `orig` is the saved `vkGetSwapchainImagesKHR` pointer.
    let f: VkGetSwapchainImagesFn = unsafe { core::mem::transmute(orig) };
    // SAFETY: forwarding the caller's own arguments unchanged.
    unsafe { f(device, swapchain, count, images) }
}

/// `vkCreateDevice` detour (forward-only).
pub extern "system" fn vk_create_device(
    physical: *mut c_void,
    create_info: *const c_void,
    allocator: *const c_void,
    out: *mut *mut c_void,
) -> i32 {
    let orig = ORIG_VK_CREATE_DEVICE.load(Ordering::Acquire);
    if orig == 0 {
        return 0;
    }
    // SAFETY: `orig` is the saved `vkCreateDevice` pointer.
    let f: VkCreateDeviceFn = unsafe { core::mem::transmute(orig) };
    // SAFETY: forwarding the caller's own arguments unchanged.
    unsafe { f(physical, create_info, allocator, out) }
}

/// `vkDestroyDevice` detour (forward-only).
pub extern "system" fn vk_destroy_device(device: *mut c_void, allocator: *const c_void) {
    let orig = ORIG_VK_DESTROY_DEVICE.load(Ordering::Acquire);
    if orig == 0 {
        return;
    }
    // SAFETY: `orig` is the saved `vkDestroyDevice` pointer.
    let f: VkDestroyDeviceFn = unsafe { core::mem::transmute(orig) };
    // SAFETY: forwarding the caller's own arguments unchanged.
    unsafe { f(device, allocator) };
}

// -- Vulkan loader proc-addr chain ------------------------------------------

/// `vkGetInstanceProcAddr` / `vkGetDeviceProcAddr` share this signature: they
/// take an opaque handle (instance/device, or null) and a name, and return the
/// resolved entry point.
type VkProcAddrFn = unsafe extern "system" fn(*mut c_void, *const c_char) -> *const c_void;

/// The detour address substituted for a resolved target name.
fn vk_detour(d: Detour) -> usize {
    match d {
        Detour::QueuePresent => vk_queue_present as *const () as usize,
        Detour::CreateSwapchain => vk_create_swapchain as *const () as usize,
        Detour::GetSwapchainImages => vk_get_swapchain_images as *const () as usize,
        Detour::CreateDevice => vk_create_device as *const () as usize,
        Detour::DestroyDevice => vk_destroy_device as *const () as usize,
        Detour::GetDeviceProcAddr => vk_get_device_proc_addr as *const () as usize,
    }
}

/// Remember the real entry point the loader returned, so the substituted
/// detour can forward to it.
fn vk_store_original(d: Detour, real: usize) {
    match d {
        Detour::QueuePresent => set_original(Api::VkQueuePresentKHR, real),
        Detour::CreateSwapchain => ORIG_VK_CREATE_SWAPCHAIN.store(real, Ordering::Release),
        Detour::GetSwapchainImages => ORIG_VK_GET_IMAGES.store(real, Ordering::Release),
        Detour::CreateDevice => ORIG_VK_CREATE_DEVICE.store(real, Ordering::Release),
        Detour::DestroyDevice => ORIG_VK_DESTROY_DEVICE.store(real, Ordering::Release),
        Detour::GetDeviceProcAddr => ORIG_VK_GET_DEVICE_PROC.store(real, Ordering::Release),
    }
}

/// Read a requested Vulkan name, or `None` when null / not valid UTF-8
/// (fail-open: the caller forwards the real pointer).
///
/// # Safety
/// `name` must be null or point to a NUL-terminated C string.
unsafe fn vk_name<'a>(name: *const c_char) -> Option<&'a str> {
    if name.is_null() {
        return None;
    }
    // SAFETY: the caller guarantees a NUL-terminated C string.
    unsafe { CStr::from_ptr(name) }.to_str().ok()
}

/// Shared proc-addr body: call the real resolver, then substitute our detour
/// for a target name (recording the real pointer first) and forward the real
/// pointer for everything else.
///
/// `allow_device_proc_addr` is true for `vkGetInstanceProcAddr` (which is how
/// an ash-style loader fetches `vkGetDeviceProcAddr`) and false for
/// `vkGetDeviceProcAddr` itself, where re-entering the chain would be wrong.
///
/// # Safety
/// `handle` and `name` are the caller's own arguments; `orig_slot` holds the
/// real proc-addr (0 when not installed).
unsafe fn vk_proc_addr(
    orig_slot: &AtomicUsize,
    handle: *mut c_void,
    name: *const c_char,
    allow_device_proc_addr: bool,
) -> *const c_void {
    let orig = orig_slot.load(Ordering::Acquire);
    if orig == 0 {
        return core::ptr::null();
    }
    // SAFETY: `orig` was stored as a proc-addr of this exact signature.
    let f: VkProcAddrFn = unsafe { core::mem::transmute(orig) };
    // SAFETY: forwarding the caller's own arguments unchanged.
    let real = unsafe { f(handle, name) };
    if real.is_null() {
        // The function does not exist here; hand back null, never a detour.
        return real;
    }
    // SAFETY: `name` is the caller's NUL-terminated request string.
    let Some(text) = (unsafe { vk_name(name) }) else {
        return real;
    };
    match vk_target(text) {
        Some(Detour::GetDeviceProcAddr) if !allow_device_proc_addr => real,
        Some(d) => {
            vk_store_original(d, real as usize);
            vk_detour(d) as *const c_void
        }
        None => real,
    }
}

/// `vulkan-1.dll!vkGetInstanceProcAddr` detour: keep the loader chain inside
/// the recorder by returning our detours for target names.
pub extern "system" fn vk_get_instance_proc_addr(
    instance: *mut c_void,
    name: *const c_char,
) -> *const c_void {
    // SAFETY: `instance`/`name` are the loader's own arguments.
    unsafe { vk_proc_addr(&ORIG_VK_GET_INSTANCE_PROC, instance, name, true) }
}

/// `vulkan-1.dll!vkGetDeviceProcAddr` detour.
pub extern "system" fn vk_get_device_proc_addr(
    device: *mut c_void,
    name: *const c_char,
) -> *const c_void {
    // SAFETY: `device`/`name` are the loader's own arguments.
    unsafe { vk_proc_addr(&ORIG_VK_GET_DEVICE_PROC, device, name, false) }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::api::{HookKind, IAT_HOOKS};
    use std::sync::Mutex;

    /// The registry, `ORIGINALS`, and the `ORIG_*` slots are process-global; the
    /// tests share one address space, so every test that reads or writes them
    /// takes this lock (otherwise a test holding the registry would make another
    /// fall back and flake).
    static GLOBAL_LOCK: Mutex<()> = Mutex::new(());

    fn serial() -> std::sync::MutexGuard<'static, ()> {
        GLOBAL_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    #[test]
    fn iat_target_resolves_every_hooked_import_to_a_nonnull_detour() {
        // Catches: an IAT_HOOKS entry with no matching dispatch arm — an
        // advertised hook (documented in HOOKING.md) that is silently never
        // installed.
        for spec in IAT_HOOKS {
            let HookKind::Iat { dll, func } = spec.kind else {
                panic!("IAT_HOOKS must only contain Iat specs");
            };
            let t =
                iat_target(dll, func).unwrap_or_else(|| panic!("no iat_target for {dll}!{func}"));
            assert_ne!(t.detour, 0, "detour for {dll}!{func} must not be null");
        }
    }

    #[test]
    fn iat_target_matches_the_dll_name_case_insensitively_and_rejects_others() {
        // Catches: a case-sensitive match silently failing for a module that
        // reports an uppercase name (the import reader preserves case), and a
        // missing rejection for functions we do not hook.
        assert!(iat_target("DXGI.DLL", "CreateDXGIFactory1").is_some());
        assert!(iat_target("dxgi.dll", "CreateDXGIFactory1").is_some());
        assert!(iat_target("opengl32.dll", "wglSwapBuffers").is_some());
        assert!(iat_target("dxgi.dll", "NotAFunction").is_none());
        assert!(iat_target("other.dll", "CreateDXGIFactory").is_none());
        assert!(
            iat_target("dxgi.dll", "createdxgifactory").is_none(),
            "func is case-sensitive"
        );
    }

    #[test]
    fn iat_target_gives_each_free_function_its_own_original_slot() {
        // Catches: two acquisition detours sharing one original slot, so the
        // second patch overwrites the first's saved original and the first
        // detour forwards to the wrong function.
        let slot = |dll: &str, func: &str| match iat_target(dll, func).expect("hooked").original {
            IatOriginal::Slot(s) => s as *const _ as usize,
            IatOriginal::Api(_) => usize::MAX,
        };
        let a = slot("dxgi.dll", "CreateDXGIFactory");
        let b = slot("dxgi.dll", "CreateDXGIFactory1");
        let c = slot("dxgi.dll", "CreateDXGIFactory2");
        let d = slot("vulkan-1.dll", "vkCreateSwapchainKHR");
        assert_ne!(a, b, "factory0 and factory1 need distinct originals");
        assert_ne!(b, c, "factory1 and factory2 need distinct originals");
        assert_ne!(a, d, "dxgi and vulkan originals must not collide");
        assert_ne!(a, usize::MAX);
        assert_ne!(d, usize::MAX, "vkCreateSwapchainKHR keeps a dedicated slot");
    }

    #[test]
    fn two_distinct_vtables_for_one_api_each_forward_to_their_own_original() {
        // Catches: a single per-Api original slot. Patching a second, distinct
        // (non-shared) vtable for the same API overwrites the first original, so
        // the first object's detour forwards to the second object's function.
        let _g = serial();
        extern "system" fn original_a(_: *mut c_void, _: u32, _: u32) -> i32 {
            111
        }
        extern "system" fn original_b(_: *mut c_void, _: u32, _: u32) -> i32 {
            222
        }

        // Two distinct class vtables, each long enough for slot 8. Leaked so the
        // process-wide patch list can restore them safely even after this test.
        let vt_a: &'static mut [usize; 9] = Box::leak(Box::new([0usize; 9]));
        let vt_b: &'static mut [usize; 9] = Box::leak(Box::new([0usize; 9]));
        vt_a[8] = original_a as *const () as usize;
        vt_b[8] = original_b as *const () as usize;
        // A COM object's first word is its vtable pointer.
        let obj_a_word: &'static mut usize = Box::leak(Box::new(vt_a.as_mut_ptr() as usize));
        let obj_b_word: &'static mut usize = Box::leak(Box::new(vt_b.as_mut_ptr() as usize));
        let obj_a = obj_a_word as *mut usize as *mut c_void;
        let obj_b = obj_b_word as *mut usize as *mut c_void;

        // SAFETY: both objects point at leaked vtables of length >= 9.
        unsafe {
            patch_slot(
                obj_a,
                8,
                Api::DxgiPresent,
                dxgi_present as *const () as usize,
            );
            patch_slot(
                obj_b,
                8,
                Api::DxgiPresent,
                dxgi_present as *const () as usize,
            );
        }

        assert_eq!(
            dxgi_present(obj_a, 0, 0),
            111,
            "object A must forward to its own original, not B's"
        );
        assert_eq!(
            dxgi_present(obj_b, 0, 0),
            222,
            "object B must forward to its own original, not A's"
        );
    }

    #[test]
    fn registry_insert_replaces_a_duplicate_key_and_refuses_when_full() {
        // Catches: a shared class vtable reached twice consuming two registry
        // slots (exhausting the fixed capacity), and a full registry silently
        // overwriting a live entry or panicking instead of refusing.
        let mut reg = [None; 2];
        assert!(registry_insert(&mut reg, 0xAA, 8, 1), "first insert fits");
        assert!(
            registry_insert(&mut reg, 0xAA, 8, 2),
            "the same key updates in place"
        );
        assert_eq!(registry_lookup(&reg, 0xAA, 8), Some(2), "last write wins");
        // The duplicate did not consume a second slot: a distinct key still fits.
        assert!(
            registry_insert(&mut reg, 0xBB, 8, 3),
            "a distinct key still fits"
        );
        assert!(
            !registry_insert(&mut reg, 0xCC, 8, 4),
            "a full registry must refuse, not overwrite"
        );
        assert_eq!(
            registry_lookup(&reg, 0xCC, 8),
            None,
            "refused key not stored"
        );
    }

    #[test]
    fn registry_lookup_is_keyed_by_both_vtable_and_slot() {
        // Catches: a lookup that matches on the vtable alone (or the slot alone),
        // so a different method on the same object — or the same slot on a
        // different class — returns the wrong original.
        let mut reg = [None; 4];
        assert!(registry_insert(&mut reg, 0xAA, 8, 111));
        assert!(registry_insert(&mut reg, 0xAA, 10, 222));
        assert!(registry_insert(&mut reg, 0xBB, 8, 333));
        assert_eq!(registry_lookup(&reg, 0xAA, 8), Some(111));
        assert_eq!(registry_lookup(&reg, 0xAA, 10), Some(222));
        assert_eq!(registry_lookup(&reg, 0xBB, 8), Some(333));
        assert_eq!(registry_lookup(&reg, 0xAA, 9), None, "unregistered slot");
        assert_eq!(registry_lookup(&reg, 0xCC, 8), None, "unregistered vtable");
    }

    #[test]
    fn resolve_original_uses_the_shared_per_api_original_for_a_free_function() {
        // Catches: a free-function (IAT) detour treating its opaque handle
        // (`hdc`, `queue`, ...) as a COM object and dereferencing it — a segfault
        // or garbage read. With `slot=None` the object must never be touched.
        let _g = serial();
        set_original(Api::WglSwapLayerBuffers, 0xBEEF);
        // Non-null but deliberately not a readable address.
        let bogus = 0x10usize as *mut c_void;
        assert_eq!(
            resolve_original(bogus, None, Api::WglSwapLayerBuffers),
            0xBEEF
        );
        set_original(Api::WglSwapLayerBuffers, 0);
    }

    #[test]
    fn resolve_original_falls_back_for_a_null_object() {
        // Catches: a null-deref when a COM detour is entered with a null `this`;
        // it must fail open to the per-Api original, never read address 0.
        let _g = serial();
        set_original(Api::GdiSwapBuffers, 0x5150);
        assert_eq!(
            resolve_original(core::ptr::null_mut(), Some(8), Api::GdiSwapBuffers),
            0x5150
        );
        set_original(Api::GdiSwapBuffers, 0);
    }

    #[test]
    fn resolve_original_prefers_the_objects_own_class_vtable_original() {
        // Catches: the registry being populated but never consulted — the exact
        // regression the per-vtable originals exist to prevent.
        let _g = serial();
        extern "system" fn own(_: *mut c_void, _: u32, _: u32) -> i32 {
            7
        }
        let vt: &'static mut [usize; 9] = Box::leak(Box::new([0usize; 9]));
        vt[8] = own as *const () as usize;
        let obj_word: &'static mut usize = Box::leak(Box::new(vt.as_mut_ptr() as usize));
        let obj = obj_word as *mut usize as *mut c_void;
        // A stale, wrong per-Api value that must NOT win.
        set_original(Api::DxgiPresent, 0x2222);
        // SAFETY: `obj` points at a leaked vtable of length >= 9.
        unsafe { patch_slot(obj, 8, Api::DxgiPresent, dxgi_present as *const () as usize) };
        assert_eq!(
            resolve_original(obj, Some(8), Api::DxgiPresent),
            own as *const () as usize,
            "the object's own vtable original must win over the shared per-Api slot"
        );
        // Leave the shared per-Api slot as we found it (another test's detour
        // could otherwise forward through this bogus address).
        set_original(Api::DxgiPresent, 0);
    }

    #[test]
    fn iat_target_uses_the_frame_hook_original_for_swap_exports() {
        // Catches: a swap/queue export wired to a private slot instead of the
        // frame hook's shared `Api` original, so the detour would read 0 and
        // never forward.
        for (dll, func, api) in [
            ("opengl32.dll", "wglSwapBuffers", Api::WglSwapBuffers),
            ("gdi32.dll", "SwapBuffers", Api::GdiSwapBuffers),
            ("libegl.dll", "eglSwapBuffers", Api::EglSwapBuffers),
            ("vulkan-1.dll", "vkQueuePresentKHR", Api::VkQueuePresentKHR),
        ] {
            match iat_target(dll, func).expect("hooked").original {
                IatOriginal::Api(a) => assert_eq!(a, api, "{dll}!{func}"),
                IatOriginal::Slot(_) => panic!("{dll}!{func} should share the {api:?} original"),
            }
        }
    }

    #[test]
    fn vk_proc_addr_substitutes_a_detour_for_targets_and_forwards_the_rest() {
        // Catches: a proc-addr detour that (a) never substitutes our present
        // detour, so the app calls the raw loader and nothing is captured, or
        // (b) swaps the pointer without recording the real one, so the detour
        // has nothing to forward to (present returns 0, no frame recorded).
        //
        // `resolved` stands in for a real loader entry point: the proc-addr
        // detour must store *its address* (callable) as the original, so the
        // forwarded detour points somewhere real.
        let _g = serial();
        extern "system" fn resolved(_h: *mut c_void, _n: *const c_char) -> *const c_void {
            0xABCD as *const c_void
        }
        extern "system" fn loader(_h: *mut c_void, _n: *const c_char) -> *const c_void {
            resolved as *const () as *const c_void
        }
        extern "system" fn returns_null(_h: *mut c_void, _n: *const c_char) -> *const c_void {
            core::ptr::null()
        }
        const QUEUE_PRESENT: &[u8] = b"vkQueuePresentKHR\0";
        const CREATE_INSTANCE: &[u8] = b"vkCreateInstance\0";
        const GET_DEVICE_PROC: &[u8] = b"vkGetDeviceProcAddr\0";

        ORIG_VK_GET_INSTANCE_PROC.store(loader as *const () as usize, Ordering::Release);
        ORIG_VK_GET_DEVICE_PROC.store(loader as *const () as usize, Ordering::Release);

        // A target name resolves to our detour and the real pointer is saved.
        assert_eq!(
            vk_get_instance_proc_addr(
                core::ptr::null_mut(),
                QUEUE_PRESENT.as_ptr() as *const c_char
            ) as usize,
            vk_queue_present as *const () as usize,
            "a hooked name must resolve to our detour"
        );
        assert_eq!(
            original(Api::VkQueuePresentKHR),
            resolved as *const () as usize,
            "the real loader pointer must be recorded for forwarding"
        );
        assert_eq!(
            vk_get_device_proc_addr(
                core::ptr::null_mut(),
                QUEUE_PRESENT.as_ptr() as *const c_char
            ) as usize,
            vk_queue_present as *const () as usize,
            "the device proc-addr detour must substitute the present detour too"
        );

        // The instance detour hands back our device proc-addr detour (keeping
        // an ash-style loader inside the chain); the device detour must pass
        // that name through instead of re-entering itself.
        assert_eq!(
            vk_get_instance_proc_addr(
                core::ptr::null_mut(),
                GET_DEVICE_PROC.as_ptr() as *const c_char
            ) as usize,
            vk_get_device_proc_addr as *const () as usize,
            "the loader chain must stay inside the recorder"
        );
        assert_eq!(
            vk_get_device_proc_addr(
                core::ptr::null_mut(),
                GET_DEVICE_PROC.as_ptr() as *const c_char
            ) as usize,
            0xABCD,
            "vkGetDeviceProcAddr must forward its own name, not re-enter the chain"
        );

        // An unknown name, a null name, and a null real result all forward
        // unchanged (fail-open).
        assert_eq!(
            vk_get_instance_proc_addr(
                core::ptr::null_mut(),
                CREATE_INSTANCE.as_ptr() as *const c_char
            ) as usize,
            resolved as *const () as usize
        );
        assert_eq!(
            vk_get_instance_proc_addr(core::ptr::null_mut(), core::ptr::null()) as usize,
            resolved as *const () as usize
        );
        ORIG_VK_GET_INSTANCE_PROC.store(returns_null as *const () as usize, Ordering::Release);
        assert!(
            vk_get_instance_proc_addr(
                core::ptr::null_mut(),
                QUEUE_PRESENT.as_ptr() as *const c_char
            )
            .is_null(),
            "a null real result must stay null, never a detour"
        );

        // Leave the shared globals as we found them.
        set_original(Api::VkQueuePresentKHR, 0);
        ORIG_VK_GET_INSTANCE_PROC.store(0, Ordering::Release);
        ORIG_VK_GET_DEVICE_PROC.store(0, Ordering::Release);
    }

    #[test]
    fn registry_refuses_a_new_key_at_capacity_but_updates_an_existing_one() {
        // Catches: a full fixed-capacity registry either silently evicting a live
        // entry (a detour then forwards to the wrong original) or refusing to
        // update a key it already holds (a re-patch loses the newer original).
        let mut reg = [None; MAX_VTABLE_ORIGINALS];
        for i in 0..MAX_VTABLE_ORIGINALS {
            assert!(registry_insert(&mut reg, 0x1000 + i, i, i), "slot {i} fits");
        }
        assert!(
            registry_insert(&mut reg, 0x1000, 0, 999),
            "an existing key updates even when full"
        );
        assert_eq!(registry_lookup(&reg, 0x1000, 0), Some(999));
        assert!(
            !registry_insert(&mut reg, 0xDEAD, 0, 1),
            "a new key is refused when full"
        );
        assert_eq!(registry_lookup(&reg, 0xDEAD, 0), None);
        // No live entry was evicted by the refusal.
        assert_eq!(
            registry_lookup(
                &reg,
                0x1000 + MAX_VTABLE_ORIGINALS - 1,
                MAX_VTABLE_ORIGINALS - 1
            ),
            Some(MAX_VTABLE_ORIGINALS - 1)
        );
    }

    #[test]
    fn resolve_original_falls_back_when_the_objects_vtable_is_unknown() {
        // Catches: a lookup that returns a wrong (or zero) original for an object
        // whose class vtable was never registered. It must fall through to the
        // shared per-Api original, never guess.
        let _g = serial();
        extern "system" fn known(_: *mut c_void, _: u32, _: u32) -> i32 {
            1
        }
        let vt_known: &'static mut [usize; 11] = Box::leak(Box::new([0usize; 11]));
        vt_known[10] = known as *const () as usize;
        assert!(register_vtable_original(
            vt_known.as_ptr() as usize,
            10,
            known as *const () as usize
        ));
        set_original(Api::D3d12ExecuteCommandLists, 0x4242);
        // A different, unregistered vtable.
        let vt_unknown: &'static mut [usize; 11] = Box::leak(Box::new([0usize; 11]));
        let obj_word: &'static mut usize = Box::leak(Box::new(vt_unknown.as_ptr() as usize));
        let obj = obj_word as *mut usize as *mut c_void;
        assert_eq!(
            resolve_original(obj, Some(10), Api::D3d12ExecuteCommandLists),
            0x4242,
            "an unregistered vtable falls back to the per-Api original"
        );
        set_original(Api::D3d12ExecuteCommandLists, 0);
    }

    #[test]
    fn resolve_original_fails_open_when_the_registry_is_contended() {
        // Catches: a hot path that BLOCKS on the registry mutex, stalling the
        // present thread while the installer holds the lock. The lookup is
        // `try_lock`; on contention it must fall back to the per-Api original
        // immediately, not wait.
        let _g = serial();
        extern "system" fn own(_: *mut c_void, _: u32, _: u32) -> i32 {
            9
        }
        let vt: &'static mut [usize; 11] = Box::leak(Box::new([0usize; 11]));
        vt[10] = own as *const () as usize;
        assert!(register_vtable_original(
            vt.as_ptr() as usize,
            10,
            own as *const () as usize
        ));
        let obj_word: &'static mut usize = Box::leak(Box::new(vt.as_ptr() as usize));
        let obj = obj_word as *mut usize as *mut c_void;
        set_original(Api::D3d9ResetEx, 0x7777);
        // Hold the registry lock while resolving: the lookup must not block.
        let hold = VTABLE_ORIGINALS.lock().unwrap_or_else(|e| e.into_inner());
        assert_eq!(
            resolve_original(obj, Some(10), Api::D3d9ResetEx),
            0x7777,
            "a contended registry must fail open, never block the present"
        );
        drop(hold);
        set_original(Api::D3d9ResetEx, 0);
    }

    #[test]
    fn with_recorder_never_blocks_the_present_hot_path() {
        // Catches: a blocking `lock()` in `with_recorder` — the only shared state
        // on every frame detour. If the recorder is held (the installer
        // mid-patch), the present must drop its sample and return, never stall
        // the game's present thread.
        let _g = serial();
        if RECORDER.get().is_none() {
            #[repr(align(8))]
            struct Block([u8; hook_ipc::TOTAL_SIZE]);
            let b: &'static mut Block = Box::leak(Box::new(Block([0u8; hook_ipc::TOTAL_SIZE])));
            // SAFETY: the leaked block is writable, 8-aligned, and lives for the
            // process.
            let rec = unsafe { Recorder::create(b.0.as_mut_ptr(), b.0.len(), 1_000_000, 1) }
                .expect("create recorder");
            let _ = set_recorder(rec);
        }
        let m = RECORDER.get().expect("a recorder is installed");
        let hold = m.lock().unwrap_or_else(|e| e.into_inner());
        let (tx, rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            with_recorder(|_| {});
            let _ = tx.send(());
        });
        assert!(
            rx.recv_timeout(std::time::Duration::from_secs(5)).is_ok(),
            "the frame hot path blocked on a held recorder"
        );
        handle.join().expect("detour thread");
        drop(hold);
    }

    // -- argument forwarding: fake originals that record what they received ---
    //
    // Each detour must call the original with the caller's arguments untouched,
    // in order. A wrong signature or swapped argument silently corrupts the
    // target (a present with the flags as the swapchain, a create with the
    // device as the descriptor), so every hooked entry point is driven through
    // a fake original and compared argument-for-argument.

    /// Every argument a fake original received, flattened to `u64`, in order.
    static FORWARDED: Mutex<Vec<u64>> = Mutex::new(Vec::new());

    fn forwarded(args: &[u64]) {
        FORWARDED
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .extend_from_slice(args);
    }

    fn take_forwarded() -> Vec<u64> {
        std::mem::take(&mut *FORWARDED.lock().unwrap_or_else(|e| e.into_inner()))
    }

    /// A leaked fake COM object whose vtable slot `index` is `f`. Leaked so a
    /// process-wide `restore_all_patches` can always restore it safely, exactly
    /// as the existing registry tests do.
    fn fake_obj(slots: usize, index: usize, f: usize) -> *mut c_void {
        let vt: &'static mut Vec<usize> = Box::leak(Box::new(vec![0usize; slots]));
        vt[index] = f;
        let word: &'static mut usize = Box::leak(Box::new(vt.as_mut_ptr() as usize));
        word as *mut usize as *mut c_void
    }

    // Frame-hook fakes (each returns a distinct sentinel).
    extern "system" fn f_present(t: *mut c_void, a: u32, b: u32) -> i32 {
        forwarded(&[t as u64, a as u64, b as u64]);
        101
    }
    extern "system" fn f_present1(t: *mut c_void, a: u32, b: u32, p: *const c_void) -> i32 {
        forwarded(&[t as u64, a as u64, b as u64, p as u64]);
        102
    }
    extern "system" fn f_setfs(t: *mut c_void, full: i32, out: *mut c_void) -> i32 {
        forwarded(&[t as u64, full as u64, out as u64]);
        103
    }
    extern "system" fn f_resize(t: *mut c_void, a: u32, b: u32, c: u32, d: u32, e: u32) -> i32 {
        forwarded(&[t as u64, a as u64, b as u64, c as u64, d as u64, e as u64]);
        104
    }
    extern "system" fn f_d9present(
        t: *mut c_void,
        s: *const c_void,
        d: *const c_void,
        h: *mut c_void,
        dr: *const c_void,
    ) -> i32 {
        forwarded(&[t as u64, s as u64, d as u64, h as u64, dr as u64]);
        105
    }
    extern "system" fn f_endscene(t: *mut c_void) -> i32 {
        forwarded(&[t as u64]);
        106
    }
    extern "system" fn f_d9present_ex(
        t: *mut c_void,
        s: *const c_void,
        d: *const c_void,
        h: *mut c_void,
        dr: *const c_void,
        fl: u32,
    ) -> i32 {
        forwarded(&[t as u64, s as u64, d as u64, h as u64, dr as u64, fl as u64]);
        107
    }
    extern "system" fn f_reset_ex(t: *mut c_void, p: *mut c_void, m: *mut c_void) -> i32 {
        forwarded(&[t as u64, p as u64, m as u64]);
        108
    }
    extern "system" fn f_ecl(t: *mut c_void, n: u32, l: *const *const c_void) {
        forwarded(&[t as u64, n as u64, l as u64]);
    }
    extern "system" fn f_wgl(h: *mut c_void) -> i32 {
        forwarded(&[h as u64]);
        110
    }
    extern "system" fn f_wgl_layer(h: *mut c_void, p: u32) -> i32 {
        forwarded(&[h as u64, p as u64]);
        111
    }
    extern "system" fn f_egl(d: *mut c_void, s: *mut c_void) -> i32 {
        forwarded(&[d as u64, s as u64]);
        112
    }
    extern "system" fn f_vkq(q: *mut c_void, i: *const c_void) -> i32 {
        forwarded(&[q as u64, i as u64]);
        113
    }

    // Acquisition fakes. They return a negative HRESULT so the detour's
    // post-forward patching (which dereferences the out-pointer / device) is
    // skipped and the forwarding assertion is exact.
    extern "system" fn f_csc(
        t: *mut c_void,
        dev: *mut c_void,
        desc: *const c_void,
        out: *mut *mut c_void,
    ) -> i32 {
        forwarded(&[t as u64, dev as u64, desc as u64, out as u64]);
        -7
    }
    extern "system" fn f_csc_ok(
        t: *mut c_void,
        dev: *mut c_void,
        desc: *const c_void,
        out: *mut *mut c_void,
    ) -> i32 {
        forwarded(&[t as u64, dev as u64, desc as u64, out as u64]);
        0
    }
    extern "system" fn f_csch(
        t: *mut c_void,
        dev: *mut c_void,
        w: *mut c_void,
        desc: *const c_void,
        fs: *const c_void,
        re: *mut c_void,
        out: *mut *mut c_void,
    ) -> i32 {
        forwarded(&[
            t as u64,
            dev as u64,
            w as u64,
            desc as u64,
            fs as u64,
            re as u64,
            out as u64,
        ]);
        -7
    }
    extern "system" fn f_cscc(
        t: *mut c_void,
        dev: *mut c_void,
        w: *mut c_void,
        desc: *const c_void,
        re: *mut c_void,
        out: *mut *mut c_void,
    ) -> i32 {
        forwarded(&[
            t as u64,
            dev as u64,
            w as u64,
            desc as u64,
            re as u64,
            out as u64,
        ]);
        -7
    }
    extern "system" fn f_cscomp(
        t: *mut c_void,
        dev: *mut c_void,
        desc: *const c_void,
        re: *mut c_void,
        out: *mut *mut c_void,
    ) -> i32 {
        forwarded(&[t as u64, dev as u64, desc as u64, re as u64, out as u64]);
        -7
    }
    extern "system" fn f_d9cd(
        t: *mut c_void,
        a: u32,
        dt: u32,
        h: *mut c_void,
        fl: u32,
        pp: *mut c_void,
        out: *mut *mut c_void,
    ) -> i32 {
        forwarded(&[
            t as u64, a as u64, dt as u64, h as u64, fl as u64, pp as u64, out as u64,
        ]);
        -7
    }
    extern "system" fn f_d9cdex(
        t: *mut c_void,
        a: u32,
        dt: u32,
        h: *mut c_void,
        fl: u32,
        pp: *mut c_void,
        m: *mut c_void,
        out: *mut *mut c_void,
    ) -> i32 {
        forwarded(&[
            t as u64, a as u64, dt as u64, h as u64, fl as u64, pp as u64, m as u64, out as u64,
        ]);
        -7
    }
    extern "system" fn f_factory(riid: *const c_void, out: *mut *mut c_void) -> i32 {
        forwarded(&[riid as u64, out as u64]);
        -7
    }
    extern "system" fn f_factory2(flags: u32, riid: *const c_void, out: *mut *mut c_void) -> i32 {
        forwarded(&[flags as u64, riid as u64, out as u64]);
        -7
    }
    #[allow(clippy::too_many_arguments)]
    extern "system" fn f_d3d11(
        a: *mut c_void,
        dt: u32,
        sw: *mut c_void,
        fl: u32,
        flv: *const c_void,
        nl: u32,
        sv: u32,
        scd: *const c_void,
        pps: *mut *mut c_void,
        ppd: *mut *mut c_void,
        pfl: *mut c_void,
        ppc: *mut *mut c_void,
    ) -> i32 {
        forwarded(&[
            a as u64, dt as u64, sw as u64, fl as u64, flv as u64, nl as u64, sv as u64,
            scd as u64, pps as u64, ppd as u64, pfl as u64, ppc as u64,
        ]);
        -7
    }
    extern "system" fn f_d3dc9(v: u32) -> *mut c_void {
        forwarded(&[v as u64]);
        core::ptr::null_mut()
    }
    extern "system" fn f_d3dc9_ex(v: u32, out: *mut *mut c_void) -> i32 {
        forwarded(&[v as u64, out as u64]);
        -7
    }
    extern "system" fn f_vkcs(
        dev: *mut c_void,
        ci: *const c_void,
        al: *const c_void,
        out: *mut *mut c_void,
    ) -> i32 {
        forwarded(&[dev as u64, ci as u64, al as u64, out as u64]);
        140
    }
    extern "system" fn f_vkgsi(
        dev: *mut c_void,
        sc: *mut c_void,
        cnt: *mut u32,
        img: *mut *mut c_void,
    ) -> i32 {
        forwarded(&[dev as u64, sc as u64, cnt as u64, img as u64]);
        141
    }
    extern "system" fn f_vkcd(
        pd: *mut c_void,
        ci: *const c_void,
        al: *const c_void,
        out: *mut *mut c_void,
    ) -> i32 {
        forwarded(&[pd as u64, ci as u64, al as u64, out as u64]);
        142
    }
    extern "system" fn f_vkdd(dev: *mut c_void, al: *const c_void) {
        forwarded(&[dev as u64, al as u64]);
    }

    /// Patch a fake COM slot and drive the detour, returning `(return, args)`.
    fn drive_com(
        slots: usize,
        index: usize,
        fake: usize,
        api: Api,
        detour: usize,
        call: impl FnOnce(*mut c_void) -> i32,
    ) -> (i32, Vec<u64>) {
        let obj = fake_obj(slots, index, fake);
        // SAFETY: `obj` points at a leaked vtable with `index` inside it.
        unsafe { patch_slot(obj, index, api, detour) };
        let ret = call(obj);
        (ret, take_forwarded())
    }

    #[test]
    fn forward_dxgi_present_every_argument() {
        let _g = serial();
        let (ret, got) = drive_com(
            9,
            8,
            f_present as *const () as usize,
            Api::DxgiPresent,
            dxgi_present as *const () as usize,
            |o| dxgi_present(o, 0xAAAA_1111, 0xBBBB_2222),
        );
        let obj = got[0];
        assert_eq!(ret, 101);
        assert_eq!(got, vec![obj, 0xAAAA_1111, 0xBBBB_2222]);
    }

    #[test]
    fn forward_dxgi_present1_every_argument() {
        let _g = serial();
        // A null params pointer must also be forwarded without a deref.
        let (ret, got) = drive_com(
            23,
            22,
            f_present1 as *const () as usize,
            Api::DxgiPresent1,
            dxgi_present1 as *const () as usize,
            |o| dxgi_present1(o, 1, 2, core::ptr::null()),
        );
        assert_eq!(ret, 102);
        assert_eq!(got, vec![got[0], 1, 2, 0]);
    }

    #[test]
    fn forward_dxgi_setfullscreen_every_argument() {
        let _g = serial();
        let (ret, got) = drive_com(
            11,
            10,
            f_setfs as *const () as usize,
            Api::DxgiSetFullscreenState,
            dxgi_setfullscreen as *const () as usize,
            |o| dxgi_setfullscreen(o, -1, 0x1234 as *mut c_void),
        );
        assert_eq!(ret, 103);
        assert_eq!(got, vec![got[0], (-1i32) as u64, 0x1234]);
    }

    #[test]
    fn forward_dxgi_resizebuffers_every_argument() {
        let _g = serial();
        let (ret, got) = drive_com(
            14,
            13,
            f_resize as *const () as usize,
            Api::DxgiResizeBuffers,
            dxgi_resizebuffers as *const () as usize,
            |o| dxgi_resizebuffers(o, 3, 1920, 1080, 0x28, 0x1),
        );
        assert_eq!(ret, 104);
        assert_eq!(got, vec![got[0], 3, 1920, 1080, 0x28, 0x1]);
    }

    #[test]
    fn forward_d3d9_present_every_argument() {
        let _g = serial();
        let (ret, got) = drive_com(
            18,
            17,
            f_d9present as *const () as usize,
            Api::D3d9Present,
            d3d9_present as *const () as usize,
            |o| {
                d3d9_present(
                    o,
                    0xA1 as *const c_void,
                    0xA2 as *const c_void,
                    0xA3 as *mut c_void,
                    0xA4 as *const c_void,
                )
            },
        );
        assert_eq!(ret, 105);
        assert_eq!(got, vec![got[0], 0xA1, 0xA2, 0xA3, 0xA4]);
    }

    #[test]
    fn forward_d3d9_endscene_every_argument() {
        let _g = serial();
        let (ret, got) = drive_com(
            43,
            42,
            f_endscene as *const () as usize,
            Api::D3d9EndScene,
            d3d9_endscene as *const () as usize,
            |o| d3d9_endscene(o),
        );
        assert_eq!(ret, 106);
        assert_eq!(got, vec![got[0]]);
    }

    #[test]
    fn forward_d3d9_present_ex_every_argument() {
        let _g = serial();
        let (ret, got) = drive_com(
            122,
            121,
            f_d9present_ex as *const () as usize,
            Api::D3d9PresentEx,
            d3d9_present_ex as *const () as usize,
            |o| {
                d3d9_present_ex(
                    o,
                    0xB1 as *const c_void,
                    0xB2 as *const c_void,
                    0xB3 as *mut c_void,
                    0xB4 as *const c_void,
                    0xB5,
                )
            },
        );
        assert_eq!(ret, 107);
        assert_eq!(got, vec![got[0], 0xB1, 0xB2, 0xB3, 0xB4, 0xB5]);
    }

    #[test]
    fn forward_d3d9_reset_ex_every_argument() {
        let _g = serial();
        let (ret, got) = drive_com(
            133,
            132,
            f_reset_ex as *const () as usize,
            Api::D3d9ResetEx,
            d3d9_reset_ex as *const () as usize,
            |o| d3d9_reset_ex(o, 0xC1 as *mut c_void, 0xC2 as *mut c_void),
        );
        assert_eq!(ret, 108);
        assert_eq!(got, vec![got[0], 0xC1, 0xC2]);
    }

    #[test]
    fn forward_d3d12_execute_command_lists_every_argument() {
        let _g = serial();
        let obj = fake_obj(11, 10, f_ecl as *const () as usize);
        // SAFETY: `obj` points at a leaked vtable with slot 10.
        unsafe {
            patch_slot(
                obj,
                10,
                Api::D3d12ExecuteCommandLists,
                d3d12_execute_command_lists as *const () as usize,
            )
        };
        let lists = 0xD1 as *const *const c_void;
        d3d12_execute_command_lists(obj, 5, lists);
        assert_eq!(take_forwarded(), vec![obj as u64, 5, 0xD1]);
    }

    #[test]
    fn forward_free_function_frame_detours_every_argument() {
        let _g = serial();
        // wglSwapBuffers(HDC)
        set_original(Api::WglSwapBuffers, f_wgl as *const () as usize);
        assert_eq!(wgl_swap_buffers(0xE1 as *mut c_void), 110);
        assert_eq!(take_forwarded(), vec![0xE1]);
        // wglSwapLayerBuffers(HDC, UINT)
        set_original(Api::WglSwapLayerBuffers, f_wgl_layer as *const () as usize);
        assert_eq!(wgl_swap_layer_buffers(0xE2 as *mut c_void, 1), 111);
        assert_eq!(take_forwarded(), vec![0xE2, 1]);
        // gdi32!SwapBuffers(HDC)
        set_original(Api::GdiSwapBuffers, f_wgl as *const () as usize);
        assert_eq!(gdi_swap_buffers(0xE3 as *mut c_void), 110);
        assert_eq!(take_forwarded(), vec![0xE3]);
        // eglSwapBuffers(display, surface)
        set_original(Api::EglSwapBuffers, f_egl as *const () as usize);
        assert_eq!(
            egl_swap_buffers(0xE4 as *mut c_void, 0xE5 as *mut c_void),
            112
        );
        assert_eq!(take_forwarded(), vec![0xE4, 0xE5]);
        // vkQueuePresentKHR(queue, info)
        set_original(Api::VkQueuePresentKHR, f_vkq as *const () as usize);
        assert_eq!(
            vk_queue_present(0xE6 as *mut c_void, 0xE7 as *const c_void),
            113
        );
        assert_eq!(take_forwarded(), vec![0xE6, 0xE7]);
        // Leave the shared per-Api originals as we found them so another test's
        // detour cannot forward through a stale fake.
        for api in [
            Api::WglSwapBuffers,
            Api::WglSwapLayerBuffers,
            Api::GdiSwapBuffers,
            Api::EglSwapBuffers,
            Api::VkQueuePresentKHR,
        ] {
            set_original(api, 0);
        }
    }

    #[test]
    fn forward_create_swap_chain_variants_every_argument() {
        let _g = serial();
        let mut out: *mut c_void = core::ptr::null_mut();
        let outp: *mut *mut c_void = &mut out;
        let (ret, got) = drive_com(
            11,
            10,
            f_csc as *const () as usize,
            Api::DxgiCreateSwapChain,
            create_swap_chain as *const () as usize,
            |o| create_swap_chain(o, 0x1001 as *mut c_void, 0x2 as *const c_void, outp),
        );
        assert_eq!(ret, -7);
        assert_eq!(got, vec![got[0], 0x1001, 0x2, outp as u64]);

        let (ret, got) = drive_com(
            16,
            15,
            f_csch as *const () as usize,
            Api::DxgiCreateSwapChainForHwnd,
            create_swap_chain_for_hwnd as *const () as usize,
            |o| {
                create_swap_chain_for_hwnd(
                    o,
                    0x3 as *mut c_void,
                    0x4 as *mut c_void,
                    0x5 as *const c_void,
                    0x6 as *const c_void,
                    0x7 as *mut c_void,
                    outp,
                )
            },
        );
        assert_eq!(ret, -7);
        assert_eq!(got, vec![got[0], 0x3, 0x4, 0x5, 0x6, 0x7, outp as u64]);

        let (ret, got) = drive_com(
            17,
            16,
            f_cscc as *const () as usize,
            Api::DxgiCreateSwapChainForCoreWindow,
            create_swap_chain_for_core_window as *const () as usize,
            |o| {
                create_swap_chain_for_core_window(
                    o,
                    0x8 as *mut c_void,
                    0x9 as *mut c_void,
                    0xA as *const c_void,
                    0xB as *mut c_void,
                    outp,
                )
            },
        );
        assert_eq!(ret, -7);
        assert_eq!(got, vec![got[0], 0x8, 0x9, 0xA, 0xB, outp as u64]);

        let (ret, got) = drive_com(
            25,
            24,
            f_cscomp as *const () as usize,
            Api::DxgiCreateSwapChainForComposition,
            create_swap_chain_for_composition as *const () as usize,
            |o| {
                create_swap_chain_for_composition(
                    o,
                    0xC as *mut c_void,
                    0xD as *const c_void,
                    0xE as *mut c_void,
                    outp,
                )
            },
        );
        assert_eq!(ret, -7);
        assert_eq!(got, vec![got[0], 0xC, 0xD, 0xE, outp as u64]);
    }

    #[test]
    fn forward_d3d9_create_device_variants_every_argument() {
        let _g = serial();
        let mut out: *mut c_void = core::ptr::null_mut();
        let outp: *mut *mut c_void = &mut out;
        let (ret, got) = drive_com(
            17,
            16,
            f_d9cd as *const () as usize,
            Api::D3d9CreateDevice,
            d3d9_create_device as *const () as usize,
            |o| {
                d3d9_create_device(
                    o,
                    1,
                    2,
                    0xF1 as *mut c_void,
                    0xF2,
                    0xF3 as *mut c_void,
                    outp,
                )
            },
        );
        assert_eq!(ret, -7);
        assert_eq!(got, vec![got[0], 1, 2, 0xF1, 0xF2, 0xF3, outp as u64]);

        let (ret, got) = drive_com(
            21,
            20,
            f_d9cdex as *const () as usize,
            Api::D3d9CreateDeviceEx,
            d3d9_create_device_ex as *const () as usize,
            |o| {
                d3d9_create_device_ex(
                    o,
                    3,
                    4,
                    0xF6 as *mut c_void,
                    0xF7,
                    0xF8 as *mut c_void,
                    0xF9 as *mut c_void,
                    outp,
                )
            },
        );
        assert_eq!(ret, -7);
        assert_eq!(got, vec![got[0], 3, 4, 0xF6, 0xF7, 0xF8, 0xF9, outp as u64]);
    }

    #[test]
    fn forward_free_function_acquisition_detours_every_argument() {
        let _g = serial();
        let mut out: *mut c_void = core::ptr::null_mut();
        let outp: *mut *mut c_void = &mut out;

        ORIG_FACTORY0.store(f_factory as *const () as usize, Ordering::Release);
        ORIG_FACTORY1.store(f_factory as *const () as usize, Ordering::Release);
        ORIG_FACTORY2.store(f_factory2 as *const () as usize, Ordering::Release);
        ORIG_D3D11_CREATE.store(f_d3d11 as *const () as usize, Ordering::Release);
        ORIG_D3D9_CREATE.store(f_d3dc9 as *const () as usize, Ordering::Release);
        ORIG_D3D9_CREATE_EX.store(f_d3dc9_ex as *const () as usize, Ordering::Release);

        assert_eq!(create_dxgi_factory(0x1001 as *const c_void, outp), -7);
        assert_eq!(take_forwarded(), vec![0x1001, outp as u64]);
        assert_eq!(create_dxgi_factory1(0x2 as *const c_void, outp), -7);
        assert_eq!(take_forwarded(), vec![0x2, outp as u64]);
        assert_eq!(create_dxgi_factory2(0xFE, 0x3 as *const c_void, outp), -7);
        assert_eq!(take_forwarded(), vec![0xFE, 0x3, outp as u64]);

        let d3d11_ret = d3d11_create_device_and_swap_chain(
            0x1001 as *mut c_void,
            2,
            0x3 as *mut c_void,
            4,
            0x5 as *const c_void,
            6,
            7,
            0x8 as *const c_void,
            outp,
            outp,
            0x9 as *mut c_void,
            outp,
        );
        assert_eq!(d3d11_ret, -7);
        assert_eq!(
            take_forwarded(),
            vec![
                0x1001,
                2,
                0x3,
                4,
                0x5,
                6,
                7,
                0x8,
                outp as u64,
                outp as u64,
                0x9,
                outp as u64
            ]
        );

        assert!(direct3d_create9(0x21).is_null());
        assert_eq!(take_forwarded(), vec![0x21]);
        assert_eq!(direct3d_create9_ex(0x22, outp), -7);
        assert_eq!(take_forwarded(), vec![0x22, outp as u64]);

        ORIG_FACTORY0.store(0, Ordering::Release);
        ORIG_FACTORY1.store(0, Ordering::Release);
        ORIG_FACTORY2.store(0, Ordering::Release);
        ORIG_D3D11_CREATE.store(0, Ordering::Release);
        ORIG_D3D9_CREATE.store(0, Ordering::Release);
        ORIG_D3D9_CREATE_EX.store(0, Ordering::Release);
    }

    #[test]
    fn vk_dispatch_returns_each_entry_points_own_detour() {
        // Catches: a copy-paste in `vk_detour` mapping one Vulkan name to
        // another's detour. The app calls the returned pointer with *its* own
        // signature, so a mis-mapped name (e.g. `vkQueuePresentKHR` resolving to
        // the acquisition detour) corrupts the call — or crashes — on the
        // present path with no record. Every name must resolve to exactly the
        // detour declared for that `Detour` variant.
        let cases: [(&str, Detour, usize); 6] = [
            (
                "vkQueuePresentKHR",
                Detour::QueuePresent,
                vk_queue_present as *const () as usize,
            ),
            (
                "vkCreateSwapchainKHR",
                Detour::CreateSwapchain,
                vk_create_swapchain as *const () as usize,
            ),
            (
                "vkGetSwapchainImagesKHR",
                Detour::GetSwapchainImages,
                vk_get_swapchain_images as *const () as usize,
            ),
            (
                "vkCreateDevice",
                Detour::CreateDevice,
                vk_create_device as *const () as usize,
            ),
            (
                "vkDestroyDevice",
                Detour::DestroyDevice,
                vk_destroy_device as *const () as usize,
            ),
            (
                "vkGetDeviceProcAddr",
                Detour::GetDeviceProcAddr,
                vk_get_device_proc_addr as *const () as usize,
            ),
        ];
        let mut addrs: Vec<usize> = Vec::new();
        for (name, detour, expected) in cases {
            assert_eq!(vk_target(name), Some(detour), "{name} maps to {detour:?}");
            let got = vk_detour(detour);
            assert_ne!(got, 0, "{name} must resolve to a real detour");
            assert_eq!(got, expected, "{name} resolved to the wrong detour");
            assert!(!addrs.contains(&got), "two names share detour {name}");
            addrs.push(got);
        }
        // A name we do not hook maps to no substitution at all.
        assert_eq!(vk_target("vkGetInstanceProcAddr"), None);
        assert_eq!(vk_target("vkCreateInstance"), None);
    }

    #[test]
    fn forward_vulkan_acquisition_detours_every_argument() {
        let _g = serial();
        let mut out: *mut c_void = core::ptr::null_mut();
        let outp: *mut *mut c_void = &mut out;

        ORIG_VK_CREATE_SWAPCHAIN.store(f_vkcs as *const () as usize, Ordering::Release);
        ORIG_VK_GET_IMAGES.store(f_vkgsi as *const () as usize, Ordering::Release);
        ORIG_VK_CREATE_DEVICE.store(f_vkcd as *const () as usize, Ordering::Release);
        ORIG_VK_DESTROY_DEVICE.store(f_vkdd as *const () as usize, Ordering::Release);

        assert_eq!(
            vk_create_swapchain(
                0x1001 as *mut c_void,
                0x2 as *const c_void,
                0x3 as *const c_void,
                outp
            ),
            140
        );
        assert_eq!(take_forwarded(), vec![0x1001, 0x2, 0x3, outp as u64]);

        let mut count = 0u32;
        assert_eq!(
            vk_get_swapchain_images(0x4 as *mut c_void, 0x5 as *mut c_void, &mut count, outp),
            141
        );
        assert_eq!(
            take_forwarded(),
            vec![0x4, 0x5, &mut count as *mut u32 as u64, outp as u64]
        );

        assert_eq!(
            vk_create_device(
                0x6 as *mut c_void,
                0x7 as *const c_void,
                0x8 as *const c_void,
                outp
            ),
            142
        );
        assert_eq!(take_forwarded(), vec![0x6, 0x7, 0x8, outp as u64]);

        vk_destroy_device(0x9 as *mut c_void, 0xA as *const c_void);
        assert_eq!(take_forwarded(), vec![0x9, 0xA]);

        ORIG_VK_CREATE_SWAPCHAIN.store(0, Ordering::Release);
        ORIG_VK_GET_IMAGES.store(0, Ordering::Release);
        ORIG_VK_CREATE_DEVICE.store(0, Ordering::Release);
        ORIG_VK_DESTROY_DEVICE.store(0, Ordering::Release);
    }

    #[test]
    fn a_null_com_object_forwards_without_a_deref() {
        // Catches: a COM detour dereferencing a null `this` instead of failing
        // open to the shared per-Api original. The caller may legitimately pass
        // null (an unset handle); the detour must forward it unchanged.
        let _g = serial();
        set_original(Api::DxgiPresent, f_present as *const () as usize);
        assert_eq!(dxgi_present(core::ptr::null_mut(), 7, 8), 101);
        assert_eq!(take_forwarded(), vec![0, 7, 8]);
        set_original(Api::DxgiPresent, 0);

        set_original(Api::WglSwapBuffers, f_wgl as *const () as usize);
        assert_eq!(wgl_swap_buffers(core::ptr::null_mut()), 110);
        assert_eq!(take_forwarded(), vec![0]);
        set_original(Api::WglSwapBuffers, 0);
    }

    #[test]
    fn an_acquisition_detour_accepts_a_null_out_pointer() {
        // Catches: an acquisition detour dereferencing a null out-pointer (or a
        // null device) on the success path — a crash on the game's
        // swapchain-creation path. A failure HRESULT is not required to be safe;
        // a *successful* create with a null out must still not fault.
        let _g = serial();
        let obj = fake_obj(11, 10, f_csc_ok as *const () as usize);
        // SAFETY: `obj` points at a leaked vtable with slot 10.
        unsafe {
            patch_slot(
                obj,
                10,
                Api::DxgiCreateSwapChain,
                create_swap_chain as *const () as usize,
            )
        };
        let ret = create_swap_chain(
            obj,
            core::ptr::null_mut(),
            core::ptr::null(),
            core::ptr::null_mut(),
        );
        assert_eq!(ret, 0, "the original's success HRESULT must be forwarded");
        assert_eq!(take_forwarded(), vec![obj as u64, 0, 0, 0]);
    }

    #[test]
    fn patch_slot_twice_with_the_same_detour_keeps_the_real_original() {
        // Catches: the double-patch guard interacting wrongly with the per-vtable
        // registry. A shared class vtable is reached twice (a bootstrap dummy,
        // then the app's own object). The second `patch_slot` must be refused by
        // the value guard *before* it can overwrite the registry's original, or
        // the detour resolves to itself and recurses without bound on the
        // present thread.
        let _g = serial();
        extern "system" fn real(_: *mut c_void, _: u32, _: u32) -> i32 {
            4242
        }
        let vt: &'static mut [usize; 9] = Box::leak(Box::new([0usize; 9]));
        vt[8] = real as *const () as usize;
        let obj_word: &'static mut usize = Box::leak(Box::new(vt.as_mut_ptr() as usize));
        let obj = obj_word as *mut usize as *mut c_void;
        // SAFETY: `obj` points at a leaked vtable with a real slot 8.
        unsafe {
            patch_slot(obj, 8, Api::DxgiPresent, dxgi_present as *const () as usize);
            // The same class vtable reached again (shared) must not re-patch.
            patch_slot(obj, 8, Api::DxgiPresent, dxgi_present as *const () as usize);
        }
        assert_eq!(
            resolve_original(obj, Some(8), Api::DxgiPresent),
            real as *const () as usize,
            "the real original must survive a shared-vtable double patch"
        );
        assert_eq!(
            dxgi_present(obj, 0, 0),
            4242,
            "the detour must forward to the real original, not to itself"
        );
    }

    // -- record stamping ------------------------------------------------------

    /// One `record_frame` call's `(api, start, stop, handle, flags)`.
    type FrameStamp = (Api, u64, u64, u64, u32);

    /// The last `record_frame` call's arguments. Test-only observation point;
    /// `record_frame` calls [`note_frame`] only in test builds.
    static LAST_FRAME: Mutex<Option<FrameStamp>> = Mutex::new(None);

    /// Called by `record_frame` (test builds only) to capture what it recorded.
    pub(super) fn note_frame(api: Api, start: u64, stop: u64, handle: u64, flags: u32) {
        *LAST_FRAME.lock().unwrap_or_else(|e| e.into_inner()) =
            Some((api, start, stop, handle, flags));
    }

    /// Take the last recorded frame's `(api, handle)`, or `None`.
    fn last_api_and_handle() -> Option<(Api, u64)> {
        LAST_FRAME
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .map(|(api, _start, _stop, handle, _flags)| (api, handle))
    }

    #[test]
    fn every_frame_detour_stamps_its_own_api_and_handle() {
        // Catches: a frame detour whose `record_frame` uses a *neighbouring*
        // entry point's `Api` (a copy-paste in the macro invocation). The detour
        // still forwards and the target still presents, but the host would
        // misattribute every captured frame — the silent corruption the wire
        // format is meant to prevent. With the shared originals cleared, a detour
        // takes its fail-open branch and only records, so no fake original is
        // needed.
        let _g = serial();
        for api in [
            Api::DxgiPresent,
            Api::DxgiPresent1,
            Api::DxgiSetFullscreenState,
            Api::DxgiResizeBuffers,
            Api::D3d9Present,
            Api::D3d9EndScene,
            Api::D3d9PresentEx,
            Api::D3d9ResetEx,
            Api::D3d12ExecuteCommandLists,
            Api::WglSwapBuffers,
            Api::WglSwapLayerBuffers,
            Api::GdiSwapBuffers,
            Api::EglSwapBuffers,
            Api::VkQueuePresentKHR,
        ] {
            set_original(api, 0);
        }

        // COM detours with a null `this` fail open and stamp handle 0.
        dxgi_present(core::ptr::null_mut(), 1, 2);
        assert_eq!(last_api_and_handle(), Some((Api::DxgiPresent, 0)));
        dxgi_present1(core::ptr::null_mut(), 1, 2, core::ptr::null());
        assert_eq!(last_api_and_handle(), Some((Api::DxgiPresent1, 0)));
        dxgi_setfullscreen(core::ptr::null_mut(), 0, core::ptr::null_mut());
        assert_eq!(
            last_api_and_handle(),
            Some((Api::DxgiSetFullscreenState, 0))
        );
        dxgi_resizebuffers(core::ptr::null_mut(), 0, 0, 0, 0, 0);
        assert_eq!(last_api_and_handle(), Some((Api::DxgiResizeBuffers, 0)));
        d3d9_present(
            core::ptr::null_mut(),
            core::ptr::null(),
            core::ptr::null(),
            core::ptr::null_mut(),
            core::ptr::null(),
        );
        assert_eq!(last_api_and_handle(), Some((Api::D3d9Present, 0)));
        d3d9_endscene(core::ptr::null_mut());
        assert_eq!(last_api_and_handle(), Some((Api::D3d9EndScene, 0)));
        d3d9_present_ex(
            core::ptr::null_mut(),
            core::ptr::null(),
            core::ptr::null(),
            core::ptr::null_mut(),
            core::ptr::null(),
            0,
        );
        assert_eq!(last_api_and_handle(), Some((Api::D3d9PresentEx, 0)));
        d3d9_reset_ex(
            core::ptr::null_mut(),
            core::ptr::null_mut(),
            core::ptr::null_mut(),
        );
        assert_eq!(last_api_and_handle(), Some((Api::D3d9ResetEx, 0)));
        d3d12_execute_command_lists(core::ptr::null_mut(), 0, core::ptr::null());
        assert_eq!(
            last_api_and_handle(),
            Some((Api::D3d12ExecuteCommandLists, 0))
        );

        // Free-function detours: the handle is their first argument.
        wgl_swap_buffers(0x11 as *mut c_void);
        assert_eq!(last_api_and_handle(), Some((Api::WglSwapBuffers, 0x11)));
        wgl_swap_layer_buffers(0x22 as *mut c_void, 1);
        assert_eq!(
            last_api_and_handle(),
            Some((Api::WglSwapLayerBuffers, 0x22))
        );
        gdi_swap_buffers(0x33 as *mut c_void);
        assert_eq!(last_api_and_handle(), Some((Api::GdiSwapBuffers, 0x33)));
        egl_swap_buffers(0x44 as *mut c_void, 0x55 as *mut c_void);
        assert_eq!(last_api_and_handle(), Some((Api::EglSwapBuffers, 0x55)));
        vk_queue_present(0x66 as *mut c_void, core::ptr::null());
        assert_eq!(last_api_and_handle(), Some((Api::VkQueuePresentKHR, 0x66)));
    }
}
