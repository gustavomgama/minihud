//! Windows named shared-memory mapping for the frame block.
//!
//! The host creates `minihud-frames-<pid>` and maps it read/write; the injected
//! recorder opens the same name (or creates it if the host has not yet). Both
//! views alias the same physical pages, so the seqlock in [`crate::ring`] is
//! the only synchronization.

use std::os::windows::ffi::OsStrExt;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE, INVALID_HANDLE_VALUE};
use windows::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, OpenFileMappingW, UnmapViewOfFile, FILE_MAP_ALL_ACCESS,
    MEMORY_MAPPED_VIEW_ADDRESS, PAGE_READWRITE,
};

use crate::layout::TOTAL_SIZE;
use crate::ring::IpcError;

/// The mapping name for a target process id.
pub fn mapping_name(pid: u32) -> String {
    format!("minihud-frames-{pid}")
}

/// Null-terminated UTF-16 for a Win32 wide-string argument.
fn wide(s: &str) -> Vec<u16> {
    std::ffi::OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

/// An owned view of a named frame block.
pub struct FrameMapping {
    handle: HANDLE,
    view: MEMORY_MAPPED_VIEW_ADDRESS,
    len: usize,
    created: bool,
}

impl FrameMapping {
    /// Create (or open an existing) block and map it read/write.
    ///
    /// `created` reports whether this call created the underlying object, so
    /// the caller knows whether it must initialize the header.
    pub fn create(pid: u32) -> Result<Self, IpcError> {
        Self::map(pid, true)
    }

    /// Open an existing block; fails when it has not been created.
    pub fn open(pid: u32) -> Result<Self, IpcError> {
        Self::map(pid, false)
    }

    /// Open the block if it exists, else create it.
    pub fn open_or_create(pid: u32) -> Result<Self, IpcError> {
        Self::open(pid).or_else(|_| Self::create(pid))
    }

    fn map(pid: u32, create: bool) -> Result<Self, IpcError> {
        let name = wide(&mapping_name(pid));
        let (handle, created) = if create {
            // CreateFileMappingW returns the existing object if the name is
            // already present; ERROR_ALREADY_EXISTS is not surfaced here, so a
            // fresh host treats every successful create as creator.
            let h = unsafe {
                CreateFileMappingW(
                    INVALID_HANDLE_VALUE,
                    None,
                    PAGE_READWRITE,
                    0,
                    TOTAL_SIZE as u32,
                    PCWSTR(name.as_ptr()),
                )
            }
            .map_err(|e| IpcError::Mapping(format!("CreateFileMappingW: {e}")))?;
            (h, true)
        } else {
            let h =
                unsafe { OpenFileMappingW(FILE_MAP_ALL_ACCESS.0, false, PCWSTR(name.as_ptr())) }
                    .map_err(|e| IpcError::Mapping(format!("OpenFileMappingW: {e}")))?;
            (h, false)
        };
        let view = unsafe { MapViewOfFile(handle, FILE_MAP_ALL_ACCESS, 0, 0, TOTAL_SIZE) };
        if view.Value.is_null() {
            // SAFETY: `handle` was returned by the mapping call above and is
            // not used after this point.
            unsafe {
                let _ = CloseHandle(handle);
            }
            return Err(IpcError::Mapping("MapViewOfFile returned null".into()));
        }
        Ok(Self {
            handle,
            view,
            len: TOTAL_SIZE,
            created,
        })
    }

    /// Block length in bytes.
    pub fn len(&self) -> usize {
        self.len
    }

    /// True when the block has no bytes (never for a real mapping).
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// True when this call created the underlying named object.
    pub fn is_created(&self) -> bool {
        self.created
    }

    /// Read-only view of the block.
    pub fn as_slice(&self) -> &[u8] {
        // SAFETY: the view is a valid mapping of `len` bytes for the life of
        // `self`; shared reads are the reader side of the seqlock.
        unsafe { std::slice::from_raw_parts(self.view.Value as *const u8, self.len) }
    }

    /// Mutable view of the block (single-writer side).
    pub fn as_mut_slice(&mut self) -> &mut [u8] {
        // SAFETY: the view is a valid, writable mapping of `len` bytes for the
        // life of `self`; the caller is the single writer.
        unsafe { std::slice::from_raw_parts_mut(self.view.Value as *mut u8, self.len) }
    }
}

impl Drop for FrameMapping {
    fn drop(&mut self) {
        // SAFETY: both handles were produced by the mapping calls and are
        // released exactly once here.
        unsafe {
            let _ = UnmapViewOfFile(self.view);
            let _ = CloseHandle(self.handle);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::layout::{Api, FrameRecord, FLAG_PRESENT};
    use crate::ring::{RingReader, RingWriter};

    /// A name unlikely to collide with a live process.
    fn test_pid() -> u32 {
        0x5A5A_0000 | (std::process::id() & 0xFFFF)
    }

    #[test]
    fn create_open_write_read_round_trip() {
        let pid = test_pid();
        let mut writer_view = FrameMapping::create(pid).expect("create");
        assert!(writer_view.is_created());
        {
            let mut w = RingWriter::init(writer_view.as_mut_slice(), 3_000_000, pid).expect("init");
            w.publish(&FrameRecord {
                qpc_start: 111,
                qpc_stop: 222,
                swapchain: 0x1234,
                flags: FLAG_PRESENT,
                api: Api::DxgiPresent,
            });
        }

        // A second, independent view of the same physical pages.
        let reader_view = FrameMapping::open(pid).expect("open");
        assert!(!reader_view.is_created());
        let r = RingReader::new(reader_view.as_slice()).expect("attach");
        assert_eq!(r.header().qpc_freq, 3_000_000);
        let latest = r.read_latest().expect("a published frame");
        assert_eq!(latest.qpc_start, 111);
        assert_eq!(latest.api, Api::DxgiPresent);
    }

    #[test]
    fn open_missing_name_errors() {
        let missing = 0x5A5A_FFFF ^ (std::process::id() & 0xFFFF);
        assert!(FrameMapping::open(missing).is_err());
    }

    #[test]
    fn a_dropped_mapping_leaves_no_stale_object_and_can_be_recreated() {
        // Catches: cleanup that leaves the named object alive (a later process
        // or a re-inject would attach to a dead block) and a recreate that then
        // fails because a stale handle still holds the name.
        let pid = 0x6B6B_0000 | (std::process::id() & 0xFFFF);
        {
            let m = FrameMapping::create(pid).expect("create");
            assert!(m.is_created());
        }
        // The sole handle is closed on drop: the named object must be gone.
        assert!(
            FrameMapping::open(pid).is_err(),
            "no stale mapping may survive the last drop"
        );
        // Recreating works and reports itself as the creator again.
        let m = FrameMapping::create(pid).expect("recreate");
        assert!(m.is_created());
        drop(m);
    }

    #[test]
    fn open_or_create_reports_creation_then_finds_the_existing_object() {
        // Catches: `open_or_create` reporting `created` on the second call, which
        // would make `install()` re-initialize (zero) a block a live recorder is
        // already publishing into.
        let pid = 0x6C6C_0000 | (std::process::id() & 0xFFFF);
        let first = FrameMapping::open_or_create(pid).expect("create");
        assert!(first.is_created(), "the first call creates the block");
        let second = FrameMapping::open_or_create(pid).expect("open existing");
        assert!(
            !second.is_created(),
            "the second call must open, so install() attaches instead of re-initializing"
        );
        drop(second);
        drop(first);
    }

    #[test]
    fn opening_a_smaller_mapping_of_the_same_name_is_rejected() {
        // Catches: aliasing a too-small (stale/foreign/older-version) block as a
        // full `TOTAL_SIZE` view, which would read past the real section and
        // fault. `MapViewOfFile` must refuse the oversized view.
        let pid = 0x6D6D_0000 | (std::process::id() & 0xFFFF);
        let name = wide(&mapping_name(pid));
        // SAFETY: a named 64-byte section, created and closed by this test.
        let small = unsafe {
            CreateFileMappingW(
                INVALID_HANDLE_VALUE,
                None,
                PAGE_READWRITE,
                0,
                64,
                PCWSTR(name.as_ptr()),
            )
        }
        .expect("create small mapping");
        let opened = FrameMapping::open(pid);
        // SAFETY: `small` is our live handle.
        unsafe {
            let _ = CloseHandle(small);
        }
        assert!(
            opened.is_err(),
            "a block smaller than TOTAL_SIZE must be rejected, not mapped short"
        );
    }
}
