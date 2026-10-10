//! The layer's write side of the shared frame ring.
//!
//! The layer runs in the application process, so it writes the ring for its
//! **own** pid (`minihud-frames-<pid>`) and creates it when absent — exactly
//! like the recorder's standalone path. The host observes it read-only.

use hook_ipc::{Api, FrameMapping, FrameRecord, RingWriter, FLAG_PRESENT};

/// Build the wire record for one present observed at `qpc`.
///
/// `qpc_stop` is 0: the layer records the present's *entry* timestamp only, it
/// does not measure how long the present took. `swapchain` is 0 (the present
/// info is forwarded untouched, not inspected).
pub fn present_record(qpc: u64) -> FrameRecord {
    FrameRecord {
        qpc_start: qpc,
        qpc_stop: 0,
        swapchain: 0,
        flags: FLAG_PRESENT,
        api: Api::VkQueuePresentKHR,
    }
}

/// Write side of the frame ring, owned by the layer process.
pub struct LayerRecorder {
    writer: RingWriter<'static>,
}

impl LayerRecorder {
    /// Open (or create) the frame ring for `pid`. A fresh block is initialized;
    /// an existing one is resumed.
    pub fn open(pid: u32) -> Option<Self> {
        let mapping = FrameMapping::open_or_create(pid).ok()?;
        let created = mapping.is_created();
        // Leak the mapping so the `'static` writer slice outlives the recorder.
        let mapping = Box::leak(Box::new(mapping));
        let ptr = mapping.as_mut_slice().as_mut_ptr();
        let len = mapping.len();
        // SAFETY: the leaked mapping is a writable view of `len` bytes that
        // lives for the process; the layer is the single writer.
        let buf: &'static mut [u8] = unsafe { core::slice::from_raw_parts_mut(ptr, len) };
        let writer = if created {
            RingWriter::init(buf, qpc_frequency(), pid).ok()?
        } else {
            RingWriter::resume(buf)?
        };
        Some(Self { writer })
    }

    /// Publish one present. Fail-open: never panics.
    ///
    /// If the injected recorder has claimed this ring (its non-zero installed
    /// mask is visible in the shared block), the layer **defers** and publishes
    /// nothing: two writers on one ring double-count every present.
    pub fn record_present(&mut self, qpc: u64) {
        if self.writer.installed() != 0 {
            return;
        }
        self.writer.publish(&present_record(qpc));
        self.writer.note_call();
    }
}

/// Current `QueryPerformanceCounter` value (0 on failure).
pub fn qpc_now() -> u64 {
    let mut v: i64 = 0;
    // SAFETY: `v` is a valid out-pointer for a QPC read.
    if unsafe { windows::Win32::System::Performance::QueryPerformanceCounter(&mut v) }.is_ok() {
        return v as u64;
    }
    0
}

/// `QueryPerformanceFrequency` (0 on failure).
fn qpc_frequency() -> u64 {
    let mut v: i64 = 0;
    // SAFETY: `v` is a valid out-pointer for a QPF read.
    if unsafe { windows::Win32::System::Performance::QueryPerformanceFrequency(&mut v) }.is_ok() {
        return v as u64;
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use hook_ipc::{RingReader, FLAG_HAS_SWAPCHAIN};

    /// A pid unlikely to collide with a live process.
    fn test_pid() -> u32 {
        0x5653_0000 | (std::process::id() & 0xFFFF)
    }
    #[test]
    fn present_record_is_a_vulkan_present_stamped_with_the_qpc() {
        // Catches: recording the wrong API (the host's per-API breakdown would
        // misattribute the layer's frames) or dropping FLAG_PRESENT (the host
        // would not count it), or leaking a swapchain flag we never set.
        let rec = present_record(0xABCD_1234);
        assert_eq!(rec.api, Api::VkQueuePresentKHR);
        assert_eq!(rec.qpc_start, 0xABCD_1234);
        assert_eq!(rec.qpc_stop, 0);
        assert_eq!(rec.swapchain, 0);
        assert_ne!(rec.flags & FLAG_PRESENT, 0, "must be a present record");
        assert_eq!(rec.flags & FLAG_HAS_SWAPCHAIN, 0, "no swapchain handle");
    }

    #[test]
    fn recorder_creates_the_ring_and_publishes_presents() {
        // Catches: the layer failing to create a ring when no host/injector is
        // present (the fully-dynamic Vulkan case has no injector) and the
        // observer therefore reading nothing.
        let pid = test_pid();
        {
            let mut rec = LayerRecorder::open(pid).expect("open creates the ring");
            rec.record_present(1_000);
            rec.record_present(2_000);
        }
        let view = FrameMapping::open(pid).expect("the ring exists");
        let r = RingReader::new(view.as_slice()).expect("attach");
        assert_eq!(r.header().next_seq, 2, "two presents published");
        assert_eq!(r.header().pid, pid);
        assert!(r.header().qpc_freq > 0, "QPC frequency written");
        let latest = r.read_latest().expect("a record");
        assert_eq!(latest.api, Api::VkQueuePresentKHR);
        assert_eq!(latest.qpc_start, 2_000);
    }

    #[test]
    fn recorder_resumes_a_host_created_ring_instead_of_recreating_it() {
        // Catches: `LayerRecorder::open` failing (or re-initializing) when the
        // host/injector already created the ring — the injected-plus-layer case
        // would then either record nothing or clobber the host's header, and
        // every frame from a host-created ring would be lost.
        let pid = test_pid() ^ 0x0BAD;
        let mut host = FrameMapping::create(pid).expect("host creates the ring");
        {
            let mut w = RingWriter::init(host.as_mut_slice(), 5_000_000, pid).expect("init");
            w.publish(&present_record(1_111));
        }

        // The layer attaches while the host's mapping is still open.
        let mut rec = LayerRecorder::open(pid).expect("resume the existing ring");
        rec.record_present(2_222);

        let r = RingReader::new(host.as_slice()).expect("attach");
        assert_eq!(r.header().pid, pid, "the host's header is preserved");
        assert_eq!(
            r.header().qpc_freq,
            5_000_000,
            "resume keeps the host's QPC freq"
        );
        assert_eq!(
            r.header().next_seq,
            2,
            "resume continues the host's sequence"
        );
        assert_eq!(r.read_latest().expect("a record").qpc_start, 2_222);
    }

    #[test]
    fn recorder_defers_when_an_injected_recorder_owns_the_ring() {
        // Catches: the layer and the injected recorder both writing the same
        // ring, double-counting every present. Once the recorder has installed
        // (its non-zero status mask is visible in the shared block) the layer
        // must stop publishing, leaving the recorder as the single writer.
        let pid = test_pid() ^ 0x0C0F;
        let mut host = FrameMapping::create(pid).expect("host creates the ring");
        {
            let mut w = RingWriter::init(host.as_mut_slice(), 5_000_000, pid).expect("init");
            w.set_installed(0b1011); // the injected recorder claims the ring
        }
        let mut rec = LayerRecorder::open(pid).expect("resume the existing ring");
        rec.record_present(1_000);
        rec.record_present(2_000);
        let r = RingReader::new(host.as_slice()).expect("attach");
        assert_eq!(
            r.header().next_seq,
            0,
            "the layer must not double-count a recorder-owned ring"
        );
        assert_eq!(
            r.installed_mask(),
            0b1011,
            "the layer must not clobber the recorder's status"
        );
    }
}
