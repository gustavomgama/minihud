//! Pure dispatch for the Vulkan layer's entry-point interception.
//!
//! The loader hands the layer every entry-point name the application looks up
//! (through the layer's `vkGetInstanceProcAddr` / `vkGetDeviceProcAddr`). This
//! module is the pure name → [`Proc`] table: which names the layer substitutes
//! its own function for, and which it forwards down the chain. Keeping it pure
//! lets the one behavior that decides whether a present is ever captured be
//! unit-tested without a GPU or a loader.

/// What the layer returns for a requested Vulkan entry-point name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Proc {
    /// `vkCreateInstance` — the layer captures the next-chain proc-addrs.
    CreateInstance,
    /// `vkCreateDevice` — the layer captures the next device proc-addr.
    CreateDevice,
    /// `vkGetInstanceProcAddr` — the layer's own instance lookup.
    GetInstanceProcAddr,
    /// `vkGetDeviceProcAddr` — the layer's own device lookup.
    GetDeviceProcAddr,
    /// `vk_layerGetPhysicalDeviceProcAddr` — forwards physical-device extensions.
    GetPhysicalDeviceProcAddr,
    /// `vkQueuePresentKHR` — the frame-recording detour.
    QueuePresent,
    /// Not intercepted: forward to the next entity in the chain.
    Forward,
}

/// Classify a requested entry-point name. Names are case-sensitive (Vulkan ABI).
pub fn proc_kind(name: &str) -> Proc {
    match name {
        "vkCreateInstance" => Proc::CreateInstance,
        "vkCreateDevice" => Proc::CreateDevice,
        "vkGetInstanceProcAddr" => Proc::GetInstanceProcAddr,
        "vkGetDeviceProcAddr" => Proc::GetDeviceProcAddr,
        "vk_layerGetPhysicalDeviceProcAddr" => Proc::GetPhysicalDeviceProcAddr,
        "vkQueuePresentKHR" => Proc::QueuePresent,
        _ => Proc::Forward,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn proc_kind_classifies_the_intercepted_names_and_forwards_the_rest() {
        // Catches two breaks that make the layer useless or hostile:
        //  1. A name the layer must intercept (present) missing from the table:
        //     the app's resolved `vkQueuePresentKHR` would stay the loader's raw
        //     pointer and NO frame would ever be recorded.
        //  2. A greedy matcher returning a local function for an unrelated
        //     entry point (`vkCreateInstance`, `vkQueueSubmit`, ...): the loader
        //     would route the app's valid calls into the wrong function.
        assert_eq!(proc_kind("vkCreateInstance"), Proc::CreateInstance);
        assert_eq!(proc_kind("vkCreateDevice"), Proc::CreateDevice);
        assert_eq!(
            proc_kind("vkGetInstanceProcAddr"),
            Proc::GetInstanceProcAddr
        );
        assert_eq!(proc_kind("vkGetDeviceProcAddr"), Proc::GetDeviceProcAddr);
        assert_eq!(
            proc_kind("vk_layerGetPhysicalDeviceProcAddr"),
            Proc::GetPhysicalDeviceProcAddr
        );
        assert_eq!(proc_kind("vkQueuePresentKHR"), Proc::QueuePresent);
        // Everything else forwards down the chain untouched.
        assert_eq!(proc_kind("vkQueueSubmit"), Proc::Forward);
        assert_eq!(proc_kind("vkAcquireNextImageKHR"), Proc::Forward);
        assert_eq!(proc_kind("vkGetPhysicalDeviceProperties"), Proc::Forward);
        assert_eq!(proc_kind(""), Proc::Forward);
        // Names are case-sensitive (Vulkan ABI).
        assert_eq!(proc_kind("vkqueuepresentkhr"), Proc::Forward);
    }

    #[test]
    fn proc_kind_matches_whole_names_and_shadows_no_near_miss() {
        // Catches a prefix/suffix matcher (`starts_with`/`contains`): a name
        // that merely contains an intercepted one would be routed to a local
        // function the loader never expected, and the app's real entry point
        // would be shadowed by the wrong ABI.
        assert_eq!(proc_kind("vkQueuePresent"), Proc::Forward, "prefix");
        assert_eq!(proc_kind("vkQueuePresentKHRx"), Proc::Forward, "suffix");
        assert_eq!(proc_kind("xkvQueuePresentKHR"), Proc::Forward, "leading");
        assert_eq!(
            proc_kind(" vkQueuePresentKHR"),
            Proc::Forward,
            "leading space"
        );
        assert_eq!(
            proc_kind("vkQueuePresentKHR "),
            Proc::Forward,
            "trailing space"
        );
        assert_eq!(proc_kind("vkCreateInstance2"), Proc::Forward);
    }
}
