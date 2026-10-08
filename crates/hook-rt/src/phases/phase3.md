# Phase 3: Vulkan loader + per-device
Status: STUB — spec developed, code deferred

Design:
- Vulkan: hook vkQueuePresentKHR via loader (vulkan-1.dll export) — not vtable (Vulkan uses function pointers, not COM vtables)
- Per-device: track vkDevice + swapchain handle in ipc::Slot (add device_id field to wire format if needed)
- Wire format extension: version 3, add device_id u32 to Slot
- Validation: same 300-frame harness, Vulkan swapchain

Blocker: RTSS interference; needs clean env for loader hook validation.
