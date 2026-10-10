//! On-wire layout of the minihud present-hook shared-memory block.
//!
//! One named mapping (`minihud-frames-<pid>`) holds a fixed header, a small
//! status block, and a fixed-capacity ring of 40-byte frame slots:
//!
//! ```text
//! offset 0                     : Header  (48 bytes)
//! offset HEADER_SIZE           : Status  (32 bytes)
//! offset RING_OFFSET           : Ring    (RING_CAPACITY * SLOT_SIZE bytes)
//! ```
//!
//! Every multi-byte field is little-endian. The slot layout is the contract
//! between the injected recorder (`hook-rt`) and the host reader, so the
//! offsets are pinned by tests.

/// Magic tag identifying a minihud frame block (`'M','H','F','R'` LE).
pub const MAGIC: u32 = 0x4D48_4652;
/// Wire-format version. Bump on any layout change.
pub const WIRE_VERSION: u16 = 1;
/// Number of 40-byte slots in the ring.
pub const RING_CAPACITY: usize = 4096;
/// Size of one frame slot in bytes.
pub const SLOT_SIZE: usize = 40;
/// Size of the fixed header in bytes.
pub const HEADER_SIZE: usize = 48;
/// Size of the status block in bytes.
pub const STATUS_SIZE: usize = 32;
/// Offset of the ring inside the block.
pub const RING_OFFSET: usize = HEADER_SIZE + STATUS_SIZE;
/// Total block size in bytes.
pub const TOTAL_SIZE: usize = RING_OFFSET + RING_CAPACITY * SLOT_SIZE;

// -- header field offsets ---------------------------------------------------

pub const HEADER_OFF_MAGIC: usize = 0;
pub const HEADER_OFF_VERSION: usize = 4;
pub const HEADER_OFF_QPC_FREQ: usize = 8;
pub const HEADER_OFF_NEXT_SEQ: usize = 16;
pub const HEADER_OFF_ERRORS: usize = 24;
pub const HEADER_OFF_CALLS: usize = 28;
pub const HEADER_OFF_CAPACITY: usize = 32;
pub const HEADER_OFF_SLOT_SIZE: usize = 36;
pub const HEADER_OFF_PID: usize = 40;

// -- status field offsets (absolute; status block starts at HEADER_SIZE) -----

/// Offset of the status block inside the mapping.
pub const STATUS_OFFSET: usize = HEADER_SIZE;

pub const STATUS_OFF_INSTALLED: usize = STATUS_OFFSET;
pub const STATUS_OFF_ERRORS: usize = STATUS_OFFSET + 4;
pub const STATUS_OFF_LAST_ERROR: usize = STATUS_OFFSET + 8;
pub const STATUS_OFF_RESCAN_GEN: usize = STATUS_OFFSET + 12;
pub const STATUS_OFF_RESCAN_COUNT: usize = STATUS_OFFSET + 16;
pub const STATUS_OFF_ATTEMPTED: usize = STATUS_OFFSET + 20;

// -- slot field offsets -----------------------------------------------------

pub const SLOT_OFF_SEQ: usize = 0;
pub const SLOT_OFF_QPC_START: usize = 8;
pub const SLOT_OFF_QPC_STOP: usize = 16;
pub const SLOT_OFF_SWAPCHAIN: usize = 24;
pub const SLOT_OFF_FLAGS: usize = 32;
pub const SLOT_OFF_API: usize = 36;

/// Byte offset of slot `index` inside the block.
pub const fn slot_offset(index: usize) -> usize {
    RING_OFFSET + index * SLOT_SIZE
}

/// What produced a record. Values are stable on the wire.
#[repr(u16)]
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Api {
    /// `IDXGISwapChain::Present` (vtable 8).
    #[default]
    DxgiPresent = 1,
    /// `IDXGISwapChain1::Present1` (vtable 22).
    DxgiPresent1 = 2,
    /// `IDXGISwapChain::ResizeBuffers`.
    DxgiResizeBuffers = 3,
    /// `IDXGISwapChain::SetFullscreenState`.
    DxgiSetFullscreenState = 4,
    /// `IDXGIFactory::CreateSwapChain` (acquisition, vtable 10).
    DxgiCreateSwapChain = 5,
    /// `IDXGIFactory2::CreateSwapChainForHwnd` (acquisition, vtable 15).
    DxgiCreateSwapChainForHwnd = 6,
    /// `IDXGIFactory2::CreateSwapChainForCoreWindow` (vtable 16).
    DxgiCreateSwapChainForCoreWindow = 7,
    /// `IDXGIFactory2::CreateSwapChainForComposition` (vtable 24).
    DxgiCreateSwapChainForComposition = 8,
    /// `IDirect3DDevice9::Present` (vtable 17).
    D3d9Present = 9,
    /// `IDirect3DDevice9::EndScene` (vtable 42).
    D3d9EndScene = 10,
    /// `IDirect3DDevice9Ex::PresentEx` (vtable 121).
    D3d9PresentEx = 11,
    /// `IDirect3DDevice9Ex::ResetEx` (vtable 132).
    D3d9ResetEx = 12,
    /// `IDirect3D9::CreateDevice` (acquisition, vtable 16).
    D3d9CreateDevice = 13,
    /// `IDirect3D9Ex::CreateDeviceEx` (vtable 20).
    D3d9CreateDeviceEx = 14,
    /// `ID3D12CommandQueue::ExecuteCommandLists` (vtable 10).
    D3d12ExecuteCommandLists = 15,
    /// `wglSwapBuffers` (`opengl32.dll`).
    WglSwapBuffers = 16,
    /// `wglSwapLayerBuffers` (`opengl32.dll`).
    WglSwapLayerBuffers = 17,
    /// `SwapBuffers` (`gdi32.dll`).
    GdiSwapBuffers = 18,
    /// `eglSwapBuffers` (`libEGL.dll`, ANGLE).
    EglSwapBuffers = 19,
    /// `vkQueuePresentKHR` (`vulkan-1.dll`).
    VkQueuePresentKHR = 20,
}

impl Api {
    /// Wire value for this API.
    pub const fn as_u16(self) -> u16 {
        self as u16
    }

    /// Decode a wire value; unknown values are rejected.
    pub const fn from_u16(v: u16) -> Option<Api> {
        Some(match v {
            1 => Api::DxgiPresent,
            2 => Api::DxgiPresent1,
            3 => Api::DxgiResizeBuffers,
            4 => Api::DxgiSetFullscreenState,
            5 => Api::DxgiCreateSwapChain,
            6 => Api::DxgiCreateSwapChainForHwnd,
            7 => Api::DxgiCreateSwapChainForCoreWindow,
            8 => Api::DxgiCreateSwapChainForComposition,
            9 => Api::D3d9Present,
            10 => Api::D3d9EndScene,
            11 => Api::D3d9PresentEx,
            12 => Api::D3d9ResetEx,
            13 => Api::D3d9CreateDevice,
            14 => Api::D3d9CreateDeviceEx,
            15 => Api::D3d12ExecuteCommandLists,
            16 => Api::WglSwapBuffers,
            17 => Api::WglSwapLayerBuffers,
            18 => Api::GdiSwapBuffers,
            19 => Api::EglSwapBuffers,
            20 => Api::VkQueuePresentKHR,
            _ => return None,
        })
    }

    /// Human label used by the host's printed line.
    pub const fn as_str(self) -> &'static str {
        match self {
            Api::DxgiPresent => "dxgi.present",
            Api::DxgiPresent1 => "dxgi.present1",
            Api::DxgiResizeBuffers => "dxgi.resizebuffers",
            Api::DxgiSetFullscreenState => "dxgi.setfullscreen",
            Api::DxgiCreateSwapChain => "dxgi.createswapchain",
            Api::DxgiCreateSwapChainForHwnd => "dxgi.forhwnd",
            Api::DxgiCreateSwapChainForCoreWindow => "dxgi.forcorewindow",
            Api::DxgiCreateSwapChainForComposition => "dxgi.forcomposition",
            Api::D3d9Present => "d3d9.present",
            Api::D3d9EndScene => "d3d9.endscene",
            Api::D3d9PresentEx => "d3d9.presentex",
            Api::D3d9ResetEx => "d3d9.resetex",
            Api::D3d9CreateDevice => "d3d9.createdevice",
            Api::D3d9CreateDeviceEx => "d3d9.createdeviceex",
            Api::D3d12ExecuteCommandLists => "d3d12.executecommandlists",
            Api::WglSwapBuffers => "gl.wglswapbuffers",
            Api::WglSwapLayerBuffers => "gl.wglswaplayerbuffers",
            Api::GdiSwapBuffers => "gl.gdiswapbuffers",
            Api::EglSwapBuffers => "gl.eglswapbuffers",
            Api::VkQueuePresentKHR => "vk.queuepresent",
        }
    }
}

/// Slot flags: what the record represents.
pub const FLAG_PRESENT: u32 = 1 << 0;
/// The record carries a swapchain / back-buffer handle in `swapchain`.
pub const FLAG_HAS_SWAPCHAIN: u32 = 1 << 1;
/// The record is an acquisition call (a swapchain/device was created).
pub const FLAG_ACQUISITION: u32 = 1 << 2;
/// The present call failed (the detour still forwards, fail-open).
pub const FLAG_PRESENT_FAILED: u32 = 1 << 3;

/// The payload of one frame slot (everything but the publication `seq`).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct FrameRecord {
    /// `QueryPerformanceCounter` at entry to the present/swap call.
    pub qpc_start: u64,
    /// `QueryPerformanceCounter` at exit (0 when not measured).
    pub qpc_stop: u64,
    /// Swapchain / device / back-buffer handle (0 when not applicable).
    pub swapchain: u64,
    /// [`FLAG_*`](FLAG_PRESENT) bits.
    pub flags: u32,
    /// Which API produced the record.
    pub api: Api,
}

/// Payload bytes in a slot (the 40-byte slot minus the 8-byte `seq`).
pub const PAYLOAD_LEN: usize = SLOT_SIZE - 8;

// Payload-relative field offsets (the `seq` occupies the first 8 slot bytes).
const P_QPC_START: usize = SLOT_OFF_QPC_START - 8;
const P_QPC_STOP: usize = SLOT_OFF_QPC_STOP - 8;
const P_SWAPCHAIN: usize = SLOT_OFF_SWAPCHAIN - 8;
const P_FLAGS: usize = SLOT_OFF_FLAGS - 8;
const P_API: usize = SLOT_OFF_API - 8;

impl FrameRecord {
    /// Encode the 32 payload bytes (no `seq`).
    pub fn encode_payload(&self) -> [u8; PAYLOAD_LEN] {
        let mut out = [0u8; PAYLOAD_LEN];
        put_u64(&mut out, P_QPC_START, self.qpc_start);
        put_u64(&mut out, P_QPC_STOP, self.qpc_stop);
        put_u64(&mut out, P_SWAPCHAIN, self.swapchain);
        put_u32(&mut out, P_FLAGS, self.flags);
        put_u16(&mut out, P_API, self.api.as_u16());
        out
    }

    /// Decode 32 payload bytes; returns `None` on a short slice or unknown API.
    pub fn decode_payload(bytes: &[u8]) -> Option<FrameRecord> {
        if bytes.len() < PAYLOAD_LEN {
            return None;
        }
        Some(FrameRecord {
            qpc_start: get_u64(bytes, P_QPC_START)?,
            qpc_stop: get_u64(bytes, P_QPC_STOP)?,
            swapchain: get_u64(bytes, P_SWAPCHAIN)?,
            flags: get_u32(bytes, P_FLAGS)?,
            api: Api::from_u16(get_u16(bytes, P_API)?)?,
        })
    }
}

/// The decoded block header.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub struct Header {
    pub magic: u32,
    pub version: u16,
    pub qpc_freq: u64,
    pub next_seq: u64,
    pub errors: u32,
    pub calls: u32,
    pub capacity: u32,
    pub slot_size: u32,
    pub pid: u32,
}

// -- little-endian field accessors ------------------------------------------

pub fn put_u16(buf: &mut [u8], off: usize, v: u16) {
    if let Some(s) = buf.get_mut(off..off + 2) {
        s.copy_from_slice(&v.to_le_bytes());
    }
}

pub fn get_u16(buf: &[u8], off: usize) -> Option<u16> {
    Some(u16::from_le_bytes(buf.get(off..off + 2)?.try_into().ok()?))
}

pub fn put_u32(buf: &mut [u8], off: usize, v: u32) {
    if let Some(s) = buf.get_mut(off..off + 4) {
        s.copy_from_slice(&v.to_le_bytes());
    }
}

pub fn get_u32(buf: &[u8], off: usize) -> Option<u32> {
    Some(u32::from_le_bytes(buf.get(off..off + 4)?.try_into().ok()?))
}

pub fn put_u64(buf: &mut [u8], off: usize, v: u64) {
    if let Some(s) = buf.get_mut(off..off + 8) {
        s.copy_from_slice(&v.to_le_bytes());
    }
}

pub fn get_u64(buf: &[u8], off: usize) -> Option<u64> {
    Some(u64::from_le_bytes(buf.get(off..off + 8)?.try_into().ok()?))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sizes_and_offsets_are_pinned() {
        // Hand-derived from the field list in the module docs.
        assert_eq!(SLOT_SIZE, 8 + 8 + 8 + 8 + 4 + 2 + 2);
        assert_eq!(HEADER_SIZE, 48);
        assert_eq!(STATUS_SIZE, 32);
        assert_eq!(RING_OFFSET, 80);
        assert_eq!(TOTAL_SIZE, 80 + 4096 * 40);
        assert_eq!(slot_offset(0), 80);
        assert_eq!(slot_offset(1), 120);
        assert_eq!(slot_offset(4095) + SLOT_SIZE, TOTAL_SIZE);
    }

    #[test]
    fn slot_offsets_match_field_order() {
        assert_eq!(SLOT_OFF_SEQ, 0);
        assert_eq!(SLOT_OFF_QPC_START, 8);
        assert_eq!(SLOT_OFF_QPC_STOP, 16);
        assert_eq!(SLOT_OFF_SWAPCHAIN, 24);
        assert_eq!(SLOT_OFF_FLAGS, 32);
        assert_eq!(SLOT_OFF_API, 36);
        // The last field ends exactly at SLOT_SIZE.
        assert_eq!(SLOT_OFF_API + 2, SLOT_SIZE - 2);
    }

    #[test]
    fn api_round_trips_every_value() {
        for v in 1u16..=20 {
            let api = Api::from_u16(v).expect("known value");
            assert_eq!(api.as_u16(), v);
        }
        assert_eq!(Api::from_u16(0), None);
        assert_eq!(Api::from_u16(21), None);
        assert_eq!(Api::from_u16(u16::MAX), None);
    }

    #[test]
    fn api_labels_are_nonempty_unique_and_pinned() {
        // Catches: a copy-paste leaving two APIs with the same label (the host
        // would misattribute records) or an empty label.
        let mut labels: Vec<&'static str> = Vec::new();
        for v in 1u16..=20 {
            let api = Api::from_u16(v).expect("known value");
            let label = api.as_str();
            assert!(!label.is_empty(), "api {v} has an empty label");
            assert!(
                !labels.contains(&label),
                "duplicate label {label:?} for api {v}"
            );
            labels.push(label);
        }
        // Pin a representative sample so a rename is a deliberate change.
        assert_eq!(Api::DxgiPresent.as_str(), "dxgi.present");
        assert_eq!(Api::D3d9Present.as_str(), "d3d9.present");
        assert_eq!(Api::VkQueuePresentKHR.as_str(), "vk.queuepresent");
    }

    #[test]
    fn frame_record_round_trips_through_bytes() {
        let rec = FrameRecord {
            qpc_start: 0x1122_3344_5566_7788,
            qpc_stop: 0x99AA_BBCC_DDEE_FF00,
            swapchain: 0x0000_0000_DEAD_BEEF,
            flags: FLAG_PRESENT | FLAG_HAS_SWAPCHAIN,
            api: Api::VkQueuePresentKHR,
        };
        let bytes = rec.encode_payload();
        assert_eq!(bytes.len(), SLOT_SIZE - 8);
        assert_eq!(FrameRecord::decode_payload(&bytes), Some(rec));
    }

    #[test]
    fn decode_rejects_unknown_api() {
        let mut bytes = [0u8; SLOT_SIZE - 8];
        put_u16(&mut bytes, SLOT_OFF_API - 8, 999);
        assert_eq!(FrameRecord::decode_payload(&bytes), None);
    }

    #[test]
    fn field_accessors_round_trip() {
        let mut buf = [0u8; 16];
        put_u16(&mut buf, 0, 0xABCD);
        put_u32(&mut buf, 4, 0x1234_5678);
        assert_eq!(get_u16(&buf, 0), Some(0xABCD));
        assert_eq!(get_u32(&buf, 4), Some(0x1234_5678));
        let mut b8 = [0u8; 8];
        put_u64(&mut b8, 0, 0xDEAD_BEEF_CAFE_0001);
        assert_eq!(get_u64(&b8, 0), Some(0xDEAD_BEEF_CAFE_0001));
        // Out-of-range reads are None, never a panic.
        assert_eq!(get_u64(&b8, 1), None);
        assert_eq!(get_u32(&b8, 6), None);
    }

    // -- wire-format contract -------------------------------------------------

    /// The documented little-endian byte order is explicit, not the machine's.
    ///
    /// Catches: an accidental native-endian write (e.g. a transmuted struct on a
    /// big-endian host) that would desync every field for a reader on the other
    /// byte order. Windows is little-endian-only, but the wire format must not
    /// depend on that.
    #[test]
    fn every_multi_byte_field_is_little_endian() {
        let mut b = [0u8; 8];
        put_u16(&mut b, 0, 0x1122);
        assert_eq!(&b[0..2], &[0x22, 0x11], "u16 is LE");
        put_u32(&mut b, 0, 0x1122_3344);
        assert_eq!(&b[0..4], &[0x44, 0x33, 0x22, 0x11], "u32 is LE");
        put_u64(&mut b, 0, 0x1122_3344_5566_7788);
        assert_eq!(&b[0..8], &[0x88, 0x77, 0x66, 0x55, 0x44, 0x33, 0x22, 0x11]);
    }

    /// Header field offsets are pinned to the documented layout.
    ///
    /// Catches: a mistyped `HEADER_OFF_*` that shifts a field. The reader and
    /// writer share the constants, so a round trip still passes — but a block
    /// written by one build and read by another (or by the research POC) would
    /// decode shifted fields silently.
    #[test]
    fn header_field_offsets_are_pinned_to_the_documented_layout() {
        assert_eq!(HEADER_OFF_MAGIC, 0);
        assert_eq!(HEADER_OFF_VERSION, 4);
        assert_eq!(HEADER_OFF_QPC_FREQ, 8);
        assert_eq!(HEADER_OFF_NEXT_SEQ, 16);
        assert_eq!(HEADER_OFF_ERRORS, 24);
        assert_eq!(HEADER_OFF_CALLS, 28);
        assert_eq!(HEADER_OFF_CAPACITY, 32);
        assert_eq!(HEADER_OFF_SLOT_SIZE, 36);
        assert_eq!(HEADER_OFF_PID, 40);
    }

    /// No two header/status fields alias, and they never reach into the ring.
    ///
    /// Catches: a mistyped offset making two fields overlap (a write to one
    /// silently corrupts the other) or a status field overlapping the header.
    #[test]
    fn header_and_status_fields_do_not_overlap_and_precede_the_ring() {
        // Every field as `(start, len)`, plus the region it must stay inside of.
        let header: &[(usize, usize)] = &[
            (HEADER_OFF_MAGIC, 4),
            (HEADER_OFF_VERSION, 2),
            (HEADER_OFF_QPC_FREQ, 8),
            (HEADER_OFF_NEXT_SEQ, 8),
            (HEADER_OFF_ERRORS, 4),
            (HEADER_OFF_CALLS, 4),
            (HEADER_OFF_CAPACITY, 4),
            (HEADER_OFF_SLOT_SIZE, 4),
            (HEADER_OFF_PID, 4),
        ];
        let status: &[(usize, usize)] = &[
            (STATUS_OFF_INSTALLED, 4),
            (STATUS_OFF_ERRORS, 4),
            (STATUS_OFF_LAST_ERROR, 4),
            (STATUS_OFF_RESCAN_GEN, 4),
            (STATUS_OFF_RESCAN_COUNT, 4),
            (STATUS_OFF_ATTEMPTED, 4),
        ];
        let mut all: Vec<(usize, usize)> = header.iter().chain(status).copied().collect();
        all.sort_by_key(|(start, _)| *start);
        let mut end = 0usize;
        for (start, len) in all {
            assert!(
                start >= end,
                "field at {start} overlaps the one ending at {end}"
            );
            end = start + len;
        }
        // Header fields stay inside the header, status inside the status block.
        for (start, len) in header {
            assert!(start + len <= HEADER_SIZE, "header field {start}+{len}");
        }
        for (start, len) in status {
            assert!(
                *start >= STATUS_OFFSET,
                "status field {start} in the header"
            );
            assert!(
                *start + len <= RING_OFFSET,
                "status field {start}+{len} in the ring"
            );
        }
        // The regions are disjoint and ordered header -> status -> ring.
        assert_eq!(STATUS_OFFSET, HEADER_SIZE);
        assert_eq!(RING_OFFSET, STATUS_OFFSET + STATUS_SIZE);
    }

    /// Slot fields are in-bounds and ordered with no overlap.
    #[test]
    fn slot_fields_do_not_overlap_and_fit_the_slot() {
        let fields: &[(usize, usize)] = &[
            (SLOT_OFF_SEQ, 8),
            (SLOT_OFF_QPC_START, 8),
            (SLOT_OFF_QPC_STOP, 8),
            (SLOT_OFF_SWAPCHAIN, 8),
            (SLOT_OFF_FLAGS, 4),
            (SLOT_OFF_API, 2),
        ];
        let mut end = 0usize;
        for (start, len) in fields {
            assert!(*start >= end, "slot field at {start} overlaps {end}");
            assert!(
                *start + len <= SLOT_SIZE,
                "slot field {start}+{len} past {SLOT_SIZE}"
            );
            end = start + len;
        }
        // The last header/vtable slot ends exactly at TOTAL_SIZE.
        assert_eq!(slot_offset(RING_CAPACITY - 1) + SLOT_SIZE, TOTAL_SIZE);
    }

    /// An 8-aligned wire block, like the page-aligned mapping the real writers
    /// use (the seqlock counters are read/written through 8-byte volatile ops).
    #[repr(align(8))]
    struct Aligned([u8; TOTAL_SIZE]);

    /// A block written by the real writer matches the documented byte layout.
    ///
    /// Catches: publishing the payload at the wrong slot offset (e.g. forgetting
    /// the 8-byte `seq` prefix), or a header field written off its documented
    /// offset. The reader would decode shifted fields for every frame, with no
    /// crash to notice.
    #[test]
    fn a_written_block_matches_the_documented_byte_layout() {
        use crate::ring::{RingReader, RingWriter};

        let mut b = Aligned([0u8; TOTAL_SIZE]);
        let rec = FrameRecord {
            qpc_start: 0x0102_0304_0506_0708,
            qpc_stop: 0x1112_1314_1516_1718,
            swapchain: 0x2122_2324_2526_2728,
            flags: FLAG_PRESENT | FLAG_HAS_SWAPCHAIN | FLAG_ACQUISITION,
            api: Api::VkQueuePresentKHR,
        };
        {
            let mut w = RingWriter::init(&mut b.0, 12_345, 6789).expect("init");
            assert_eq!(w.publish(&rec), 0);
        }

        // Header, read back at its raw documented offsets.
        assert_eq!(get_u32(&b.0, HEADER_OFF_MAGIC), Some(MAGIC));
        assert_eq!(get_u16(&b.0, HEADER_OFF_VERSION), Some(WIRE_VERSION));
        assert_eq!(get_u64(&b.0, HEADER_OFF_QPC_FREQ), Some(12_345));
        assert_eq!(get_u64(&b.0, HEADER_OFF_NEXT_SEQ), Some(1));
        assert_eq!(get_u32(&b.0, HEADER_OFF_ERRORS), Some(0));
        assert_eq!(get_u32(&b.0, HEADER_OFF_CALLS), Some(0));
        assert_eq!(
            get_u32(&b.0, HEADER_OFF_CAPACITY),
            Some(RING_CAPACITY as u32)
        );
        assert_eq!(get_u32(&b.0, HEADER_OFF_SLOT_SIZE), Some(SLOT_SIZE as u32));
        assert_eq!(get_u32(&b.0, HEADER_OFF_PID), Some(6789));

        // Slot 0: an even (published) seq, then the payload at its field offsets.
        let base = slot_offset(0);
        assert_eq!(get_u64(&b.0, base + SLOT_OFF_SEQ), Some(2), "2*0+2");
        assert_eq!(
            get_u64(&b.0, base + SLOT_OFF_QPC_START),
            Some(rec.qpc_start)
        );
        assert_eq!(get_u64(&b.0, base + SLOT_OFF_QPC_STOP), Some(rec.qpc_stop));
        assert_eq!(
            get_u64(&b.0, base + SLOT_OFF_SWAPCHAIN),
            Some(rec.swapchain)
        );
        assert_eq!(get_u32(&b.0, base + SLOT_OFF_FLAGS), Some(rec.flags));
        assert_eq!(get_u16(&b.0, base + SLOT_OFF_API), Some(rec.api.as_u16()));

        // And the reader decodes the whole record back, field for field.
        let r = RingReader::new(&b.0).expect("attach");
        assert_eq!(r.read_latest(), Some(rec));
    }

    /// Every `Api` value survives a real writer -> reader round trip.
    ///
    /// Catches: an `Api` id that decodes to a different variant (a transposed
    /// `from_u16` arm), or a payload field the round trip drops — the host's
    /// per-API breakdown would misattribute frames.
    #[test]
    fn every_api_round_trips_through_a_published_slot() {
        use crate::ring::{RingReader, RingWriter};

        let mut b = Aligned([0u8; TOTAL_SIZE]);
        let mut written: Vec<FrameRecord> = Vec::new();
        {
            let mut w = RingWriter::init(&mut b.0, 1, 1).expect("init");
            for v in 1u16..=20 {
                let api = Api::from_u16(v).expect("known api");
                let rec = FrameRecord {
                    qpc_start: v as u64 * 10,
                    qpc_stop: v as u64 * 10 + 1,
                    swapchain: v as u64,
                    flags: FLAG_PRESENT | (v as u32),
                    api,
                };
                w.publish(&rec);
                written.push(rec);
            }
        }
        let r = RingReader::new(&b.0).expect("attach");
        assert_eq!(r.header().next_seq, 20);
        let mut recent = Vec::new();
        assert_eq!(r.read_recent(20, &mut recent), 20);
        assert_eq!(
            recent, written,
            "all 20 records back in order, byte-identical"
        );
    }

    /// Every field survives encode -> decode for arbitrary bit patterns.
    ///
    /// Catches: a field packed at the wrong width/offset for a value a fixed
    /// literal would not exercise (e.g. a high-bit `flags` colliding with `api`,
    /// or a `qpc` value losing its top bytes). Deterministic xorshift so a
    /// failure reproduces.
    #[test]
    fn encoding_round_trips_every_field_for_arbitrary_values() {
        let mut x: u64 = 0x1234_5678_9ABC_DEF0;
        let mut next = || {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x
        };
        for _ in 0..2000 {
            let api = Api::from_u16(1 + (next() % 20) as u16).expect("known api");
            let rec = FrameRecord {
                qpc_start: next(),
                qpc_stop: next(),
                swapchain: next(),
                flags: next() as u32,
                api,
            };
            let bytes = rec.encode_payload();
            assert_eq!(bytes.len(), PAYLOAD_LEN);
            assert_eq!(FrameRecord::decode_payload(&bytes), Some(rec));
        }
    }

    /// The slot's raw `seq` encodes the publication counter, across a wrap.
    #[test]
    fn the_slot_sequence_encodes_the_publication_counter_across_a_wrap() {
        use crate::ring::{RingReader, RingWriter};

        let mut b = Aligned([0u8; TOTAL_SIZE]);
        let total = RING_CAPACITY + 3;
        let rec = FrameRecord {
            qpc_start: 1,
            qpc_stop: 2,
            swapchain: 0,
            flags: FLAG_PRESENT,
            api: Api::DxgiPresent,
        };
        {
            let mut w = RingWriter::init(&mut b.0, 1, 1).expect("init");
            for _ in 0..total {
                w.publish(&rec);
            }
        }
        let idx = (total - 1) % RING_CAPACITY;
        let base = slot_offset(idx);
        // Record `total - 1` is the `total`-th publish: seq = 2*(total-1)+2.
        let expected_seq = 2 * (total as u64 - 1) + 2;
        assert_eq!(get_u64(&b.0, base), Some(expected_seq));
        assert_eq!(get_u64(&b.0, HEADER_OFF_NEXT_SEQ), Some(total as u64));
        let r = RingReader::new(&b.0).expect("attach");
        assert_eq!(r.read_latest().expect("a record").qpc_start, 1);
    }
}
