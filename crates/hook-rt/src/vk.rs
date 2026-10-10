//! Pure dispatch for the Vulkan loader proc-addr chain.
//!
//! A natively-linked application imports `vulkan-1.dll!vkGetInstanceProcAddr`
//! statically and resolves every other entry point by name through it (and,
//! for device-level functions, through `vkGetDeviceProcAddr`). The recorder
//! IAT-hooks both proc-addr exports; when one of *our* target names is
//! requested it records the real pointer and hands back its own detour, so the
//! application's later `vkQueuePresentKHR` call lands on the recorder instead
//! of the raw loader trampoline.
//!
//! This module is the pure name → target table (no globals, no FFI), so the
//! dispatch is unit-tested without a GPU.

/// A proc-addr name the recorder substitutes a detour for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Detour {
    /// `vkQueuePresentKHR` — records a frame.
    QueuePresent,
    /// `vkCreateSwapchainKHR` — forwards (acquisition).
    CreateSwapchain,
    /// `vkGetSwapchainImagesKHR` — forwards (acquisition).
    GetSwapchainImages,
    /// `vkCreateDevice` — forwards (acquisition).
    CreateDevice,
    /// `vkDestroyDevice` — forwards (acquisition).
    DestroyDevice,
    /// `vkGetDeviceProcAddr` — hand back our device proc-addr detour so an
    /// ash-style loader that resolves device functions through it stays inside
    /// the chain. Without this, `vkGetInstanceProcAddr(instance,
    /// "vkGetDeviceProcAddr")` yields the real loader pointer and the present
    /// detour is never installed.
    GetDeviceProcAddr,
}

/// Map a requested Vulkan entry-point name to the detour we substitute, or
/// `None` to forward the real pointer. Names are case-sensitive (Vulkan ABI).
pub fn vk_target(name: &str) -> Option<Detour> {
    Some(match name {
        "vkQueuePresentKHR" => Detour::QueuePresent,
        "vkCreateSwapchainKHR" => Detour::CreateSwapchain,
        "vkGetSwapchainImagesKHR" => Detour::GetSwapchainImages,
        "vkCreateDevice" => Detour::CreateDevice,
        "vkDestroyDevice" => Detour::DestroyDevice,
        "vkGetDeviceProcAddr" => Detour::GetDeviceProcAddr,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vk_target_maps_every_hooked_name_and_rejects_the_rest() {
        // Catches: a hooked Vulkan entry point (present/acquisition) missing
        // from the dispatch — the recorder would hand the app the raw loader
        // pointer and never capture. Also catches a greedy matcher that would
        // substitute a detour for the loader's own proc-addr or unrelated
        // commands (infinite recursion / broken loader).
        assert_eq!(vk_target("vkQueuePresentKHR"), Some(Detour::QueuePresent));
        assert_eq!(
            vk_target("vkCreateSwapchainKHR"),
            Some(Detour::CreateSwapchain)
        );
        assert_eq!(
            vk_target("vkGetSwapchainImagesKHR"),
            Some(Detour::GetSwapchainImages)
        );
        assert_eq!(vk_target("vkCreateDevice"), Some(Detour::CreateDevice));
        assert_eq!(vk_target("vkDestroyDevice"), Some(Detour::DestroyDevice));
        assert_eq!(
            vk_target("vkGetDeviceProcAddr"),
            Some(Detour::GetDeviceProcAddr)
        );
        // Every other name forwards the real pointer.
        assert_eq!(vk_target("vkGetInstanceProcAddr"), None);
        assert_eq!(vk_target("vkCreateInstance"), None);
        assert_eq!(vk_target("vkQueueSubmit"), None);
        assert_eq!(vk_target(""), None);
        // Names are case-sensitive.
        assert_eq!(vk_target("vkqueuepresentkhr"), None);
    }
}
