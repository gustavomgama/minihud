//! Low-level memory patching: vtable slots and import-address-table entries.
//!
//! A COM vtable and a PE import-address table are both arrays of function
//! pointers in memory that the OS maps read-only. [`swap_usize`] temporarily
//! re-protects the containing page, swaps one pointer, and restores the
//! protection. [`VtableSlotPatch`] wraps that for a COM interface slot and
//! remembers the original so [`VtableSlotPatch::restore`] can undo it.

use core::ffi::c_void;

#[cfg(windows)]
use windows::Win32::System::Memory::{
    VirtualProtect, PAGE_EXECUTE_READWRITE, PAGE_PROTECTION_FLAGS,
};

/// Read the vtable pointer from a COM object (`*this` is the vtable address).
///
/// # Safety
/// `obj` must point to a live COM interface whose first word is a vtable
/// pointer.
pub unsafe fn vtable_of(obj: *mut c_void) -> *mut usize {
    // SAFETY: the caller guarantees `obj` is a valid COM interface pointer.
    *(obj as *mut *mut usize)
}

/// Swap a `usize` (a function pointer) in place, returning the previous value.
///
/// # Safety
/// `slot` must point to a `usize` inside committed memory that the caller
/// alone mutates for the duration of the swap; the caller must restore the old
/// value before the region is freed.
pub unsafe fn swap_usize(slot: *mut usize, replacement: usize) -> Option<usize> {
    let old_protect = make_writable(slot as *const c_void, size_of::<usize>())?;
    // SAFETY: the page is writable after `make_writable`; `slot` is a valid
    // aligned `usize` per the caller's contract.
    let previous = core::ptr::read(slot);
    core::ptr::write(slot, replacement);
    restore_protect(slot as *const c_void, size_of::<usize>(), old_protect);
    Some(previous)
}

#[cfg(windows)]
fn make_writable(addr: *const c_void, len: usize) -> Option<PAGE_PROTECTION_FLAGS> {
    let mut old = PAGE_PROTECTION_FLAGS(0);
    // SAFETY: `addr`/`len` describe a live committed region supplied by the
    // caller; `old` is a valid out-pointer.
    unsafe { VirtualProtect(addr, len, PAGE_EXECUTE_READWRITE, &mut old).ok()? };
    Some(old)
}

#[cfg(windows)]
fn restore_protect(addr: *const c_void, len: usize, old: PAGE_PROTECTION_FLAGS) {
    let mut ignored = PAGE_PROTECTION_FLAGS(0);
    // SAFETY: the region was made writable by `make_writable`; restoring the
    // recorded protection is best-effort and cannot invalidate live code.
    unsafe {
        let _ = VirtualProtect(addr, len, old, &mut ignored);
    }
}

/// A patched vtable slot that remembers its original pointer.
pub struct VtableSlotPatch {
    slot: *mut usize,
    original: usize,
}

impl VtableSlotPatch {
    /// Replace a raw slot (an IAT entry) with `replacement`.
    ///
    /// # Safety
    /// `slot` must point to a live, committed import-address-table entry that
    /// the caller owns restoring.
    pub unsafe fn from_slot(slot: *mut usize, replacement: usize) -> Option<Self> {
        // Never re-patch a slot already pointing at this replacement: the
        // original is already stored, and adopting the replacement as its own
        // "original" would make the detour forward to itself.
        // SAFETY: `slot` is a live IAT entry per the caller's contract.
        if unsafe { *slot } == replacement {
            return None;
        }
        let original = swap_usize(slot, replacement)?;
        Some(Self { slot, original })
    }

    /// Replace vtable slot `index` of `obj` with `replacement`, returning the
    /// patch. `None` when the page could not be made writable, or when the slot
    /// already holds `replacement` (a shared vtable may be reached by more than
    /// one path — bootstrap then the app's own swapchain — and re-patching would
    /// adopt our own detour as the "original", so it would forward to itself and
    /// recurse without bound).
    ///
    /// # Safety
    /// `obj` must be a live COM interface pointer and `index` a valid slot.
    pub unsafe fn install(obj: *mut c_void, index: usize, replacement: usize) -> Option<Self> {
        let vtable = vtable_of(obj);
        if vtable.is_null() {
            return None;
        }
        // SAFETY: `vtable` is a valid pointer to an array of function pointers
        // at least `index + 1` long, per the caller's contract.
        let slot = vtable.add(index);
        // SAFETY: `slot` is a valid vtable slot per the caller's contract.
        if unsafe { *slot } == replacement {
            return None;
        }
        let original = swap_usize(slot, replacement)?;
        Some(Self { slot, original })
    }

    /// The pointer that was in the slot before the patch.
    pub fn original(&self) -> usize {
        self.original
    }

    /// Restore the original pointer. Idempotent.
    ///
    /// # Safety
    /// The vtable must still be mapped (the owning module must not have been
    /// unloaded).
    pub unsafe fn restore(&mut self) {
        if self.slot.is_null() {
            return;
        }
        let mut _old = 0usize;
        // SAFETY: `slot` was a valid vtable slot when installed and the caller
        // guarantees the module is still loaded.
        if let Some(prev) = swap_usize(self.slot, self.original) {
            _old = prev;
        }
        self.slot = core::ptr::null_mut();
    }
}

// SAFETY: a patch only stores raw pointers; it is never dereferenced except
// through the installer, which serializes every access behind a mutex.
unsafe impl Send for VtableSlotPatch {}

#[cfg(test)]
mod tests {
    use super::*;

    extern "system" fn returns_one() -> usize {
        1
    }
    extern "system" fn returns_two() -> usize {
        2
    }

    #[test]
    fn swap_usize_replaces_and_reports_the_old_value() {
        let mut cells = [11usize, 22usize];
        // SAFETY: `cells` is a live, owned array; the pointer stays valid for
        // the call and no one else mutates it concurrently.
        let old = unsafe { swap_usize(cells.as_mut_ptr(), 99) }.expect("writable");
        assert_eq!(old, 11);
        assert_eq!(cells[0], 99);
    }

    #[test]
    fn from_slot_swaps_restores_and_restore_is_idempotent() {
        // Catches: the IAT path (`from_slot`) forgetting the real original, or
        // a second restore rewriting a slot that has since been legitimately
        // re-patched.
        let mut cells = [7usize];
        // SAFETY: `cells` is a live, owned array; the pointer stays valid for
        // the call and no one else mutates it concurrently.
        let mut p =
            unsafe { VtableSlotPatch::from_slot(cells.as_mut_ptr(), 42) }.expect("writable");
        assert_eq!(p.original(), 7);
        assert_eq!(cells[0], 42, "slot now points at the replacement");

        // SAFETY: `cells` is still live.
        unsafe { p.restore() };
        assert_eq!(cells[0], 7, "restore put the original back");

        // A second restore must be a no-op, not a rewrite of a slot that has
        // since changed.
        cells[0] = 99;
        // SAFETY: `cells` is still live.
        unsafe { p.restore() };
        assert_eq!(cells[0], 99, "second restore must not touch the slot");
    }

    #[test]
    fn install_refuses_to_double_patch_an_already_patched_slot() {
        // Catches: re-patching a slot we already own. The second install would
        // read our detour as the "original" and store it; the detour would then
        // forward to itself -> unbounded recursion / stack overflow on the
        // game's present thread. Must be impossible to reach.
        let mut vtable: [usize; 2] = [
            returns_one as *const () as usize,
            returns_two as *const () as usize,
        ];
        let mut object_storage: usize = vtable.as_mut_ptr() as usize;
        let obj = &mut object_storage as *mut usize as *mut c_void;
        let detour = returns_two as *const () as usize;

        // SAFETY: `obj` points to a word holding a valid vtable address; slot 0
        // is a real function pointer and the backing array outlives the patch.
        let mut first = unsafe { VtableSlotPatch::install(obj, 0, detour) }.expect("first patch");
        assert_eq!(first.original(), returns_one as *const () as usize);

        // A second install on the same, already-patched slot must be refused.
        // SAFETY: same live vtable as above.
        let second = unsafe { VtableSlotPatch::install(obj, 0, detour) };
        assert!(
            second.is_none(),
            "an already-patched slot must never be patched again"
        );

        // The genuine original survives: restoring puts the real function back,
        // never the detour.
        // SAFETY: the backing `vtable` array is still live.
        unsafe { first.restore() };
        assert_eq!(
            vtable[0], returns_one as *const () as usize,
            "restore must yield the real original, not the detour"
        );
    }

    #[test]
    fn vtable_slot_patch_redirects_then_restores() {
        // A COM-shaped object: first word is the vtable address.
        let mut vtable: [usize; 2] = [
            returns_one as *const () as usize,
            returns_two as *const () as usize,
        ];
        let mut object_storage: usize = vtable.as_mut_ptr() as usize;
        let obj = &mut object_storage as *mut usize as *mut c_void;

        // SAFETY: `obj` points to a word holding a valid vtable address; slot 0
        // is a real function pointer and the backing array outlives the patch.
        let mut patch =
            unsafe { VtableSlotPatch::install(obj, 0, returns_two as *const () as usize) }
                .expect("patchable");
        assert_eq!(patch.original(), returns_one as *const () as usize);

        let installed: extern "system" fn() -> usize =
            // SAFETY: slot 0 holds a real `extern "system" fn() -> usize`.
            unsafe { core::mem::transmute(vtable[0]) };
        assert_eq!(installed(), 2, "slot now points at the replacement");

        // SAFETY: the backing `vtable` array is still live.
        unsafe { patch.restore() };
        assert_eq!(
            vtable[0], returns_one as *const () as usize,
            "restored the original"
        );
    }

    #[test]
    fn swap_usize_fails_open_when_the_address_is_not_mapped() {
        // Catches: a `VirtualProtect` failure being ignored, so the swap reads
        // and writes an unmapped address (an access violation on the game's
        // present thread) instead of failing open with `None`.
        //
        // SAFETY: `0x10` is in the always-unmapped null page, so `VirtualProtect`
        // fails *before* any read or write; `swap_usize` must return `None`
        // without touching it.
        let r = unsafe { swap_usize(0x10 as *mut usize, 0xDEAD) };
        assert!(r.is_none(), "an unmapped slot must fail open, not fault");
    }

    #[test]
    fn from_slot_refuses_a_slot_that_already_holds_the_replacement() {
        // Catches: the IAT path re-patching a slot it already owns. The second
        // `from_slot` would adopt our own detour as the "original" and the
        // detour would forward to itself (unbounded recursion).
        let mut cells = [42usize];
        // SAFETY: `cells` is a live, owned array.
        let first = unsafe { VtableSlotPatch::from_slot(cells.as_mut_ptr(), 42) };
        assert!(
            first.is_none(),
            "a slot already at the replacement is refused"
        );
    }

    #[test]
    fn install_restore_then_install_again_round_trips() {
        // Catches: a restore that fails to make the slot look unpatched again
        // (a second install then wrongly refused), or a re-install that adopts
        // the previous detour as its "original".
        let mut vtable: [usize; 2] = [
            returns_one as *const () as usize,
            returns_two as *const () as usize,
        ];
        let mut object_storage: usize = vtable.as_mut_ptr() as usize;
        let obj = &mut object_storage as *mut usize as *mut c_void;
        let detour = returns_two as *const () as usize;

        // SAFETY: `obj` points to a word holding a valid vtable address.
        let mut first = unsafe { VtableSlotPatch::install(obj, 0, detour) }.expect("first patch");
        assert_eq!(first.original(), returns_one as *const () as usize);
        assert_eq!(vtable[0], detour);

        // SAFETY: the backing `vtable` array is still live.
        unsafe { first.restore() };
        assert_eq!(vtable[0], returns_one as *const () as usize, "restored");

        // The slot no longer holds the detour, so a fresh install must succeed
        // and must recover the *real* original, never the previous detour.
        // SAFETY: the backing `vtable` array is still live.
        let mut second =
            unsafe { VtableSlotPatch::install(obj, 0, detour) }.expect("re-patch after restore");
        assert_eq!(
            second.original(),
            returns_one as *const () as usize,
            "re-patch must store the real original, not the prior detour"
        );
        assert_eq!(vtable[0], detour);
        // SAFETY: the backing `vtable` array is still live.
        unsafe { second.restore() };
    }

    #[test]
    fn install_double_restore_is_a_noop() {
        // Catches: a second restore rewriting a slot that has since changed —
        // e.g. a legitimate re-patch by another path would be clobbered back to
        // the stale original.
        let mut vtable: [usize; 2] = [
            returns_one as *const () as usize,
            returns_two as *const () as usize,
        ];
        let mut object_storage: usize = vtable.as_mut_ptr() as usize;
        let obj = &mut object_storage as *mut usize as *mut c_void;
        let detour = returns_two as *const () as usize;

        // SAFETY: `obj` points to a word holding a valid vtable address.
        let mut p = unsafe { VtableSlotPatch::install(obj, 0, detour) }.expect("patch");
        // SAFETY: the backing `vtable` array is still live.
        unsafe { p.restore() };
        assert_eq!(vtable[0], returns_one as *const () as usize);

        // Something else now legitimately owns the slot.
        vtable[0] = 0xABCD;
        // SAFETY: the backing `vtable` array is still live.
        unsafe { p.restore() };
        assert_eq!(vtable[0], 0xABCD, "second restore must not touch the slot");
    }

    #[cfg(windows)]
    #[test]
    fn swap_usize_leaves_the_page_protection_as_it_found_it() {
        use windows::Win32::System::Memory::{
            VirtualAlloc, VirtualFree, VirtualProtect, VirtualQuery, MEMORY_BASIC_INFORMATION,
            MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_PROTECTION_FLAGS, PAGE_READONLY,
            PAGE_READWRITE,
        };

        // A dedicated one-page region so `VirtualProtect` touches only ours.
        // SAFETY: a fresh page reservation, freed before the test returns.
        let page = unsafe { VirtualAlloc(None, 4096, MEM_COMMIT | MEM_RESERVE, PAGE_READWRITE) }
            as *mut usize;
        assert!(!page.is_null(), "VirtualAlloc failed");
        // SAFETY: `page` is our live, committed page.
        unsafe { *page = 7 };

        // Mark it read-only: `swap_usize` must re-protect, write, then restore
        // *this* protection (leaving it PAGE_EXECUTE_READWRITE would be a leak
        // of write access to what was a read-only page).
        let mut old = PAGE_PROTECTION_FLAGS(0);
        // SAFETY: `page` is live and ours.
        unsafe {
            VirtualProtect(page as *const c_void, 4096, PAGE_READONLY, &mut old)
                .expect("make read-only");
        }
        // SAFETY: `page` is live; `swap_usize` internally re-protects the page.
        let previous = unsafe { swap_usize(page, 42) }.expect("swap on a read-only page");
        assert_eq!(
            previous, 7,
            "the old value was read through the temporary grant"
        );

        let mut mbi = MEMORY_BASIC_INFORMATION::default();
        // SAFETY: `page` is live; `mbi` is a valid out-struct of the right size.
        let written = unsafe {
            VirtualQuery(
                Some(page as *const c_void),
                &mut mbi,
                core::mem::size_of::<MEMORY_BASIC_INFORMATION>(),
            )
        };
        assert!(written > 0, "VirtualQuery failed");
        assert_eq!(
            mbi.Protect, PAGE_READONLY,
            "the page's protection must be restored to what it was"
        );

        // Read the swapped value back after re-granting read/write.
        // SAFETY: `page` is live and ours.
        unsafe {
            VirtualProtect(page as *const c_void, 4096, PAGE_READWRITE, &mut old)
                .expect("re-grant");
            assert_eq!(*page, 42, "the swap took effect");
            VirtualFree(page as *mut c_void, 0, MEM_RELEASE).expect("free");
        }
    }
}
