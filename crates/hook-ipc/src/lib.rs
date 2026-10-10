//! minihud present-hook wire format.
//!
//! A named shared-memory block (`minihud-frames-<pid>`) carries a seqlock ring
//! of frame records written by the injected recorder (`hook-rt`) and read by
//! the host. See [`layout`] for the byte layout, [`ring`] for the seqlock
//! publication protocol, [`metrics`] for the trailing fps/frametime math, and
//! [`map`] for the Windows mapping wrapper.

pub mod layout;
pub mod metrics;
pub mod ring;

#[cfg(windows)]
pub mod map;

#[cfg(windows)]
pub use map::{mapping_name, FrameMapping};

pub use layout::{
    slot_offset, Api, FrameRecord, Header, FLAG_ACQUISITION, FLAG_HAS_SWAPCHAIN, FLAG_PRESENT,
    FLAG_PRESENT_FAILED, HEADER_SIZE, MAGIC, RING_CAPACITY, RING_OFFSET, SLOT_SIZE, STATUS_SIZE,
    TOTAL_SIZE, WIRE_VERSION,
};
pub use metrics::{trailing, Trailing};
pub use ring::{IpcError, RingReader, RingWriter};
