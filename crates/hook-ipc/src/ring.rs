//! Seqlock ring publication and torn-write-safe reads.
//!
//! Writers and readers share one byte block. Each 40-byte slot carries its own
//! publication counter (`seq`): odd means a write is in progress, even means
//! the slot is stable. A reader only accepts a slot when its `seq` is even and
//! unchanged across the payload copy — a torn write is detected and the reader
//! falls back to the previous stable slot, never returning a partial record.
//! This mirrors `research/poc/shm_transport.py`'s seqlock, applied to a ring.

use core::sync::atomic::{fence, Ordering};

use crate::layout::{
    get_u16, get_u32, get_u64, put_u16, put_u32, put_u64, slot_offset, FrameRecord, Header,
    HEADER_OFF_CALLS, HEADER_OFF_CAPACITY, HEADER_OFF_ERRORS, HEADER_OFF_MAGIC,
    HEADER_OFF_NEXT_SEQ, HEADER_OFF_PID, HEADER_OFF_QPC_FREQ, HEADER_OFF_SLOT_SIZE,
    HEADER_OFF_VERSION, MAGIC, RING_CAPACITY, SLOT_SIZE, STATUS_OFF_ATTEMPTED, STATUS_OFF_ERRORS,
    STATUS_OFF_INSTALLED, STATUS_OFF_LAST_ERROR, STATUS_OFF_RESCAN_COUNT, STATUS_OFF_RESCAN_GEN,
    TOTAL_SIZE, WIRE_VERSION,
};

/// Errors from attaching to or creating a frame block.
#[derive(Debug, thiserror::Error)]
pub enum IpcError {
    /// The block is smaller than [`TOTAL_SIZE`](crate::layout::TOTAL_SIZE).
    #[error("frame block too small: {0} bytes")]
    TooSmall(usize),
    /// The magic tag does not match [`MAGIC`].
    #[error("bad magic: {0:#010x}")]
    BadMagic(u32),
    /// The wire version is not [`WIRE_VERSION`].
    #[error("unsupported wire version: {0}")]
    BadVersion(u16),
    /// The OS mapping call failed.
    #[error("mapping: {0}")]
    Mapping(String),
}

/// Volatile 64-bit load from a byte buffer (used for the seqlock counters).
fn load_u64(buf: &[u8], off: usize) -> u64 {
    // SAFETY: `off` is a fixed in-bounds offset into a buffer of at least
    // `TOTAL_SIZE` bytes; `read_volatile` is valid for any aligned-or-not
    // pointer, and the block is page-aligned so `off` is 8-aligned.
    unsafe { core::ptr::read_volatile(buf.as_ptr().add(off) as *const u64) }
}

/// Volatile 64-bit store into a byte buffer (used for the seqlock counters).
fn store_u64(buf: &mut [u8], off: usize, v: u64) {
    // SAFETY: `off` is a fixed in-bounds offset into a buffer of at least
    // `TOTAL_SIZE` bytes; `write_volatile` is valid for any pointer, and the
    // block is page-aligned so `off` is 8-aligned.
    unsafe { core::ptr::write_volatile(buf.as_mut_ptr().add(off) as *mut u64, v) }
}

/// Volatile 32-bit load from a byte buffer (a status word another process —
/// the injected recorder — may write concurrently).
fn load_u32(buf: &[u8], off: usize) -> u32 {
    // SAFETY: `off` is a fixed in-bounds, 4-aligned offset into a buffer of at
    // least `TOTAL_SIZE` bytes; `read_volatile` is valid for the address.
    unsafe { core::ptr::read_volatile(buf.as_ptr().add(off) as *const u32) }
}

/// Previous ring index, wrapping.
fn prev_index(idx: usize) -> usize {
    if idx == 0 {
        RING_CAPACITY - 1
    } else {
        idx - 1
    }
}

/// Single-writer publisher over a raw block.
pub struct RingWriter<'a> {
    buf: &'a mut [u8],
    next: u64,
}

impl<'a> RingWriter<'a> {
    /// Zero the block and write the header. The caller is the sole creator of
    /// the mapping (the host, or the recorder when it had to create it).
    pub fn init(buf: &'a mut [u8], qpc_freq: u64, pid: u32) -> Result<Self, IpcError> {
        if buf.len() < TOTAL_SIZE {
            return Err(IpcError::TooSmall(buf.len()));
        }
        for b in buf.iter_mut() {
            *b = 0;
        }
        put_u32(buf, HEADER_OFF_MAGIC, MAGIC);
        put_u16(buf, HEADER_OFF_VERSION, WIRE_VERSION);
        put_u64(buf, HEADER_OFF_QPC_FREQ, qpc_freq);
        put_u32(buf, HEADER_OFF_CAPACITY, RING_CAPACITY as u32);
        put_u32(buf, HEADER_OFF_SLOT_SIZE, SLOT_SIZE as u32);
        put_u32(buf, HEADER_OFF_PID, pid);
        Ok(Self { buf, next: 0 })
    }

    /// Resume publishing into an already-initialized block (the recorder that
    /// attached to a host-created mapping). Returns `None` if the header is
    /// not a valid block.
    pub fn resume(buf: &'a mut [u8]) -> Option<Self> {
        let r = RingReader::new(buf).ok()?;
        let next = r.header().next_seq;
        Some(Self { buf, next })
    }

    /// Publish one record, returning its zero-based sequence number.
    pub fn publish(&mut self, rec: &FrameRecord) -> u64 {
        let n = self.next;
        let idx = (n as usize) % RING_CAPACITY;
        let base = slot_offset(idx);
        // Wrapping: `resume` trusts a header's `next_seq`, which a foreign or
        // corrupt block can set arbitrarily. A plain `2 * n + 1` would panic in
        // debug (and wrap unpredictably in release) on a near-`u64::MAX` value.
        let odd = n.wrapping_mul(2).wrapping_add(1);
        // Odd seq marks "write in progress"; readers skip it.
        store_u64(self.buf, base, odd);
        fence(Ordering::SeqCst);
        let payload = rec.encode_payload();
        if let Some(dst) = self.buf.get_mut(base + 8..base + 8 + payload.len()) {
            dst.copy_from_slice(&payload);
        }
        fence(Ordering::SeqCst);
        // Even seq publishes the completed slot.
        store_u64(self.buf, base, odd.wrapping_add(1));
        fence(Ordering::SeqCst);
        self.next = n.wrapping_add(1);
        store_u64(self.buf, HEADER_OFF_NEXT_SEQ, self.next);
        n
    }

    /// Bump the total detour-invocation counter.
    pub fn note_call(&mut self) {
        let cur = get_u32(self.buf, HEADER_OFF_CALLS).unwrap_or(0);
        put_u32(self.buf, HEADER_OFF_CALLS, cur.wrapping_add(1));
    }

    /// Record an internal error: bump the header and status counters.
    pub fn note_error(&mut self, code: i32) {
        let e = get_u32(self.buf, HEADER_OFF_ERRORS).unwrap_or(0);
        put_u32(self.buf, HEADER_OFF_ERRORS, e.wrapping_add(1));
        put_u32(self.buf, STATUS_OFF_ERRORS, e.wrapping_add(1));
        put_u32(self.buf, STATUS_OFF_LAST_ERROR, code as u32);
    }

    /// Publish the installed-API bitmask.
    pub fn set_installed(&mut self, mask: u32) {
        put_u32(self.buf, STATUS_OFF_INSTALLED, mask);
    }

    /// The installed-API bitmask currently published in the status block.
    ///
    /// Read fresh (volatile) each call, because the injected recorder writes
    /// this word from its own view of the same shared block. The Vulkan layer
    /// uses a non-zero value as "a recorder already owns this ring" and defers
    /// to it, so one ring has one writer.
    pub fn installed(&self) -> u32 {
        load_u32(self.buf, STATUS_OFF_INSTALLED)
    }

    /// Record a module-rescan pass.
    pub fn note_rescan(&mut self, generation: u32) {
        let c = get_u32(self.buf, STATUS_OFF_RESCAN_COUNT).unwrap_or(0);
        put_u32(self.buf, STATUS_OFF_RESCAN_COUNT, c.wrapping_add(1));
        put_u32(self.buf, STATUS_OFF_RESCAN_GEN, generation);
    }

    /// Record an install attempt.
    pub fn note_attempt(&mut self) {
        let c = get_u32(self.buf, STATUS_OFF_ATTEMPTED).unwrap_or(0);
        put_u32(self.buf, STATUS_OFF_ATTEMPTED, c.wrapping_add(1));
    }

    /// The next sequence number to be published.
    pub fn next_seq(&self) -> u64 {
        self.next
    }
}

/// Lock-free reader over a raw block.
pub struct RingReader<'a> {
    buf: &'a [u8],
}

impl<'a> RingReader<'a> {
    /// Attach to a block, validating size, magic and version.
    pub fn new(buf: &'a [u8]) -> Result<Self, IpcError> {
        if buf.len() < TOTAL_SIZE {
            return Err(IpcError::TooSmall(buf.len()));
        }
        let magic = get_u32(buf, HEADER_OFF_MAGIC).ok_or(IpcError::TooSmall(buf.len()))?;
        if magic != MAGIC {
            return Err(IpcError::BadMagic(magic));
        }
        let version = get_u16(buf, HEADER_OFF_VERSION).ok_or(IpcError::TooSmall(buf.len()))?;
        if version != WIRE_VERSION {
            return Err(IpcError::BadVersion(version));
        }
        Ok(Self { buf })
    }

    /// The decoded header.
    pub fn header(&self) -> Header {
        Header {
            magic: get_u32(self.buf, HEADER_OFF_MAGIC).unwrap_or(0),
            version: get_u16(self.buf, HEADER_OFF_VERSION).unwrap_or(0),
            qpc_freq: get_u64(self.buf, HEADER_OFF_QPC_FREQ).unwrap_or(0),
            next_seq: get_u64(self.buf, HEADER_OFF_NEXT_SEQ).unwrap_or(0),
            errors: get_u32(self.buf, HEADER_OFF_ERRORS).unwrap_or(0),
            calls: get_u32(self.buf, HEADER_OFF_CALLS).unwrap_or(0),
            capacity: get_u32(self.buf, HEADER_OFF_CAPACITY).unwrap_or(0),
            slot_size: get_u32(self.buf, HEADER_OFF_SLOT_SIZE).unwrap_or(0),
            pid: get_u32(self.buf, HEADER_OFF_PID).unwrap_or(0),
        }
    }

    /// Read the newest stable record, or `None` before the first publish.
    ///
    /// Scans back from the head slot: a torn (odd-`seq`) head falls back to
    /// the previous completed slot rather than returning a partial record.
    pub fn read_latest(&self) -> Option<FrameRecord> {
        let next = load_u64(self.buf, HEADER_OFF_NEXT_SEQ);
        if next == 0 {
            return None;
        }
        let mut idx = ((next - 1) as usize) % RING_CAPACITY;
        for _ in 0..RING_CAPACITY {
            if let Some(rec) = self.read_slot(idx) {
                return Some(rec);
            }
            idx = prev_index(idx);
        }
        None
    }

    /// Read up to `max` recent stable records into `out`, oldest first.
    /// Returns the number written. `out` is cleared first.
    pub fn read_recent(&self, max: usize, out: &mut Vec<FrameRecord>) -> usize {
        out.clear();
        let next = load_u64(self.buf, HEADER_OFF_NEXT_SEQ);
        if next == 0 || max == 0 {
            return 0;
        }
        let count = (next as usize).min(RING_CAPACITY).min(max);
        let mut idx = ((next - 1) as usize) % RING_CAPACITY;
        for _ in 0..count {
            if let Some(rec) = self.read_slot(idx) {
                out.push(rec);
            }
            idx = prev_index(idx);
        }
        out.reverse();
        out.len()
    }

    /// Read one slot if its sequence is even and stable across the copy.
    fn read_slot(&self, idx: usize) -> Option<FrameRecord> {
        let base = slot_offset(idx);
        let s1 = load_u64(self.buf, base);
        if s1 == 0 || s1 & 1 == 1 {
            return None;
        }
        fence(Ordering::SeqCst);
        let payload: [u8; SLOT_SIZE - 8] =
            self.buf.get(base + 8..base + SLOT_SIZE)?.try_into().ok()?;
        fence(Ordering::SeqCst);
        if load_u64(self.buf, base) != s1 {
            return None;
        }
        FrameRecord::decode_payload(&payload)
    }

    /// The installed-API bitmask from the status block.
    pub fn installed_mask(&self) -> u32 {
        get_u32(self.buf, STATUS_OFF_INSTALLED).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{Api, FLAG_HAS_SWAPCHAIN, FLAG_PRESENT};

    /// 8-aligned block storage so the volatile 64-bit counters are aligned.
    #[repr(align(8))]
    struct Aligned([u8; TOTAL_SIZE]);

    fn block() -> Box<Aligned> {
        Box::new(Aligned([0u8; TOTAL_SIZE]))
    }

    fn rec(start: u64, api: Api) -> FrameRecord {
        FrameRecord {
            qpc_start: start,
            qpc_stop: start + 1,
            swapchain: 0xABCD,
            flags: FLAG_PRESENT | FLAG_HAS_SWAPCHAIN,
            api,
        }
    }

    #[test]
    fn round_trips_records_through_the_ring() {
        let mut b = block();
        let mut w = RingWriter::init(&mut b.0, 1_000_000, 4242).expect("init");
        assert_eq!(w.publish(&rec(100, Api::DxgiPresent)), 0);
        assert_eq!(w.publish(&rec(200, Api::D3d9Present)), 1);

        let r = RingReader::new(&b.0).expect("attach");
        assert_eq!(r.header().qpc_freq, 1_000_000);
        assert_eq!(r.header().pid, 4242);
        assert_eq!(r.header().next_seq, 2);
        assert_eq!(r.read_latest(), Some(rec(200, Api::D3d9Present)));

        let mut recent = Vec::new();
        assert_eq!(r.read_recent(8, &mut recent), 2);
        assert_eq!(recent[0], rec(100, Api::DxgiPresent), "oldest first");
        assert_eq!(recent[1], rec(200, Api::D3d9Present));
    }

    #[test]
    fn empty_block_reads_none() {
        let mut b = block();
        // A created-but-unwritten block has a valid header and zero records.
        RingWriter::init(&mut b.0, 1_000_000, 1).expect("init");
        let r = RingReader::new(&b.0).expect("attach");
        assert_eq!(r.read_latest(), None);
        let mut v = Vec::new();
        assert_eq!(r.read_recent(4, &mut v), 0);
    }

    #[test]
    fn torn_head_slot_falls_back_to_the_previous_frame() {
        let mut b = block();
        let mut w = RingWriter::init(&mut b.0, 1_000_000, 1).expect("init");
        w.publish(&rec(10, Api::DxgiPresent));
        w.publish(&rec(20, Api::DxgiPresent));
        w.publish(&rec(30, Api::DxgiPresent));

        // Simulate a writer caught mid-update on the head slot (seq -> odd).
        let head = slot_offset(2);
        store_u64(&mut b.0, head, 5); // 2*2+1 = 5, odd
        {
            let r = RingReader::new(&b.0).expect("attach");
            assert_eq!(
                r.read_latest(),
                Some(rec(20, Api::DxgiPresent)),
                "torn head must not be returned"
            );
        }

        // Completing the write (even seq) makes it visible again.
        store_u64(&mut b.0, head, 6);
        let r = RingReader::new(&b.0).expect("attach");
        assert_eq!(r.read_latest(), Some(rec(30, Api::DxgiPresent)));
    }

    #[test]
    fn unaligned_or_short_buffers_are_rejected() {
        let short = [0u8; 8];
        assert!(matches!(
            RingReader::new(&short),
            Err(IpcError::TooSmall(8))
        ));
        let mut b = block();
        // A block whose magic is wrong is rejected at attach.
        put_u32(&mut b.0, HEADER_OFF_MAGIC, 0xDEAD_BEEF);
        assert!(matches!(
            RingReader::new(&b.0),
            Err(IpcError::BadMagic(0xDEAD_BEEF))
        ));
    }

    #[test]
    fn status_and_counters_track_writes() {
        let mut b = block();
        let mut w = RingWriter::init(&mut b.0, 1, 7).expect("init");
        w.publish(&rec(1, Api::DxgiPresent));
        w.note_call();
        w.note_call();
        w.note_error(-3);
        w.set_installed(0b101);
        w.note_rescan(4);
        w.note_attempt();

        let r = RingReader::new(&b.0).expect("attach");
        let h = r.header();
        assert_eq!(h.calls, 2);
        assert_eq!(h.errors, 1);
        assert_eq!(r.installed_mask(), 0b101);
        assert_eq!(get_u32(&b.0, STATUS_OFF_RESCAN_GEN), Some(4));
        assert_eq!(get_u32(&b.0, STATUS_OFF_RESCAN_COUNT), Some(1));
        assert_eq!(get_u32(&b.0, STATUS_OFF_ATTEMPTED), Some(1));
        assert_eq!(get_u32(&b.0, STATUS_OFF_LAST_ERROR), Some((-3i32) as u32));
    }

    #[test]
    fn resume_continues_the_sequence() {
        let mut b = block();
        {
            let mut w = RingWriter::init(&mut b.0, 1, 9).expect("init");
            w.publish(&rec(1, Api::DxgiPresent));
            w.publish(&rec(2, Api::DxgiPresent));
        }
        let mut w = RingWriter::resume(&mut b.0).expect("resume");
        assert_eq!(w.next_seq(), 2);
        assert_eq!(w.publish(&rec(3, Api::DxgiPresent)), 2);
        let r = RingReader::new(&b.0).expect("attach");
        assert_eq!(r.read_latest(), Some(rec(3, Api::DxgiPresent)));
    }

    #[test]
    fn publish_does_not_overflow_on_a_corrupt_next_seq() {
        // Catches: `resume` trusting the header's `next_seq`, then `publish`
        // computing `2 * n + 1` on a near-`u64::MAX` value — a debug
        // multiply-overflow panic on the target's present thread (or a wrapped
        // sequence that corrupts the seqlock). A foreign/corrupt block can carry
        // any `next_seq`; publishing must wrap, never panic.
        let mut b = block();
        RingWriter::init(&mut b.0, 1_000_000, 1).expect("init");
        put_u64(&mut b.0, HEADER_OFF_NEXT_SEQ, u64::MAX - 1);
        let mut w = RingWriter::resume(&mut b.0).expect("resume a corrupt header");
        assert_eq!(w.next_seq(), u64::MAX - 1);
        let n = w.publish(&rec(1, Api::DxgiPresent));
        assert_eq!(n, u64::MAX - 1, "the sequence wraps instead of panicking");
        assert_eq!(w.next_seq(), u64::MAX);
    }

    #[test]
    fn a_corrupt_header_cannot_make_reads_index_out_of_bounds() {
        // Catches: using the header's `capacity`/`slot_size`/`next_seq` for
        // indexing. A foreign block can declare any capacity or a huge
        // next_seq; the reader must index with its own fixed `RING_CAPACITY`
        // and fail open (no record), never read out of bounds or panic.
        let mut b = block();
        RingWriter::init(&mut b.0, 1_000_000, 1).expect("init");
        put_u32(&mut b.0, HEADER_OFF_CAPACITY, u32::MAX);
        put_u32(&mut b.0, HEADER_OFF_SLOT_SIZE, 1);
        put_u64(&mut b.0, HEADER_OFF_NEXT_SEQ, u64::MAX);
        let r = RingReader::new(&b.0).expect("attach");
        assert_eq!(r.read_latest(), None, "no stable slot in a zeroed ring");
        let mut v = Vec::new();
        assert_eq!(r.read_recent(RING_CAPACITY, &mut v), 0);
    }

    #[test]
    fn a_full_ring_returns_the_newest_capacity_records_in_order() {
        // Catches: an off-by-one in the wrap (`% RING_CAPACITY`) or in
        // `read_recent`'s count that hides or misorders the newest records once
        // the ring has wrapped at least once.
        let mut b = block();
        let mut w = RingWriter::init(&mut b.0, 1_000_000, 1).expect("init");
        let total = RING_CAPACITY as u64 + 37;
        for i in 0..total {
            w.publish(&rec(i, Api::DxgiPresent));
        }
        let r = RingReader::new(&b.0).expect("attach");
        assert_eq!(r.read_latest(), Some(rec(total - 1, Api::DxgiPresent)));
        let mut recent = Vec::new();
        assert_eq!(r.read_recent(RING_CAPACITY, &mut recent), RING_CAPACITY);
        assert_eq!(
            recent[0],
            rec(total - RING_CAPACITY as u64, Api::DxgiPresent),
            "oldest of the wrapped window"
        );
        assert_eq!(
            recent[RING_CAPACITY - 1],
            rec(total - 1, Api::DxgiPresent),
            "newest last"
        );
    }

    #[test]
    fn a_concurrent_writer_never_exposes_a_torn_or_mixed_record() {
        // Catches: a seqlock that is not actually torn-write-safe under a real
        // racing writer — a reader could observe half of record N and half of
        // record N+1 (the `swapchain`/`qpc_*` fields would then disagree). The
        // writer encodes `qpc_start = 2*i`, `qpc_stop = 2*i+1`, `swapchain = i`,
        // so any mix is detectable. Bounded (a fixed publish count), so it
        // cannot hang.
        let b: &'static mut Aligned = Box::leak(Box::new(Aligned([0u8; TOTAL_SIZE])));
        // A `usize` is `Send`; the raw pointer is reconstructed inside the thread.
        let addr = b.0.as_mut_ptr() as usize;
        RingWriter::init(&mut b.0, 1_000_000, 1).expect("init");
        let done = core::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|s| {
            s.spawn(|| {
                // SAFETY: the leaked block is TOTAL_SIZE bytes and the writer is
                // the sole mutator (the seqlock reader only reads).
                let buf = unsafe { core::slice::from_raw_parts_mut(addr as *mut u8, TOTAL_SIZE) };
                let mut w = RingWriter::resume(buf).expect("resume");
                for i in 0..100_000u64 {
                    w.publish(&FrameRecord {
                        qpc_start: i * 2,
                        qpc_stop: i * 2 + 1,
                        swapchain: i,
                        flags: FLAG_PRESENT,
                        api: Api::DxgiPresent,
                    });
                }
                done.store(true, core::sync::atomic::Ordering::SeqCst);
            });
            // SAFETY: the same block, read-only from the seqlock reader side.
            let view = unsafe { core::slice::from_raw_parts(addr as *const u8, TOTAL_SIZE) };
            let r = RingReader::new(view).expect("attach");
            let mut seen = 0u64;
            while !done.load(core::sync::atomic::Ordering::SeqCst) || seen == 0 {
                if let Some(rec) = r.read_latest() {
                    assert_eq!(rec.qpc_stop, rec.qpc_start + 1, "torn payload {rec:?}");
                    assert_eq!(rec.swapchain, rec.qpc_start / 2, "mixed records {rec:?}");
                    seen += 1;
                }
            }
            assert!(seen > 0, "the reader must observe at least one record");
        });
    }
}
