//! The exact hook set: every entry point, its vtable index, and its source.
//!
//! Indices are copied from the research project's VERIFIED table
//! (`research/docs/07-verified-facts.md` §1/§1.1, cross-checked against the
//! mingw-w64 headers). The tables here are the single source of truth the
//! installer and the unit tests both read, so a transposed index is caught.

use hook_ipc::Api;

/// Where an entry point lives.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HookKind {
    /// A COM vtable slot: `index` into the object's vtable.
    Vtable { index: usize },
    /// An import-address-table entry: `dll` exporting `func`.
    Iat {
        dll: &'static str,
        func: &'static str,
    },
}

/// One hook entry point. The function name is carried in the table comments.
#[derive(Clone, Copy, Debug)]
pub struct HookSpec {
    pub api: Api,
    pub kind: HookKind,
}

const fn vtable(api: Api, index: usize) -> HookSpec {
    HookSpec {
        api,
        kind: HookKind::Vtable { index },
    }
}

const fn iat(api: Api, dll: &'static str, func: &'static str) -> HookSpec {
    HookSpec {
        api,
        kind: HookKind::Iat { dll, func },
    }
}

/// COM vtable hooks (present/swap/resize + swapchain creation).
pub static VTABLE_HOOKS: &[HookSpec] = &[
    // DXGI swapchain (docs/07 §1). SetFullscreenState=10 / ResizeBuffers=13
    // follow from the IDXGISwapChain method order (Present=8, GetBuffer=9,
    // SetFullscreenState=10, GetFullscreenState=11, GetDesc=12,
    // ResizeBuffers=13), the same source that pins Present=8.
    vtable(Api::DxgiPresent, 8),             // IDXGISwapChain::Present
    vtable(Api::DxgiPresent1, 22),           // IDXGISwapChain1::Present1
    vtable(Api::DxgiSetFullscreenState, 10), // IDXGISwapChain::SetFullscreenState
    vtable(Api::DxgiResizeBuffers, 13),      // IDXGISwapChain::ResizeBuffers
    // DXGI factory acquisition (docs/07 §1.1).
    vtable(Api::DxgiCreateSwapChain, 10), // IDXGIFactory::CreateSwapChain
    vtable(Api::DxgiCreateSwapChainForHwnd, 15), // IDXGIFactory2::CreateSwapChainForHwnd
    vtable(Api::DxgiCreateSwapChainForCoreWindow, 16), // IDXGIFactory2::CreateSwapChainForCoreWindow
    vtable(Api::DxgiCreateSwapChainForComposition, 24), // IDXGIFactory2::CreateSwapChainForComposition
    // D3D9 device present/end + Ex (docs/07 §1).
    vtable(Api::D3d9Present, 17),    // IDirect3DDevice9::Present
    vtable(Api::D3d9EndScene, 42),   // IDirect3DDevice9::EndScene
    vtable(Api::D3d9PresentEx, 121), // IDirect3DDevice9Ex::PresentEx
    vtable(Api::D3d9ResetEx, 132),   // IDirect3DDevice9Ex::ResetEx
    // D3D9 acquisition (docs/07 §1.1).
    vtable(Api::D3d9CreateDevice, 16),   // IDirect3D9::CreateDevice
    vtable(Api::D3d9CreateDeviceEx, 20), // IDirect3D9Ex::CreateDeviceEx
    // D3D12 has no device Present: the swapchain's command queue is hooked.
    vtable(Api::D3d12ExecuteCommandLists, 10), // ID3D12CommandQueue::ExecuteCommandLists
];

/// Import-address-table hooks (creation exports + GL/Vulkan swap exports).
pub static IAT_HOOKS: &[HookSpec] = &[
    iat(Api::DxgiCreateSwapChain, "dxgi.dll", "CreateDXGIFactory"),
    iat(Api::DxgiCreateSwapChain, "dxgi.dll", "CreateDXGIFactory1"),
    iat(Api::DxgiCreateSwapChain, "dxgi.dll", "CreateDXGIFactory2"),
    iat(
        Api::DxgiCreateSwapChain,
        "d3d11.dll",
        "D3D11CreateDeviceAndSwapChain",
    ),
    iat(Api::D3d9CreateDevice, "d3d9.dll", "Direct3DCreate9"),
    iat(Api::D3d9CreateDeviceEx, "d3d9.dll", "Direct3DCreate9Ex"),
    iat(Api::WglSwapBuffers, "opengl32.dll", "wglSwapBuffers"),
    iat(
        Api::WglSwapLayerBuffers,
        "opengl32.dll",
        "wglSwapLayerBuffers",
    ),
    iat(Api::GdiSwapBuffers, "gdi32.dll", "SwapBuffers"),
    iat(Api::EglSwapBuffers, "libEGL.dll", "eglSwapBuffers"),
    iat(Api::VkQueuePresentKHR, "vulkan-1.dll", "vkQueuePresentKHR"),
    iat(
        Api::VkQueuePresentKHR,
        "vulkan-1.dll",
        "vkCreateSwapchainKHR",
    ),
    iat(
        Api::VkQueuePresentKHR,
        "vulkan-1.dll",
        "vkGetSwapchainImagesKHR",
    ),
    iat(Api::VkQueuePresentKHR, "vulkan-1.dll", "vkCreateDevice"),
    iat(Api::VkQueuePresentKHR, "vulkan-1.dll", "vkDestroyDevice"),
    // The loader proc-addr chain: a natively-linked app imports
    // `vkGetInstanceProcAddr` (and often `vkGetDeviceProcAddr`) and resolves
    // every other entry point through them, so hooking these two is what makes
    // the Vulkan detours reachable for such an app.
    iat(
        Api::VkQueuePresentKHR,
        "vulkan-1.dll",
        "vkGetInstanceProcAddr",
    ),
    iat(
        Api::VkQueuePresentKHR,
        "vulkan-1.dll",
        "vkGetDeviceProcAddr",
    ),
];

/// The IAT hook spec for an imported `dll!func`, matched case-insensitively on
/// the DLL name.
pub fn iat_spec(dll: &str, func: &str) -> Option<&'static HookSpec> {
    IAT_HOOKS.iter().find(|h| match h.kind {
        HookKind::Iat { dll: d, func: f } => d.eq_ignore_ascii_case(dll) && f == func,
        HookKind::Vtable { .. } => false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn index(api: Api) -> Option<usize> {
        VTABLE_HOOKS
            .iter()
            .find(|h| h.api == api)
            .and_then(|h| match h.kind {
                HookKind::Vtable { index } => Some(index),
                _ => None,
            })
    }

    #[test]
    fn dxgi_vtable_indices_match_the_verified_table() {
        assert_eq!(index(Api::DxgiPresent), Some(8));
        assert_eq!(index(Api::DxgiPresent1), Some(22));
        assert_eq!(index(Api::DxgiSetFullscreenState), Some(10));
        assert_eq!(index(Api::DxgiResizeBuffers), Some(13));
        assert_eq!(index(Api::DxgiCreateSwapChain), Some(10));
        assert_eq!(index(Api::DxgiCreateSwapChainForHwnd), Some(15));
        assert_eq!(index(Api::DxgiCreateSwapChainForCoreWindow), Some(16));
        assert_eq!(index(Api::DxgiCreateSwapChainForComposition), Some(24));
    }

    #[test]
    fn d3d9_vtable_indices_match_the_verified_table() {
        assert_eq!(index(Api::D3d9Present), Some(17));
        assert_eq!(index(Api::D3d9EndScene), Some(42));
        assert_eq!(index(Api::D3d9PresentEx), Some(121));
        assert_eq!(index(Api::D3d9ResetEx), Some(132));
        assert_eq!(index(Api::D3d9CreateDevice), Some(16));
        assert_eq!(index(Api::D3d9CreateDeviceEx), Some(20));
    }

    #[test]
    fn d3d12_hooks_the_command_queue() {
        assert_eq!(index(Api::D3d12ExecuteCommandLists), Some(10));
    }

    #[test]
    fn iat_spec_matches_case_insensitively() {
        assert!(iat_spec("DXGI.DLL", "CreateDXGIFactory1").is_some());
        assert!(iat_spec("dxgi.dll", "CreateDXGIFactory1").is_some());
        assert!(iat_spec("opengl32.dll", "wglSwapBuffers").is_some());
        assert!(iat_spec("vulkan-1.dll", "vkQueuePresentKHR").is_some());
        assert!(iat_spec("VULKAN-1.DLL", "vkGetInstanceProcAddr").is_some());
        assert!(iat_spec("vulkan-1.dll", "vkGetDeviceProcAddr").is_some());
        assert!(iat_spec("dxgi.dll", "NotAFunction").is_none());
        assert!(iat_spec("other.dll", "CreateDXGIFactory1").is_none());
    }

    #[test]
    fn iat_hooks_cover_the_creation_and_swap_exports() {
        for (dll, func) in [
            ("dxgi.dll", "CreateDXGIFactory"),
            ("dxgi.dll", "CreateDXGIFactory1"),
            ("dxgi.dll", "CreateDXGIFactory2"),
            ("d3d11.dll", "D3D11CreateDeviceAndSwapChain"),
            ("d3d9.dll", "Direct3DCreate9"),
            ("d3d9.dll", "Direct3DCreate9Ex"),
            ("opengl32.dll", "wglSwapBuffers"),
            ("opengl32.dll", "wglSwapLayerBuffers"),
            ("gdi32.dll", "SwapBuffers"),
            ("libEGL.dll", "eglSwapBuffers"),
            ("vulkan-1.dll", "vkQueuePresentKHR"),
            ("vulkan-1.dll", "vkCreateSwapchainKHR"),
            ("vulkan-1.dll", "vkGetSwapchainImagesKHR"),
            ("vulkan-1.dll", "vkCreateDevice"),
            ("vulkan-1.dll", "vkDestroyDevice"),
            ("vulkan-1.dll", "vkGetInstanceProcAddr"),
            ("vulkan-1.dll", "vkGetDeviceProcAddr"),
        ] {
            assert!(
                iat_spec(dll, func).is_some(),
                "missing IAT hook {dll}!{func}"
            );
        }
    }

    /// Every `Api` variant is referenced by a hook, and no hook invents an id.
    ///
    /// Catches: a defined-but-never-hooked `Api` (a dead id the host could
    /// report but that never fires) or a hook bound to an id outside the
    /// enumerated set (which would `from_u16` to `None` on the wire, so the
    /// reader would drop every one of its records).
    #[test]
    fn the_hook_tables_cover_exactly_the_entire_api_id_space() {
        let mut referenced: Vec<u16> = Vec::new();
        for h in VTABLE_HOOKS.iter().chain(IAT_HOOKS) {
            if !referenced.contains(&h.api.as_u16()) {
                referenced.push(h.api.as_u16());
            }
        }
        referenced.sort_unstable();

        let mut enumerated: Vec<u16> = Vec::new();
        for v in 1u16..=64 {
            if Api::from_u16(v).is_some() {
                enumerated.push(v);
            }
        }
        assert_eq!(
            referenced, enumerated,
            "every Api id must be hooked, and every hook must name a real Api"
        );
    }

    /// Ids are 1-based, distinct, and fit the fixed `ORIGINALS`/mask storage.
    ///
    /// Catches: an id of 0 (breaks the `1 << (id - 1)` mask and underflows) or an
    /// id at/above 32 (`ORIGINALS` is `[AtomicUsize; 32]`; an out-of-range index
    /// would panic on the present thread).
    #[test]
    fn every_api_id_is_positive_distinct_and_within_capacity() {
        let mut seen: Vec<u16> = Vec::new();
        for v in 1u16..=64 {
            let Some(api) = Api::from_u16(v) else {
                continue;
            };
            assert!(v >= 1, "ids are 1-based");
            assert!(v < 32, "id {v} does not fit ORIGINALS/[1<<(id-1)]");
            assert!(!seen.contains(&v), "duplicate api id {v}");
            seen.push(v);
            // The cast-based wire value agrees with the enumerated discriminant.
            assert_eq!(api.as_u16(), v);
            assert_eq!(Api::from_u16(api.as_u16()), Some(api));
        }
        assert_eq!(seen.len(), 20, "the hook set has 20 entry points");
    }

    /// The two hand-maintained IAT tables agree on exactly the hooked set.
    ///
    /// Catches: drift between `iat_spec` (which `should_patch` uses to decide a
    /// slot is a target) and `iat_target` (which supplies the detour). A function
    /// in only one table either silently never installs (spec-only) or is
    /// scanned on every rescan for nothing (target-only).
    #[test]
    fn iat_spec_and_iat_target_agree_on_the_hook_set() {
        for spec in IAT_HOOKS {
            let HookKind::Iat { dll, func } = spec.kind else {
                panic!("IAT_HOOKS must only contain Iat specs");
            };
            assert!(
                iat_spec(dll, func).is_some(),
                "{dll}!{func} missing from spec"
            );
            assert!(
                crate::detour::iat_target(dll, func).is_some(),
                "{dll}!{func} missing from target"
            );
        }
        for (dll, func) in [
            ("dxgi.dll", "NotAFunction"),
            ("other.dll", "wglSwapBuffers"),
            ("vulkan-1.dll", "vkQueueSubmit"),
        ] {
            assert!(
                iat_spec(dll, func).is_none(),
                "{dll}!{func} must not be a target"
            );
            assert!(
                crate::detour::iat_target(dll, func).is_none(),
                "{dll}!{func} must not be a target"
            );
        }
    }

    #[test]
    fn vtable_apis_are_unique_and_iat_pairs_are_unique() {
        let mut seen = Vec::new();
        for h in VTABLE_HOOKS {
            assert!(
                !seen.contains(&h.api),
                "duplicate vtable hook for {:?}",
                h.api
            );
            seen.push(h.api);
        }
        let mut pairs = Vec::new();
        for h in IAT_HOOKS {
            let HookKind::Iat { dll, func } = h.kind else {
                panic!("IAT_HOOKS must only contain Iat specs");
            };
            assert!(
                !pairs.contains(&(dll, func)),
                "duplicate IAT hook {dll}!{func}"
            );
            pairs.push((dll, func));
        }
    }
}
