//! Wire format for minihud frame data.
//!
//! One writer (hook-rt, inside the game), one reader (hook-host or,
//! later, minihud itself). The game path must never block, allocate,
//! or fail: a torn slot is detectable and counted, never fatal.
//!
//! Layout: header followed by `capacity` slots. The writer assigns
//! `seq = ++global` per present, writes the slot, and publishes via
//! the sequence number. The reader walks `last_seen..global` and
//! accepts a slot only when `slot.seq` matches the expected value;
//! anything else is counted as lost, not trusted.

use std::sync::atomic::{AtomicU64, Ordering};

/// Must match on both sides; bump on any layout change.
pub const MAGIC: u32 = 0x48444B46; // "HDKF" (hook data frames)
pub const VERSION: u16 = 2;
/// Ring capacity. At 1000fps this holds ~0.5s; the reader polls faster.
pub const CAPACITY: usize = 512;
/// `minihud-frames-<pid>` — per-process, no collisions, no cleanup races.
pub fn mapping_name(pid: u32) -> String {
    format!("minihud-frames-{pid}")
}

/// Presenting API, as observed at the hooked entry point.
#[repr(u8)]
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApiId {
    Dxgi = 0,
    Dxgi1 = 1,
    D3D9 = 2,
    OpenGl = 3,
    Vulkan = 4,
    Unknown = 255,
}

#[repr(C)]
pub struct Header {
    pub magic: u32,
    pub version: u16,
    pub _pad: u16,
    /// QPC frequency, read once by the writer. Reader converts ticks/ms.
    pub qpc_freq: u64,
    /// Next sequence number to assign (writer-owned, read-only for reader).
    pub next_seq: AtomicU64,
    /// Writer-side error counter (hook install failures after init).
    pub errors: AtomicU64,
    /// Detour entries total. If this climbs while counted frames don't,
    /// the bug is in publish/sequencing; if it stays at 1, the detour
    /// itself runs once (hook-side issue).
    pub calls: AtomicU64,
}

#[repr(C)]
pub struct Slot {
    /// Publication marker: valid only when equal to this slot's global order.
    pub seq: u64,
    /// QPC tick at hook entry (frame submit).
    pub qpc_start: u64,
    /// QPC tick after the original call returns (CPU duration).
    pub qpc_stop: u64,
    /// Presenting swapchain address (per-swapchain tracking).
    pub swapchain: u64,
    /// Present flags verbatim (lets the reader skip TEST presents too).
    pub flags: u32,
    pub api: u8,
    pub _pad: [u8; 3],
}

pub const HEADER_SIZE: usize = std::mem::size_of::<Header>();
pub const SLOT_SIZE: usize = std::mem::size_of::<Slot>();
/// Over-provisioned mapping size; the file mapping rounds up anyway.
pub const MAPPING_SIZE: usize = 65536;

const _: () = {
    assert!(std::mem::size_of::<Header>() <= 64);
    assert!(std::mem::size_of::<Slot>() == 40);
};

/// Reader-side view over a mapped region. `base` must point at
/// MAPPING_SIZE readable bytes owned by the caller.
pub struct Reader {
    header: *const Header,
    slots: *const Slot,
}

impl Reader {
    /// # Safety
    /// `base` must be a valid mapping of at least MAPPING_SIZE bytes for
    /// the lifetime of the reader.
    pub unsafe fn new(base: *const u8) -> Option<Self> {
        let header = base as *const Header;
        let h = &*header;
        if h.magic != MAGIC || h.version != VERSION {
            return None;
        }
        let slots = base.add(64) as *const Slot;
        Some(Self { header, slots })
    }

    pub fn qpc_freq(&self) -> u64 {
        unsafe { (*self.header).qpc_freq }
    }

    pub fn next_seq(&self) -> u64 {
        unsafe { (*self.header).next_seq.load(Ordering::Acquire) }
    }

    pub fn calls(&self) -> u64 {
        unsafe { (*self.header).calls.load(Ordering::Acquire) }
    }

    /// Read slot for global order `seq`. Returns None when torn
    /// (writer mid-publish) — count it as lost, do not trust it.
    pub fn slot(&self, seq: u64) -> Option<SlotView> {
        if seq == 0 {
            return None;
        }
        let slot = unsafe { &*self.slots.add((seq as usize) % CAPACITY) };
        // Re-read seq after the payload (cheap torn-write detector).
        let a = unsafe { std::ptr::addr_of!(slot.seq).read_volatile() };
        let view = SlotView {
            qpc_start: unsafe { std::ptr::addr_of!(slot.qpc_start).read_volatile() },
            qpc_stop: unsafe { std::ptr::addr_of!(slot.qpc_stop).read_volatile() },
            swapchain: unsafe { std::ptr::addr_of!(slot.swapchain).read_volatile() },
            flags: unsafe { std::ptr::addr_of!(slot.flags).read_volatile() },
            api: unsafe { std::ptr::addr_of!(slot.api).read_volatile() },
        };
        let b = unsafe { std::ptr::addr_of!(slot.seq).read_volatile() };
        if a == seq && b == seq {
            Some(view)
        } else {
            None
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct SlotView {
    pub qpc_start: u64,
    pub qpc_stop: u64,
    pub swapchain: u64,
    pub flags: u32,
    pub api: u8,
}
