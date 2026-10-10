//! `hook-vk-layer` — the minihud Vulkan **implicit layer**.
//!
//! Some applications resolve Vulkan **only** through `GetProcAddress`
//! (`ash::Entry::load()` / libloading) and never statically import the loader's
//! proc-addr export, so the recorder's IAT hook never sees them. The loader
//! loads an implicit layer before the app resolves any entry point, so wrapping
//! `vkQueuePresentKHR` here covers every dynamic-resolution app.
//!
//! It records one `QueryPerformanceCounter` timestamp per present into the same
//! shared ring as the injected recorder (`hook-ipc`), then forwards the present
//! down the chain. It is fail-open and panic-free across the FFI boundary: any
//! failure just forwards.
//!
//! The hand-declared Vulkan subset (the loader ABI is tiny and stable) keeps the
//! layer dependency-light: no Vulkan loader, headers, or binding crate.

#![cfg(windows)]
#![allow(non_snake_case)]

mod dispatch;
mod ring;

pub use dispatch::{proc_kind, Proc};

use core::ffi::{c_char, c_void, CStr};
use core::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

use ring::LayerRecorder;

// -- Vulkan / loader ABI subset (hand-declared) ------------------------------

type VkResult = i32;
type VkInstance = *mut c_void;
type VkDevice = *mut c_void;
type VkPhysicalDevice = *mut c_void;
type VkQueue = *mut c_void;
type PfnVoid = Option<unsafe extern "system" fn()>;
type FnGipa = unsafe extern "system" fn(VkInstance, *const c_char) -> PfnVoid;
type FnGdpa = unsafe extern "system" fn(VkDevice, *const c_char) -> PfnVoid;
type FnPdpa = unsafe extern "system" fn(VkInstance, *const c_char) -> PfnVoid;
type FnQueuePresent = unsafe extern "system" fn(VkQueue, *const c_void) -> VkResult;
type FnCreateInstance =
    unsafe extern "system" fn(*const c_void, *const c_void, *mut VkInstance) -> VkResult;
type FnCreateDevice = unsafe extern "system" fn(
    VkPhysicalDevice,
    *const c_void,
    *const c_void,
    *mut VkDevice,
) -> VkResult;

/// Nullable loader proc-addr pointers (the C struct fields can be `NULL`).
type PfnGipa = Option<FnGipa>;
type PfnGdpa = Option<FnGdpa>;
type PfnPdpa = Option<FnPdpa>;

const VK_SUCCESS: VkResult = 0;
const VK_ERROR_INITIALIZATION_FAILED: VkResult = -3;

/// `VK_STRUCTURE_TYPE_LOADER_INSTANCE_CREATE_INFO`.
const LOADER_INSTANCE_CREATE_INFO: i32 = 47;
/// `VK_STRUCTURE_TYPE_LOADER_DEVICE_CREATE_INFO`.
const LOADER_DEVICE_CREATE_INFO: i32 = 48;
/// `VK_LAYER_LINK_INFO` (the `function` value carrying the link node).
const LAYER_LINK_INFO: i32 = 0;
/// `LAYER_NEGOTIATE_INTERFACE_STRUCT`.
const LAYER_NEGOTIATE_INTERFACE_STRUCT: i32 = 1;

/// The loader/layer interface version this layer implements.
const LAYER_INTERFACE_VERSION: u32 = 2;

const VK_CREATE_INSTANCE: &[u8] = b"vkCreateInstance\0";
const VK_CREATE_DEVICE: &[u8] = b"vkCreateDevice\0";
const VK_QUEUE_PRESENT: &[u8] = b"vkQueuePresentKHR\0";

// -- loader link structures (vk_layer.h) -------------------------------------

/// `VkLayerInstanceLink`: `pNext`, then the next entity's instance + physical
/// device proc-addrs.
#[repr(C)]
struct VkLayerInstanceLink {
    p_next: *mut VkLayerInstanceLink,
    next_gipa: PfnGipa,
    next_pdpa: PfnPdpa,
}

/// `VkLayerDeviceLink`: `pNext`, then the next entity's instance + device
/// proc-addrs.
#[repr(C)]
struct VkLayerDeviceLink {
    p_next: *mut VkLayerDeviceLink,
    next_gipa: PfnGipa,
    next_gdpa: PfnGdpa,
}

/// `VkNegotiateLayerInterface` (hand-declared; field order pinned to vk_layer.h).
#[repr(C)]
pub struct VkNegotiateLayerInterface {
    s_type: i32,
    p_next: *mut c_void,
    loader_layer_interface_version: u32,
    pfn_get_instance_proc_addr: PfnGipa,
    pfn_get_device_proc_addr: PfnGdpa,
    pfn_get_physical_device_proc_addr: PfnPdpa,
}

// -- global state ------------------------------------------------------------

static NEXT_GIPA: AtomicUsize = AtomicUsize::new(0);
static NEXT_GDPA: AtomicUsize = AtomicUsize::new(0);
static NEXT_PDPA: AtomicUsize = AtomicUsize::new(0);
static NEXT_PRESENT: AtomicUsize = AtomicUsize::new(0);
static GLOBAL_INSTANCE: AtomicUsize = AtomicUsize::new(0);
static RECORDER: Mutex<Option<LayerRecorder>> = Mutex::new(None);

/// Run `f`, converting a panic into `fallback` so nothing unwinds across FFI.
fn catch<T>(fallback: T, f: impl FnOnce() -> T) -> T {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or(fallback)
}

/// [`catch`] specialized to a `PFN_vkVoidFunction` (returns `NULL` on panic).
fn catch_opt(f: impl FnOnce() -> PfnVoid) -> PfnVoid {
    catch(None, f)
}

/// Store a function-pointer address (0 for `None`).
fn store_addr(a: &AtomicUsize, addr: usize) {
    a.store(addr, Ordering::Release);
}

/// Load a function pointer from the given signature, `None` when unset.
fn load_fn<T: Copy>(a: &AtomicUsize) -> Option<T> {
    let p = a.load(Ordering::Acquire);
    if p == 0 {
        return None;
    }
    // SAFETY: every value stored here is a valid entry point of signature `T`
    // (or the null address, rejected above).
    Some(unsafe { core::mem::transmute_copy::<usize, T>(&p) })
}

/// A `PFN_vkVoidFunction` for an address (always non-null here).
fn as_pfn_void(f: *const ()) -> PfnVoid {
    if f.is_null() {
        None
    } else {
        // SAFETY: `f` is a live entry point; the loader casts it back to the
        // signature it asked for.
        Some(unsafe { core::mem::transmute::<*const (), unsafe extern "system" fn()>(f) })
    }
}

/// The next entity's instance proc-addr, if captured.
fn next_gipa() -> Option<FnGipa> {
    load_fn(&NEXT_GIPA)
}

/// The next entity's device proc-addr, if captured.
fn next_gdpa() -> Option<FnGdpa> {
    load_fn(&NEXT_GDPA)
}

/// Resolve and cache the next entity's real `vkQueuePresentKHR` for an instance.
fn resolve_present_for_instance(instance: VkInstance) {
    if NEXT_PRESENT.load(Ordering::Acquire) != 0 {
        return;
    }
    let Some(gipa) = next_gipa() else {
        return;
    };
    // SAFETY: `instance` is a live instance handle; `gipa` is the next lookup.
    if let Some(p) = unsafe { gipa(instance, VK_QUEUE_PRESENT.as_ptr() as *const c_char) } {
        NEXT_PRESENT.store(p as usize, Ordering::Release);
    }
}

/// Resolve and cache the next entity's real `vkQueuePresentKHR` for a device,
/// falling back to the instance lookup.
fn resolve_present_for_device(device: VkDevice) {
    if NEXT_PRESENT.load(Ordering::Acquire) != 0 {
        return;
    }
    if let Some(gdpa) = next_gdpa() {
        // SAFETY: `device` is a live device handle; `gdpa` is the next lookup.
        if let Some(p) = unsafe { gdpa(device, VK_QUEUE_PRESENT.as_ptr() as *const c_char) } {
            NEXT_PRESENT.store(p as usize, Ordering::Release);
            return;
        }
    }
    resolve_present_for_instance(GLOBAL_INSTANCE.load(Ordering::Acquire) as VkInstance);
}

// -- loader chain walking ----------------------------------------------------

/// Walk a `VkInstanceCreateInfo` / `VkDeviceCreateInfo` `pNext` chain and return
/// the loader link-info node (the node whose `function == VK_LAYER_LINK_INFO`).
///
/// # Safety
/// `create_info` must point to a live Vulkan create-info struct with the
/// loader-provided `pNext` chain.
unsafe fn find_link_node(create_info: *const c_void, s_type: i32) -> Option<*mut c_void> {
    if create_info.is_null() {
        return None;
    }
    // Both create-info structs begin `sType: i32` then pointer-aligned `pNext`
    // at offset 8.
    // SAFETY: offset 8 of a Vulkan create-info struct is its `pNext`.
    let mut node = unsafe { core::ptr::read(create_info.add(8) as *const *const c_void) };
    while !node.is_null() {
        // SAFETY: every pNext chain node begins with `sType: i32`.
        let node_s_type = unsafe { core::ptr::read(node as *const i32) };
        if node_s_type == s_type {
            // SAFETY: the node's `function` is at offset 16 (sType, pad, pNext).
            let function = unsafe { core::ptr::read(node.add(16) as *const i32) };
            if function == LAYER_LINK_INFO {
                return Some(node as *mut c_void);
            }
        }
        // SAFETY: every pNext chain node's `pNext` is at offset 8.
        node = unsafe { core::ptr::read(node.add(8) as *const *const c_void) };
    }
    None
}

/// Read the link pointer (`union u`) of a link-info node.
///
/// # Safety
/// `node` must come from [`find_link_node`].
unsafe fn node_link(node: *mut c_void) -> *mut c_void {
    // SAFETY: the union `u` is at offset 24 in the link-info node.
    unsafe { core::ptr::read(node.add(24) as *const *mut c_void) }
}

/// Overwrite the link pointer of a link-info node (advance the chain).
///
/// # Safety
/// `node` must come from [`find_link_node`].
unsafe fn set_node_link(node: *mut c_void, link: *mut c_void) {
    // SAFETY: the union `u` is at offset 24 in the link-info node.
    unsafe { core::ptr::write(node.add(24) as *mut *mut c_void, link) };
}

// -- intercepted entry points ------------------------------------------------

/// `vkCreateInstance`: capture the next-chain proc-addrs, then forward.
///
/// # Safety
/// The standard `vkCreateInstance` pointer contract: live create-info and a
/// writable instance out-pointer (or null).
unsafe extern "system" fn mh_create_instance(
    create_info: *const c_void,
    allocator: *const c_void,
    instance: *mut VkInstance,
) -> VkResult {
    catch(VK_ERROR_INITIALIZATION_FAILED, || {
        let Some(node) = (unsafe { find_link_node(create_info, LOADER_INSTANCE_CREATE_INFO) })
        else {
            return VK_ERROR_INITIALIZATION_FAILED;
        };
        let link = unsafe { node_link(node) } as *mut VkLayerInstanceLink;
        if link.is_null() {
            return VK_ERROR_INITIALIZATION_FAILED;
        }
        // SAFETY: the loader-provided link node is valid.
        let (next_gipa, next_pdpa) = unsafe { ((*link).next_gipa, (*link).next_pdpa) };
        store_addr(&NEXT_GIPA, next_gipa.map(|f| f as usize).unwrap_or(0));
        store_addr(&NEXT_PDPA, next_pdpa.map(|f| f as usize).unwrap_or(0));
        // Advance the chain so the next entity sees its own link info.
        // SAFETY: `link` is the live link node.
        unsafe { set_node_link(node, (*link).p_next as *mut c_void) };

        let Some(gipa) = next_gipa else {
            return VK_ERROR_INITIALIZATION_FAILED;
        };
        // Resolve the next entity's `vkCreateInstance` through its proc-addr.
        // SAFETY: `gipa` is the next entity's lookup; NULL is valid for the
        // global `vkCreateInstance`.
        let create = unsafe {
            gipa(
                core::ptr::null_mut(),
                VK_CREATE_INSTANCE.as_ptr() as *const c_char,
            )
        };
        let Some(create) = create else {
            return VK_ERROR_INITIALIZATION_FAILED;
        };
        // SAFETY: `create` is the next entity's vkCreateInstance.
        let create: FnCreateInstance = unsafe { core::mem::transmute(create) };
        let result = unsafe { create(create_info, allocator, instance) };
        if result == VK_SUCCESS && !instance.is_null() {
            // SAFETY: `instance` is a valid out-pointer on success.
            let handle = unsafe { *instance };
            store_addr(&GLOBAL_INSTANCE, handle as usize);
        }
        result
    })
}

/// `vkCreateDevice`: capture the next device proc-addr, then forward.
///
/// # Safety
/// The standard `vkCreateDevice` pointer contract.
unsafe extern "system" fn mh_create_device(
    physical: VkPhysicalDevice,
    create_info: *const c_void,
    allocator: *const c_void,
    device: *mut VkDevice,
) -> VkResult {
    catch(VK_ERROR_INITIALIZATION_FAILED, || {
        let Some(node) = (unsafe { find_link_node(create_info, LOADER_DEVICE_CREATE_INFO) }) else {
            return VK_ERROR_INITIALIZATION_FAILED;
        };
        let link = unsafe { node_link(node) } as *mut VkLayerDeviceLink;
        if link.is_null() {
            return VK_ERROR_INITIALIZATION_FAILED;
        }
        // SAFETY: the loader-provided device link node is valid.
        let (next_gipa, next_gdpa) = unsafe { ((*link).next_gipa, (*link).next_gdpa) };
        store_addr(&NEXT_GIPA, next_gipa.map(|f| f as usize).unwrap_or(0));
        store_addr(&NEXT_GDPA, next_gdpa.map(|f| f as usize).unwrap_or(0));
        // SAFETY: `link` is the live device link node.
        unsafe { set_node_link(node, (*link).p_next as *mut c_void) };

        let Some(gipa) = next_gipa else {
            return VK_ERROR_INITIALIZATION_FAILED;
        };
        let instance = GLOBAL_INSTANCE.load(Ordering::Acquire) as VkInstance;
        // SAFETY: `gipa` is the next entity's lookup; instance is live.
        let create = unsafe { gipa(instance, VK_CREATE_DEVICE.as_ptr() as *const c_char) };
        let Some(create) = create else {
            return VK_ERROR_INITIALIZATION_FAILED;
        };
        // SAFETY: `create` is the next entity's vkCreateDevice.
        let create: FnCreateDevice = unsafe { core::mem::transmute(create) };
        let result = unsafe { create(physical, create_info, allocator, device) };
        if result == VK_SUCCESS && !device.is_null() {
            // SAFETY: `device` is a valid out-pointer on success.
            resolve_present_for_device(unsafe { *device });
        }
        result
    })
}

/// The frame-recording `vkQueuePresentKHR` detour: record one timestamp, then
/// forward. Never panics across the boundary.
///
/// # Safety
/// The standard `vkQueuePresentKHR` pointer contract.
unsafe extern "system" fn mh_queue_present(
    queue: VkQueue,
    present_info: *const c_void,
) -> VkResult {
    catch(VK_SUCCESS, || {
        // Record one QPC timestamp. Fail-open: a contended lock, a mapping
        // failure, or a bad block just drops the sample; the present forwards.
        // `try_lock` (not `lock`) so a held lock — another present thread, or the
        // lazy `open` on a slow mapping — never stalls the app's present thread.
        if let Ok(mut guard) = RECORDER.try_lock() {
            if guard.is_none() {
                *guard = LayerRecorder::open(std::process::id());
            }
            if let Some(rec) = guard.as_mut() {
                rec.record_present(ring::qpc_now());
            }
        }

        let p = NEXT_PRESENT.load(Ordering::Acquire);
        if p == 0 {
            return VK_SUCCESS;
        }
        // SAFETY: `p` is the next entity's vkQueuePresentKHR.
        let f: FnQueuePresent = unsafe { core::mem::transmute(p) };
        unsafe { f(queue, present_info) }
    })
}

/// Collect a physical-device proc-addr by forwarding down the chain.
///
/// # Safety
/// The `vk_layerGetPhysicalDeviceProcAddr` contract.
unsafe extern "system" fn mh_get_physical_device_proc_addr(
    instance: VkInstance,
    name: *const c_char,
) -> PfnVoid {
    catch_opt(|| load_fn::<FnPdpa>(&NEXT_PDPA).and_then(|f| unsafe { f(instance, name) }))
}

// -- exported layer interface ------------------------------------------------

/// The layer's `vkGetInstanceProcAddr`.
///
/// # Safety
/// Standard loader contract: `name` is a NUL-terminated entry-point name (or null).
#[no_mangle]
pub unsafe extern "system" fn vkGetInstanceProcAddr(
    instance: VkInstance,
    name: *const c_char,
) -> PfnVoid {
    catch_opt(|| {
        if name.is_null() {
            return None;
        }
        // SAFETY: the loader passes a NUL-terminated entry-point name.
        let Ok(name_str) = (unsafe { CStr::from_ptr(name) }).to_str() else {
            return None;
        };
        match proc_kind(name_str) {
            Proc::CreateInstance => as_pfn_void(mh_create_instance as *const ()),
            Proc::CreateDevice => as_pfn_void(mh_create_device as *const ()),
            Proc::GetInstanceProcAddr => as_pfn_void(vkGetInstanceProcAddr as *const ()),
            Proc::GetDeviceProcAddr => as_pfn_void(vkGetDeviceProcAddr as *const ()),
            Proc::GetPhysicalDeviceProcAddr => {
                as_pfn_void(mh_get_physical_device_proc_addr as *const ())
            }
            Proc::QueuePresent => {
                resolve_present_for_instance(instance);
                as_pfn_void(mh_queue_present as *const ())
            }
            Proc::Forward => next_gipa().and_then(|f| unsafe { f(instance, name) }),
        }
    })
}

/// The layer's `vkGetDeviceProcAddr`.
///
/// # Safety
/// Standard loader contract: `name` is a NUL-terminated entry-point name (or null).
#[no_mangle]
pub unsafe extern "system" fn vkGetDeviceProcAddr(
    device: VkDevice,
    name: *const c_char,
) -> PfnVoid {
    catch_opt(|| {
        if name.is_null() {
            return None;
        }
        // SAFETY: the loader passes a NUL-terminated entry-point name.
        let Ok(name_str) = (unsafe { CStr::from_ptr(name) }).to_str() else {
            return None;
        };
        match proc_kind(name_str) {
            Proc::GetInstanceProcAddr => as_pfn_void(vkGetInstanceProcAddr as *const ()),
            Proc::GetDeviceProcAddr => as_pfn_void(vkGetDeviceProcAddr as *const ()),
            Proc::GetPhysicalDeviceProcAddr => {
                as_pfn_void(mh_get_physical_device_proc_addr as *const ())
            }
            Proc::QueuePresent => {
                resolve_present_for_device(device);
                as_pfn_void(mh_queue_present as *const ())
            }
            Proc::Forward => {
                if let Some(f) = next_gdpa() {
                    unsafe { f(device, name) }
                } else {
                    let instance = GLOBAL_INSTANCE.load(Ordering::Acquire) as VkInstance;
                    next_gipa().and_then(|f| unsafe { f(instance, name) })
                }
            }
            Proc::CreateInstance | Proc::CreateDevice => None,
        }
    })
}

/// Layer/loader version negotiation, called by the loader before any other
/// entry point. It must not call down the chain.
///
/// # Safety
/// `iface` must be the loader-constructed `VkNegotiateLayerInterface` (or null).
#[no_mangle]
pub unsafe extern "system" fn vkNegotiateLoaderLayerInterfaceVersion(
    iface: *mut VkNegotiateLayerInterface,
) -> VkResult {
    catch(VK_ERROR_INITIALIZATION_FAILED, || {
        if iface.is_null() {
            return VK_ERROR_INITIALIZATION_FAILED;
        }
        // SAFETY: non-null, loader-provided, valid for the duration of the call.
        let iface = unsafe { &mut *iface };
        if iface.s_type != LAYER_NEGOTIATE_INTERFACE_STRUCT {
            return VK_ERROR_INITIALIZATION_FAILED;
        }
        // Report the interface version this layer implements (never newer than
        // the loader's request).
        if iface.loader_layer_interface_version > LAYER_INTERFACE_VERSION {
            iface.loader_layer_interface_version = LAYER_INTERFACE_VERSION;
        }
        iface.pfn_get_instance_proc_addr = Some(vkGetInstanceProcAddr);
        iface.pfn_get_device_proc_addr = Some(vkGetDeviceProcAddr);
        iface.pfn_get_physical_device_proc_addr = Some(mh_get_physical_device_proc_addr);
        VK_SUCCESS
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    static FORWARDED: AtomicUsize = AtomicUsize::new(0);
    /// Calls into the fake next-entity acquisition entry points.
    static CREATE_INSTANCE_CALLS: AtomicUsize = AtomicUsize::new(0);
    static CREATE_DEVICE_CALLS: AtomicUsize = AtomicUsize::new(0);
    /// `NEXT_*` / `GLOBAL_INSTANCE` / `RECORDER` are process-global in a real
    /// process (the loader owns the chain). Every test here shares one address
    /// space, so tests that read or write them take this lock.
    static GLOBAL_LOCK: Mutex<()> = Mutex::new(());

    fn serial() -> std::sync::MutexGuard<'static, ()> {
        GLOBAL_LOCK.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn cstr(bytes: &[u8]) -> *const c_char {
        bytes.as_ptr() as *const c_char
    }

    /// Address of an optional function pointer. Comparing function pointers
    /// with `==` is warned against (their addresses are not unique across
    /// codegen units); comparing addresses is the sanctioned check.
    fn fn_addr(f: PfnVoid) -> Option<usize> {
        f.map(|f| f as usize)
    }

    /// A fake "next entity" present that counts its calls.
    unsafe extern "system" fn fake_next(_queue: VkQueue, _info: *const c_void) -> VkResult {
        FORWARDED.fetch_add(1, Ordering::AcqRel);
        VK_SUCCESS
    }

    /// The sentinel a fake instance lookup resolves `vkQueuePresentKHR` to.
    const PRESENT_SENTINEL: usize = 0x1111_2222;
    /// The sentinel a fake device lookup resolves `vkQueuePresentKHR` to.
    const DEVICE_PRESENT_SENTINEL: usize = 0x3333_4444;

    /// A fake next-entity `vkGetInstanceProcAddr` that resolves every name to
    /// [`PRESENT_SENTINEL`], so a capture test can assert what was stored.
    unsafe extern "system" fn fake_gipa(_instance: VkInstance, _name: *const c_char) -> PfnVoid {
        as_pfn_void(PRESENT_SENTINEL as *const ())
    }

    /// A fake next-entity `vkGetDeviceProcAddr` (resolves to the device
    /// sentinel).
    unsafe extern "system" fn fake_gdpa(_device: VkDevice, _name: *const c_char) -> PfnVoid {
        as_pfn_void(DEVICE_PRESENT_SENTINEL as *const ())
    }

    /// A fake next-entity `vkCreateInstance` that stores the instance it is
    /// handed and reports success.
    unsafe extern "system" fn fake_create_instance(
        _ci: *const c_void,
        _alloc: *const c_void,
        instance: *mut VkInstance,
    ) -> VkResult {
        CREATE_INSTANCE_CALLS.fetch_add(1, Ordering::AcqRel);
        if !instance.is_null() {
            // SAFETY: the layer passes a live out-pointer per the ABI.
            unsafe { *instance = 0xABCD as VkInstance };
        }
        VK_SUCCESS
    }

    /// A fake next-entity `vkGetInstanceProcAddr` resolving `vkCreateInstance`.
    unsafe extern "system" fn gipa_returns_create(
        _instance: VkInstance,
        _name: *const c_char,
    ) -> PfnVoid {
        as_pfn_void(fake_create_instance as *const ())
    }

    /// A fake next-entity `vkCreateDevice` that stores the device it is handed.
    unsafe extern "system" fn fake_create_device(
        _pd: VkPhysicalDevice,
        _ci: *const c_void,
        _alloc: *const c_void,
        device: *mut VkDevice,
    ) -> VkResult {
        CREATE_DEVICE_CALLS.fetch_add(1, Ordering::AcqRel);
        if !device.is_null() {
            // SAFETY: the layer passes a live out-pointer per the ABI.
            unsafe { *device = 0xFFFF as VkDevice };
        }
        VK_SUCCESS
    }

    /// A fake next-entity `vkGetInstanceProcAddr` resolving `vkCreateDevice`.
    unsafe extern "system" fn gipa_returns_create_device(
        _instance: VkInstance,
        _name: *const c_char,
    ) -> PfnVoid {
        as_pfn_void(fake_create_device as *const ())
    }

    /// Mirror of the loader's `VkLayerInstanceCreateInfo`/`Device` link node
    /// (`sType`, pad, `pNext`, `function`, pad, `union u`) so a test can build a
    /// real `pNext` chain on the stack.
    #[repr(C)]
    struct FakeNode {
        s_type: i32,
        _s_type_pad: i32,
        p_next: *const c_void,
        function: i32,
        _function_pad: i32,
        link: *mut c_void,
    }

    /// Mirror of the leading fields of a Vulkan create-info struct (`sType`,
    /// pad, `pNext`) — everything `find_link_node` reads from the head.
    #[repr(C)]
    struct FakeCreateInfo {
        s_type: i32,
        _pad: i32,
        p_next: *const c_void,
    }

    fn node(s_type: i32, function: i32, link: *mut c_void, p_next: *const c_void) -> FakeNode {
        FakeNode {
            s_type,
            _s_type_pad: 0,
            p_next,
            function,
            _function_pad: 0,
            link,
        }
    }

    fn create_info(p_next: *const c_void) -> FakeCreateInfo {
        FakeCreateInfo {
            s_type: 0,
            _pad: 0,
            p_next,
        }
    }

    fn iface(s_type: i32, version: u32) -> VkNegotiateLayerInterface {
        VkNegotiateLayerInterface {
            s_type,
            p_next: core::ptr::null_mut(),
            loader_layer_interface_version: version,
            pfn_get_instance_proc_addr: None,
            pfn_get_device_proc_addr: None,
            pfn_get_physical_device_proc_addr: None,
        }
    }

    #[test]
    fn present_hot_path_does_not_block_on_a_contended_recorder_lock() {
        // Catches: a BLOCKING mutex on the present hot path. `mh_queue_present`
        // runs on the app's present thread; if the recorder lock is held (another
        // present, or the lazy `open` on a slow mapping) a blocking `lock()`
        // stalls the frame. It must `try_lock` and forward regardless.
        let _g = serial();
        NEXT_PRESENT.store(fake_next as *const () as usize, Ordering::Release);
        let before = FORWARDED.load(Ordering::Acquire);
        let hold = RECORDER.lock().unwrap_or_else(|e| e.into_inner());
        let (tx, rx) = std::sync::mpsc::channel();
        let handle = std::thread::spawn(move || {
            // SAFETY: null queue/info are forwarded verbatim to the fake.
            unsafe { mh_queue_present(core::ptr::null_mut(), core::ptr::null()) };
            let _ = tx.send(());
        });
        assert!(
            rx.recv_timeout(std::time::Duration::from_secs(5)).is_ok(),
            "the present detour blocked on a contended recorder lock"
        );
        handle.join().expect("present thread");
        assert_eq!(
            FORWARDED.load(Ordering::Acquire),
            before + 1,
            "the present must still forward while the lock is held"
        );
        drop(hold);
        NEXT_PRESENT.store(0, Ordering::Release);
    }

    #[test]
    fn present_detour_records_a_frame_and_forwards() {
        // Catches the two ways the detour could silently break capture:
        // forgetting to record (a dynamic app would read 0 fps) or forgetting to
        // forward (the app would stop presenting). Exercises the real detour.
        let _g = serial();
        let pid = std::process::id();
        NEXT_PRESENT.store(fake_next as *const () as usize, Ordering::Release);
        let before = FORWARDED.load(Ordering::Acquire);

        // SAFETY: a null queue/info are ignored by the detour (it forwards them
        // verbatim to the fake, which also ignores them).
        unsafe { mh_queue_present(core::ptr::null_mut(), core::ptr::null()) };

        assert_eq!(
            FORWARDED.load(Ordering::Acquire),
            before + 1,
            "the present must be forwarded exactly once"
        );
        let view = hook_ipc::FrameMapping::open(pid).expect("the detour created the ring");
        let r = hook_ipc::RingReader::new(view.as_slice()).expect("attach");
        let latest = r.read_latest().expect("a recorded frame");
        assert_eq!(latest.api, hook_ipc::Api::VkQueuePresentKHR);
        assert!(r.header().next_seq >= 1);

        NEXT_PRESENT.store(0, Ordering::Release);
    }

    #[test]
    fn negotiation_accepts_the_loader_struct_and_installs_the_local_entry_points() {
        // Catches: rejecting the loader's negotiation (the layer never loads, so
        // a fully-dynamic app is never captured) or advertising a proc-addr it
        // did not install (the loader then routes every lookup through NULL).
        let _g = serial();
        let mut iface = iface(LAYER_NEGOTIATE_INTERFACE_STRUCT, LAYER_INTERFACE_VERSION);
        // SAFETY: `iface` is a live, correctly-shaped negotiate struct.
        let r = unsafe { vkNegotiateLoaderLayerInterfaceVersion(&mut iface) };
        assert_eq!(r, VK_SUCCESS);
        assert_eq!(
            iface.loader_layer_interface_version,
            LAYER_INTERFACE_VERSION
        );
        assert_eq!(
            iface.pfn_get_instance_proc_addr.map(|f| f as usize),
            Some(vkGetInstanceProcAddr as *const () as usize)
        );
        assert_eq!(
            iface.pfn_get_device_proc_addr.map(|f| f as usize),
            Some(vkGetDeviceProcAddr as *const () as usize)
        );
        assert_eq!(
            iface.pfn_get_physical_device_proc_addr.map(|f| f as usize),
            Some(mh_get_physical_device_proc_addr as *const () as usize)
        );
    }

    #[test]
    fn negotiation_clamps_a_newer_loader_version() {
        // Catches: echoing back an interface version newer than this layer
        // implements — the loader may then call entry points that do not exist.
        let _g = serial();
        let mut iface = iface(LAYER_NEGOTIATE_INTERFACE_STRUCT, 99);
        let r = unsafe { vkNegotiateLoaderLayerInterfaceVersion(&mut iface) };
        assert_eq!(r, VK_SUCCESS);
        assert_eq!(
            iface.loader_layer_interface_version,
            LAYER_INTERFACE_VERSION
        );
    }

    #[test]
    fn negotiation_accepts_an_older_interface_version_without_bumping_it() {
        // Catches: advertising a version NEWER than the loader asked for. An
        // older request (v1) must be echoed unchanged, with the local entry
        // points installed so the loader can still route through the layer.
        let _g = serial();
        let mut iface = iface(LAYER_NEGOTIATE_INTERFACE_STRUCT, 1);
        let r = unsafe { vkNegotiateLoaderLayerInterfaceVersion(&mut iface) };
        assert_eq!(r, VK_SUCCESS);
        assert_eq!(
            iface.loader_layer_interface_version, 1,
            "an older loader version must be echoed, never bumped"
        );
        assert!(iface.pfn_get_instance_proc_addr.is_some());
        assert!(iface.pfn_get_device_proc_addr.is_some());
    }

    #[test]
    fn negotiation_rejects_a_foreign_struct_or_null() {
        // Catches: accepting a struct that is not the negotiate struct (the
        // layer would write function pointers at the wrong offsets) or a null.
        let _g = serial();
        let mut wrong = iface(1234, LAYER_INTERFACE_VERSION);
        assert_eq!(
            unsafe { vkNegotiateLoaderLayerInterfaceVersion(&mut wrong) },
            VK_ERROR_INITIALIZATION_FAILED
        );
        assert!(
            wrong.pfn_get_instance_proc_addr.is_none(),
            "a rejected struct is left untouched"
        );
        assert_eq!(
            unsafe { vkNegotiateLoaderLayerInterfaceVersion(core::ptr::null_mut()) },
            VK_ERROR_INITIALIZATION_FAILED
        );
    }

    #[test]
    fn find_link_node_skips_unrelated_nodes_and_finds_the_link_info() {
        // Catches: matching a node whose `function` is not VK_LAYER_LINK_INFO
        // (returning a node whose union is not a link → `node_link` reads a
        // garbage pointer the layer then dereferences) and using the wrong
        // pNext offset (the scan never advances, so capture silently never
        // installs).
        let _g = serial();
        let mut link = node(
            LOADER_INSTANCE_CREATE_INFO,
            LAYER_LINK_INFO,
            0xCAFE as *mut c_void,
            core::ptr::null(),
        );
        let unrelated = node(
            99,
            99,
            core::ptr::null_mut(),
            &link as *const _ as *const c_void,
        );
        let ci = create_info(&unrelated as *const _ as *const c_void);
        assert_eq!(
            unsafe {
                find_link_node(
                    &ci as *const _ as *const c_void,
                    LOADER_INSTANCE_CREATE_INFO,
                )
            },
            Some(&mut link as *mut FakeNode as *mut c_void),
        );
    }

    #[test]
    fn find_link_node_returns_none_without_a_link_info_node() {
        // Catches: accepting a node with the right sType but the wrong
        // `function` value, and crashing on a null create-info instead of
        // failing open.
        let _g = serial();
        let empty = create_info(core::ptr::null());
        assert_eq!(
            unsafe {
                find_link_node(
                    &empty as *const _ as *const c_void,
                    LOADER_INSTANCE_CREATE_INFO,
                )
            },
            None
        );
        let wrong_fn = node(
            LOADER_INSTANCE_CREATE_INFO,
            7,
            core::ptr::null_mut(),
            core::ptr::null(),
        );
        let ci = create_info(&wrong_fn as *const _ as *const c_void);
        assert_eq!(
            unsafe {
                find_link_node(
                    &ci as *const _ as *const c_void,
                    LOADER_INSTANCE_CREATE_INFO,
                )
            },
            None
        );
        assert_eq!(
            unsafe { find_link_node(core::ptr::null(), LOADER_INSTANCE_CREATE_INFO) },
            None
        );
    }

    #[test]
    fn node_link_reads_and_writes_the_union_offset() {
        // Catches: using the wrong offset for the link node's union `u` —
        // advancing the chain would then corrupt an unrelated field of the
        // loader's struct.
        let _g = serial();
        let mut n = node(
            LOADER_INSTANCE_CREATE_INFO,
            LAYER_LINK_INFO,
            0x1234 as *mut c_void,
            core::ptr::null(),
        );
        let p = &mut n as *mut FakeNode as *mut c_void;
        assert_eq!(unsafe { node_link(p) } as usize, 0x1234);
        unsafe { set_node_link(p, 0x5678 as *mut c_void) };
        assert_eq!(n.link as usize, 0x5678);
    }

    #[test]
    fn create_instance_captures_the_next_chain_and_advances_the_link_node() {
        // Catches a broken chain walk in the layer's `vkCreateInstance`: if the
        // next entity's instance proc-addrs are not captured, every later lookup
        // forwards into nothing; if the link node is not advanced, the next
        // entity sees *our* link info again (recursion / wrong forwarding); and a
        // successful instance must be remembered for the device-level fallback.
        let _g = serial();
        NEXT_GIPA.store(0, Ordering::Release);
        NEXT_PDPA.store(0, Ordering::Release);
        GLOBAL_INSTANCE.store(0, Ordering::Release);
        CREATE_INSTANCE_CALLS.store(0, Ordering::Release);

        let mut next_link = VkLayerInstanceLink {
            p_next: 0xDEAD as *mut VkLayerInstanceLink,
            next_gipa: Some(gipa_returns_create),
            next_pdpa: None,
        };
        let n = node(
            LOADER_INSTANCE_CREATE_INFO,
            LAYER_LINK_INFO,
            &mut next_link as *mut VkLayerInstanceLink as *mut c_void,
            core::ptr::null(),
        );
        let ci = create_info(&n as *const FakeNode as *const c_void);
        let mut out: VkInstance = core::ptr::null_mut();
        // SAFETY: a lab-built create-info with a valid loader-shaped link node.
        let r = unsafe {
            mh_create_instance(
                &ci as *const FakeCreateInfo as *const c_void,
                core::ptr::null(),
                &mut out,
            )
        };
        assert_eq!(r, VK_SUCCESS);
        assert_eq!(
            CREATE_INSTANCE_CALLS.load(Ordering::Acquire),
            1,
            "the next entity's vkCreateInstance must be called exactly once"
        );
        assert_eq!(
            NEXT_GIPA.load(Ordering::Acquire),
            gipa_returns_create as *const () as usize,
            "the next instance proc-addr must be captured"
        );
        assert_eq!(
            n.link as usize, 0xDEAD,
            "the link node must advance to the next entity's link"
        );
        assert_eq!(out as usize, 0xABCD);
        assert_eq!(
            GLOBAL_INSTANCE.load(Ordering::Acquire),
            0xABCD,
            "a successful instance is remembered for the device fallback"
        );
    }

    #[test]
    fn create_device_captures_the_next_chain_and_advances_the_link_node() {
        // Same invariants on the device path: capture the next device proc-addr,
        // advance the link node, and resolve the real present through it so the
        // detour can forward.
        let _g = serial();
        NEXT_GIPA.store(0, Ordering::Release);
        NEXT_GDPA.store(0, Ordering::Release);
        NEXT_PRESENT.store(0, Ordering::Release);
        GLOBAL_INSTANCE.store(0xBEEF, Ordering::Release);
        CREATE_DEVICE_CALLS.store(0, Ordering::Release);

        let mut next_link = VkLayerDeviceLink {
            p_next: 0xCAFE as *mut VkLayerDeviceLink,
            next_gipa: Some(gipa_returns_create_device),
            next_gdpa: Some(fake_gdpa),
        };
        let n = node(
            LOADER_DEVICE_CREATE_INFO,
            LAYER_LINK_INFO,
            &mut next_link as *mut VkLayerDeviceLink as *mut c_void,
            core::ptr::null(),
        );
        let ci = create_info(&n as *const FakeNode as *const c_void);
        let mut dev: VkDevice = core::ptr::null_mut();
        // SAFETY: a lab-built device create-info with a valid link node.
        let r = unsafe {
            mh_create_device(
                core::ptr::null_mut(),
                &ci as *const FakeCreateInfo as *const c_void,
                core::ptr::null(),
                &mut dev,
            )
        };
        assert_eq!(r, VK_SUCCESS);
        assert_eq!(
            CREATE_DEVICE_CALLS.load(Ordering::Acquire),
            1,
            "the next entity's vkCreateDevice must be called exactly once"
        );
        assert_eq!(
            NEXT_GIPA.load(Ordering::Acquire),
            gipa_returns_create_device as *const () as usize
        );
        assert_eq!(
            NEXT_GDPA.load(Ordering::Acquire),
            fake_gdpa as *const () as usize,
            "the next device proc-addr must be captured"
        );
        assert_eq!(
            n.link as usize, 0xCAFE,
            "the link node must advance to the next entity's link"
        );
        assert_eq!(dev as usize, 0xFFFF);
        assert_eq!(
            NEXT_PRESENT.load(Ordering::Acquire),
            DEVICE_PRESENT_SENTINEL,
            "the present must be resolved through the captured device lookup"
        );
    }

    #[test]
    fn instance_proc_addr_maps_every_intercepted_name_to_its_local_entry_point() {
        // Catches: returning the wrong function for a name — the loader routes
        // the app's call into the wrong entry point, and in particular a present
        // not routed to `mh_queue_present` records nothing.
        let _g = serial();
        let q = core::ptr::null_mut();
        // SAFETY: every name is NUL-terminated; the loader contract is mirrored.
        unsafe {
            assert_eq!(
                fn_addr(vkGetInstanceProcAddr(q, cstr(b"vkCreateInstance\0"))),
                Some(mh_create_instance as *const () as usize)
            );
            assert_eq!(
                fn_addr(vkGetInstanceProcAddr(q, cstr(b"vkCreateDevice\0"))),
                Some(mh_create_device as *const () as usize)
            );
            assert_eq!(
                fn_addr(vkGetInstanceProcAddr(q, cstr(b"vkGetInstanceProcAddr\0"))),
                Some(vkGetInstanceProcAddr as *const () as usize)
            );
            assert_eq!(
                fn_addr(vkGetInstanceProcAddr(q, cstr(b"vkGetDeviceProcAddr\0"))),
                Some(vkGetDeviceProcAddr as *const () as usize)
            );
            assert_eq!(
                fn_addr(vkGetInstanceProcAddr(
                    q,
                    cstr(b"vk_layerGetPhysicalDeviceProcAddr\0")
                )),
                Some(mh_get_physical_device_proc_addr as *const () as usize)
            );
            assert_eq!(
                fn_addr(vkGetInstanceProcAddr(q, cstr(b"vkQueuePresentKHR\0"))),
                Some(mh_queue_present as *const () as usize)
            );
            assert_eq!(
                fn_addr(vkGetDeviceProcAddr(q, cstr(b"vkQueuePresentKHR\0"))),
                Some(mh_queue_present as *const () as usize)
            );
        }
    }

    #[test]
    fn proc_addr_lookups_fail_open_for_unknown_null_and_unavailable_forwards() {
        // Catches: resolving an unknown name locally (shadowing a real entry
        // point), crashing on a null name, answering an instance-creation entry
        // point from the device lookup, and returning a dangling pointer for a
        // passthrough when no next entity was captured.
        let _g = serial();
        let q = core::ptr::null_mut();
        // SAFETY: names are NUL-terminated; null is the documented "no name".
        unsafe {
            assert!(vkGetInstanceProcAddr(q, cstr(b"vkQueueSubmit\0")).is_none());
            assert!(vkGetInstanceProcAddr(q, core::ptr::null()).is_none());
            assert!(vkGetDeviceProcAddr(q, cstr(b"vkCreateInstance\0")).is_none());
            assert!(vkGetDeviceProcAddr(q, cstr(b"vkCreateDevice\0")).is_none());
            assert!(vkGetDeviceProcAddr(q, cstr(b"vkQueueSubmit\0")).is_none());
            assert!(vkGetDeviceProcAddr(q, core::ptr::null()).is_none());
        }
    }

    #[test]
    fn resolve_present_for_instance_caches_the_next_entities_present_once() {
        // Catches: the capture never storing the next entity's pointer (the
        // detour then forwards to nothing and swallows every present) and
        // re-resolving/overwriting a good pointer on each lookup.
        let _g = serial();
        let gipa: FnGipa = fake_gipa;
        NEXT_GIPA.store(gipa as usize, Ordering::Release);
        NEXT_PRESENT.store(0, Ordering::Release);
        resolve_present_for_instance(core::ptr::null_mut());
        assert_eq!(
            NEXT_PRESENT.load(Ordering::Acquire),
            PRESENT_SENTINEL,
            "the next entity's present is captured"
        );

        NEXT_PRESENT.store(DEVICE_PRESENT_SENTINEL, Ordering::Release);
        resolve_present_for_instance(core::ptr::null_mut());
        assert_eq!(
            NEXT_PRESENT.load(Ordering::Acquire),
            DEVICE_PRESENT_SENTINEL,
            "a cached present is not re-resolved"
        );

        NEXT_GIPA.store(0, Ordering::Release);
        NEXT_PRESENT.store(0, Ordering::Release);
    }

    #[test]
    fn resolve_present_for_device_prefers_the_device_lookup_then_falls_back() {
        // Catches: ignoring a captured device proc-addr (falling back to an
        // instance lookup that cannot see device-level present) and ignoring the
        // instance fallback when no device lookup was captured.
        let _g = serial();
        let gdpa: FnGdpa = fake_gdpa;
        NEXT_GDPA.store(gdpa as usize, Ordering::Release);
        NEXT_GIPA.store(0, Ordering::Release);
        NEXT_PRESENT.store(0, Ordering::Release);
        resolve_present_for_device(core::ptr::null_mut());
        assert_eq!(
            NEXT_PRESENT.load(Ordering::Acquire),
            DEVICE_PRESENT_SENTINEL,
            "the device lookup wins when captured"
        );

        NEXT_GDPA.store(0, Ordering::Release);
        let gipa: FnGipa = fake_gipa;
        NEXT_GIPA.store(gipa as usize, Ordering::Release);
        GLOBAL_INSTANCE.store(0xBEEF, Ordering::Release);
        NEXT_PRESENT.store(0, Ordering::Release);
        resolve_present_for_device(core::ptr::null_mut());
        assert_eq!(
            NEXT_PRESENT.load(Ordering::Acquire),
            PRESENT_SENTINEL,
            "falls back to the instance lookup"
        );

        NEXT_GIPA.store(0, Ordering::Release);
        GLOBAL_INSTANCE.store(0, Ordering::Release);
        NEXT_PRESENT.store(0, Ordering::Release);
    }
}
