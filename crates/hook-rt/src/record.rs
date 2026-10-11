//! The in-target recorder: one QPC-stamped [`FrameRecord`] per present.
//!
//! The recorder owns the write side of the shared ring. It records timing and
//! swapchain metadata only — it never renders anything. The global instance is
//! held behind a `try_lock` so a contended present drops its sample instead of
//! blocking the game (fail-open).

use hook_ipc::{Api, FrameRecord, RingWriter, FLAG_HAS_SWAPCHAIN, FLAG_PRESENT};

/// Write side of the frame ring.
pub struct Recorder {
    writer: RingWriter<'static>,
}

impl Recorder {
    /// Create a recorder over a raw block, initializing the header.
    ///
    /// # Safety
    /// `ptr`/`len` must describe a writable mapping that outlives the recorder
    /// and is written by this thread alone.
    pub unsafe fn create(ptr: *mut u8, len: usize, qpc_freq: u64, pid: u32) -> Option<Self> {
        // SAFETY: the caller guarantees `ptr`/`len` name a writable mapping
        // that outlives the recorder.
        let buf: &'static mut [u8] = unsafe { core::slice::from_raw_parts_mut(ptr, len) };
        let writer = RingWriter::init(buf, qpc_freq, pid).ok()?;
        Some(Self { writer })
    }

    /// Attach to an already-initialized block.
    ///
    /// # Safety
    /// Same as [`Recorder::create`].
    pub unsafe fn attach(ptr: *mut u8, len: usize) -> Option<Self> {
        // SAFETY: the caller guarantees `ptr`/`len` name a writable mapping
        // that outlives the recorder.
        let buf: &'static mut [u8] = unsafe { core::slice::from_raw_parts_mut(ptr, len) };
        let writer = RingWriter::resume(buf)?;
        Some(Self { writer })
    }

    /// Record one present. `handle` is the swapchain/device/back-buffer, or 0.
    pub fn record_present(
        &mut self,
        api: Api,
        qpc_start: u64,
        qpc_stop: u64,
        handle: u64,
        extra_flags: u32,
    ) {
        let mut flags = FLAG_PRESENT | extra_flags;
        if handle != 0 {
            flags |= FLAG_HAS_SWAPCHAIN;
        }
        let rec = FrameRecord {
            qpc_start,
            qpc_stop,
            swapchain: handle,
            flags,
            api,
        };
        self.writer.publish(&rec);
        self.writer.note_call();
    }

    /// Record an internal error code.
    pub fn note_error(&mut self, code: i32) {
        self.writer.note_error(code);
    }

    /// Publish the installed-API bitmask.
    pub fn set_installed(&mut self, mask: u32) {
        self.writer.set_installed(mask);
    }

    /// Record a module-rescan pass.
    pub fn note_rescan(&mut self, generation: u32) {
        self.writer.note_rescan(generation);
    }

    /// Record an install attempt.
    pub fn note_attempt(&mut self) {
        self.writer.note_attempt();
    }
}

/// Current `QueryPerformanceCounter` value (0 on failure).
pub fn qpc_now() -> u64 {
    #[cfg(windows)]
    {
        let mut v: i64 = 0;
        // SAFETY: `v` is a valid out-pointer for a QPC read.
        if unsafe { windows::Win32::System::Performance::QueryPerformanceCounter(&mut v) }.is_ok() {
            return v as u64;
        }
        0
    }
    #[cfg(not(windows))]
    {
        0
    }
}

/// `QueryPerformanceFrequency` (0 on failure).
pub fn qpc_frequency() -> u64 {
    #[cfg(windows)]
    {
        let mut v: i64 = 0;
        // SAFETY: `v` is a valid out-pointer for a QPC read.
        if unsafe { windows::Win32::System::Performance::QueryPerformanceFrequency(&mut v) }.is_ok()
        {
            return v as u64;
        }
        0
    }
    #[cfg(not(windows))]
    {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use hook_ipc::{RingReader, TOTAL_SIZE};

    #[repr(align(8))]
    struct Aligned([u8; TOTAL_SIZE]);

    fn leaked_block() -> &'static mut Aligned {
        Box::leak(Box::new(Aligned([0u8; TOTAL_SIZE])))
    }

    #[test]
    fn records_presents_into_the_ring() {
        let b = leaked_block();
        // SAFETY: the leaked block is writable and lives for the process.
        let mut rec = unsafe { Recorder::create(b.0.as_mut_ptr(), TOTAL_SIZE, 1_000_000, 99) }
            .expect("create");
        rec.record_present(Api::DxgiPresent, 100, 150, 0x1234, 0);
        rec.record_present(Api::D3d9Present, 200, 260, 0x5678, 0);

        let r = RingReader::new(&b.0).expect("attach");
        assert_eq!(r.header().qpc_freq, 1_000_000);
        assert_eq!(r.header().next_seq, 2);
        let latest = r.read_latest().expect("a record");
        assert_eq!(latest.api, Api::D3d9Present);
        assert_eq!(latest.qpc_start, 200);
        assert_eq!(latest.qpc_stop, 260);
        assert_eq!(latest.swapchain, 0x5678);
        assert_ne!(latest.flags & FLAG_PRESENT, 0);
        assert_ne!(latest.flags & FLAG_HAS_SWAPCHAIN, 0);
        assert_eq!(r.header().calls, 2, "each present bumps the call counter");
    }

    #[test]
    fn zero_handle_clears_the_swapchain_flag() {
        let b = leaked_block();
        // SAFETY: the leaked block is writable and lives for the process.
        let mut rec =
            unsafe { Recorder::create(b.0.as_mut_ptr(), TOTAL_SIZE, 1, 1) }.expect("create");
        rec.record_present(Api::WglSwapBuffers, 1, 2, 0, 0);
        let r = RingReader::new(&b.0).expect("attach");
        let latest = r.read_latest().expect("a record");
        assert_eq!(latest.flags & FLAG_HAS_SWAPCHAIN, 0);
    }

    #[test]
    fn attach_resumes_an_existing_block() {
        let b = leaked_block();
        // SAFETY: the leaked block is writable and lives for the process.
        unsafe {
            let mut rec =
                Recorder::create(b.0.as_mut_ptr(), TOTAL_SIZE, 2_000_000, 7).expect("create");
            rec.record_present(Api::DxgiPresent, 1, 2, 0, 0);
        }
        // SAFETY: the same block, still writable and owned by this test.
        let mut rec = unsafe { Recorder::attach(b.0.as_mut_ptr(), TOTAL_SIZE) }.expect("attach");
        rec.record_present(Api::DxgiPresent, 3, 4, 0, 0);
        let r = RingReader::new(&b.0).expect("attach");
        assert_eq!(r.header().qpc_freq, 2_000_000);
        assert_eq!(r.header().next_seq, 2, "attach resumed the sequence");
    }

    #[test]
    fn recorder_status_writes_reach_the_block() {
        // Catches: a recorder method that silently drops its status write, so
        // the host's installed mask / error / rescan counters stay frozen.
        use hook_ipc::layout::{
            get_u32, STATUS_OFF_ATTEMPTED, STATUS_OFF_LAST_ERROR, STATUS_OFF_RESCAN_COUNT,
            STATUS_OFF_RESCAN_GEN,
        };
        let b = leaked_block();
        // SAFETY: the leaked block is writable and lives for the process.
        let mut rec =
            unsafe { Recorder::create(b.0.as_mut_ptr(), TOTAL_SIZE, 1, 5) }.expect("create");
        rec.set_installed(0b1011);
        rec.note_error(-7);
        rec.note_rescan(9);
        rec.note_attempt();

        let r = RingReader::new(&b.0).expect("attach");
        assert_eq!(r.installed_mask(), 0b1011);
        assert_eq!(r.header().errors, 1, "note_error bumps the header counter");
        assert_eq!(get_u32(&b.0, STATUS_OFF_LAST_ERROR), Some((-7i32) as u32));
        assert_eq!(get_u32(&b.0, STATUS_OFF_RESCAN_GEN), Some(9));
        assert_eq!(get_u32(&b.0, STATUS_OFF_RESCAN_COUNT), Some(1));
        assert_eq!(get_u32(&b.0, STATUS_OFF_ATTEMPTED), Some(1));
    }

    #[test]
    fn attach_rejects_a_block_that_was_never_initialized() {
        // Catches: attaching to a zeroed/foreign mapping and publishing into it
        // as if it were ours — would corrupt another process's block or mislead
        // a reader with a bad magic/version.
        let b = leaked_block(); // all zero: no magic, no version
                                // SAFETY: the leaked block is writable and lives for the process.
        let rec = unsafe { Recorder::attach(b.0.as_mut_ptr(), TOTAL_SIZE) };
        assert!(rec.is_none(), "an uninitialized block must not be attached");
    }

    #[cfg(windows)]
    #[test]
    fn qpc_now_is_a_live_counter() {
        // Catches: a stubbed qpc_now returning 0, which would make every frame
        // timestamp identical and the trailing fps math return None forever.
        assert!(qpc_now() > 0, "QueryPerformanceCounter must be live");
    }

    #[cfg(windows)]
    #[test]
    fn qpc_now_never_goes_backwards() {
        // Catches: a timestamp source that is not monotonic, which would make a
        // later present look older than an earlier one and the trailing span
        // math fail open (`None`) or, worse, report a negative interval. QPC is
        // monotonic by contract; consecutive reads must be non-decreasing.
        let mut prev = qpc_now();
        for _ in 0..1000 {
            let now = qpc_now();
            assert!(now >= prev, "QPC went backwards: {now} < {prev}");
            prev = now;
        }
    }

    #[cfg(windows)]
    #[test]
    fn qpc_frequency_is_nonzero() {
        assert!(qpc_frequency() > 0);
    }

    /// Every entry point's id is stamped verbatim and the handle sets the
    /// swapchain flag.
    ///
    /// Catches: a `record_present` that drops or transposes the `api` field (the
    /// host would misattribute every frame) or mishandles the `FLAG_HAS_SWAPCHAIN`
    /// bit for a null/non-null handle (a 0-handle record advertising a swapchain,
    /// or a real handle losing it).
    #[test]
    fn record_present_stamps_the_api_and_handle_for_every_entry_point() {
        for v in 1u16..=20 {
            let api = Api::from_u16(v).expect("known api");
            let b = leaked_block();
            // SAFETY: the leaked block is writable and lives for the process.
            let mut rec = unsafe { Recorder::create(b.0.as_mut_ptr(), TOTAL_SIZE, 1, v as u32) }
                .expect("create");
            rec.record_present(api, 10, 20, 0xABCD, 0);
            rec.record_present(api, 30, 40, 0, 0);
            let r = RingReader::new(&b.0).expect("attach");

            let mut recent = Vec::new();
            assert_eq!(r.read_recent(2, &mut recent), 2);
            assert_eq!(recent[0].api, api, "api id {v} must round-trip");
            assert_eq!(recent[1].api, api);
            assert_eq!(recent[0].swapchain, 0xABCD);
            assert_ne!(
                recent[0].flags & FLAG_HAS_SWAPCHAIN,
                0,
                "handle set the bit"
            );
            assert_ne!(
                recent[0].flags & FLAG_PRESENT,
                0,
                "every record is a present"
            );
            assert_eq!(recent[1].swapchain, 0);
            assert_eq!(
                recent[1].flags & FLAG_HAS_SWAPCHAIN,
                0,
                "a zero handle must not advertise a swapchain"
            );
        }
    }

    /// The publication sequence is monotonic and gap-free, and survives a ring
    /// wrap.
    ///
    /// Catches: a `next_seq` that skips or repeats (a reader's ring index would
    /// drift, returning stale/duplicate frames forever) or that resets when the
    /// ring wraps (the host would report fps over the wrong window).
    #[test]
    fn the_sequence_is_monotonic_and_gap_free_across_a_ring_wrap() {
        let b = leaked_block();
        let total = hook_ipc::RING_CAPACITY + 7;
        // SAFETY: the leaked block is writable and lives for the process.
        let mut rec =
            unsafe { Recorder::create(b.0.as_mut_ptr(), TOTAL_SIZE, 1, 1) }.expect("create");
        for i in 0..total as u64 {
            let n = rec.writer.publish(&hook_ipc::FrameRecord {
                qpc_start: i,
                qpc_stop: i,
                swapchain: 0,
                flags: FLAG_PRESENT,
                api: Api::DxgiPresent,
            });
            assert_eq!(n, i, "publish must return a gap-free sequence");
        }
        let r = RingReader::new(&b.0).expect("attach");
        assert_eq!(
            r.header().next_seq,
            total as u64,
            "next_seq counts every publish, past the capacity"
        );
        assert_eq!(
            r.read_latest().expect("a record").qpc_start,
            (total - 1) as u64,
            "the newest record survives the wrap"
        );
        let mut recent = Vec::new();
        assert_eq!(
            r.read_recent(hook_ipc::RING_CAPACITY, &mut recent),
            hook_ipc::RING_CAPACITY
        );
        assert_eq!(recent.first().expect("oldest").qpc_start, 7);
        assert_eq!(recent.last().expect("newest").qpc_start, (total - 1) as u64);
    }

    /// Overfilling the ring drops the oldest record, never corrupting the
    /// sequence or the newest record.
    ///
    /// Catches: a full-ring path that stalls, wraps `next_seq` back to zero (the
    /// reader's "empty" sentinel), or loses the newest frame.
    #[test]
    fn a_full_ring_drops_the_oldest_without_corrupting_the_sequence() {
        let b = leaked_block();
        // SAFETY: the leaked block is writable and lives for the process.
        let mut rec =
            unsafe { Recorder::create(b.0.as_mut_ptr(), TOTAL_SIZE, 1, 1) }.expect("create");
        for i in 0..(hook_ipc::RING_CAPACITY as u64 * 3) {
            rec.record_present(Api::DxgiPresent, i, i, 0x100, 0);
        }
        let r = RingReader::new(&b.0).expect("attach");
        assert_eq!(r.header().next_seq, hook_ipc::RING_CAPACITY as u64 * 3);
        assert_ne!(
            r.header().next_seq,
            0,
            "must not wrap to the empty sentinel"
        );
        assert_eq!(
            r.read_latest().expect("a record").qpc_start,
            hook_ipc::RING_CAPACITY as u64 * 3 - 1
        );
    }
}
