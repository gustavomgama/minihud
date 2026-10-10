//! Minimal PE import-table reader for IAT hooking.
//!
//! 64-bit only (the injector refuses WOW64 targets). Given a module base it
//! walks **both** import directories — the standard one (data directory 1) and
//! the delay-load one (data directory 13) — and yields each imported `dll!func`
//! together with the address of its import-address-table slot, which is the
//! pointer an IAT hook swaps.
//!
//! A DLL imported via a delay descriptor is absent from the standard table, so
//! walking only directory 1 silently misses it; e.g. an app that delay-loads
//! `d3d9.dll` would never have its `Direct3DCreate9` import hooked.

/// One imported function and its IAT slot.
pub struct ImportRef {
    /// Importing DLL name, e.g. `DXGI.DLL`.
    pub dll: String,
    /// Imported function name, e.g. `CreateDXGIFactory1`.
    pub func: String,
    /// Address of the IAT entry (where the resolved pointer lives).
    pub slot: *mut usize,
    /// True for an import found in the delay-load directory. A delay IAT slot
    /// is overwritten by `__delayLoadHelper2` the first time the function is
    /// called, so a hook on it must be re-attempted rather than deduplicated.
    pub delay: bool,
}

const DOS_MAGIC: u16 = 0x5A4D; // "MZ"
const PE_SIGNATURE: u32 = 0x0000_4550; // "PE\0\0"
const PE32_PLUS_MAGIC: u16 = 0x20B;
/// `NumberOfRvaAndSizes` offset inside the PE32+ optional header.
const OPT_NUM_DIR_OFF: usize = 108;
/// `SizeOfImage` offset inside the PE32+ optional header. Every RVA is bounded
/// by it: the image's own declaration of how many bytes it occupies.
const OPT_SIZE_OF_IMAGE_OFF: usize = 56;
/// The DOS header is 64 bytes; `e_lfanew` must point at or past it.
const DOS_HEADER_SIZE: usize = 64;
/// A loose ceiling on `e_lfanew` so a corrupt in-memory header cannot send the
/// NT-header read far out of bounds when no buffer length is known (a live
/// module passes `usize::MAX`). Real images are far below this.
const MAX_NT_OFFSET: usize = 0x10_0000;
/// Data-directory offset inside the PE32+ optional header.
const OPT_DATA_DIR_OFF: usize = 112;
/// Size half of an `IMAGE_DATA_DIRECTORY` entry (RVA at offset 0, size here).
const DATA_DIR_SIZE_OFF: usize = 4;
/// `e_lfanew` offset inside the DOS header (points at the NT headers).
const DOS_E_LFANEW_OFF: usize = 0x3C;
/// NT signature size (the `PE\0\0` before the file header).
const NT_SIGNATURE_SIZE: usize = 4;
/// `IMAGE_FILE_HEADER` size (between the NT signature and the optional header).
const FILE_HEADER_SIZE: usize = 20;
/// Size of one `IMAGE_DATA_DIRECTORY` (RVA + size).
const DATA_DIR_ENTRY: usize = 8;
/// Standard import directory index in the data directory.
const IMPORT_DIR_INDEX: usize = 1;
/// Delay-load import directory index in the data directory.
const DELAY_IMPORT_DIR_INDEX: usize = 13;
/// Size of one `IMAGE_IMPORT_DESCRIPTOR`.
const IMPORT_DESC_SIZE: usize = 20;
/// Size of one `IMAGE_DELAYLOAD_DESCRIPTOR`.
const DELAY_DESC_SIZE: usize = 32;
/// Size of one PE32+ thunk (a lookup-table or IAT entry). PE32+ thunks are 8
/// bytes; a 4-byte stride reads the high half of the first entry (zero) and
/// stops, hiding every import after the first.
const THUNK_SIZE: usize = 8;
/// Field offsets inside a standard `IMAGE_IMPORT_DESCRIPTOR` (20 bytes).
const STD_ORIGINAL_FIRST_THUNK_OFF: usize = 0;
const STD_NAME_OFF: usize = 12;
const STD_FIRST_THUNK_OFF: usize = 16;
/// Field offsets inside an `IMAGE_DELAYLOAD_DESCRIPTOR` (32 bytes).
const DELAY_ATTRS_OFF: usize = 0;
const DELAY_DLL_NAME_OFF: usize = 4;
const DELAY_IAT_OFF: usize = 12;
const DELAY_INT_OFF: usize = 16;
/// Hard cap on a DLL/function name read from the image, so a missing NUL
/// terminator cannot make the scan run without bound.
const MAX_CSTR_LEN: usize = 512;
/// Hard ceiling on descriptors walked when the directory declares no usable
/// size (a value too small to hold one descriptor). Real images have a
/// terminator; this only bounds a malformed one.
const MAX_DESCRIPTORS: usize = 4096;

/// Read a `u16` at an unaligned offset.
///
/// # Safety
/// `p` must point to at least 2 readable bytes.
unsafe fn u16_at(p: *const u8) -> u16 {
    // SAFETY: caller guarantees 2 readable bytes.
    u16::from_le(unsafe { core::ptr::read_unaligned(p as *const u16) })
}

/// Read a `u32` at an unaligned offset.
///
/// # Safety
/// `p` must point to at least 4 readable bytes.
unsafe fn u32_at(p: *const u8) -> u32 {
    // SAFETY: caller guarantees 4 readable bytes.
    u32::from_le(unsafe { core::ptr::read_unaligned(p as *const u32) })
}

/// Read a `usize` (a PE32+ thunk) at an unaligned offset.
///
/// # Safety
/// `p` must point to at least `size_of::<usize>()` readable bytes.
unsafe fn usize_at(p: *const u8) -> usize {
    // SAFETY: caller guarantees `size_of::<usize>()` readable bytes.
    usize::from_le(unsafe { core::ptr::read_unaligned(p as *const usize) })
}

/// Read a NUL-terminated ASCII string at `p`, scanning at most `max` bytes.
///
/// # Safety
/// `p` must point to at least `max` readable bytes.
unsafe fn cstr_at(p: *const u8, max: usize) -> String {
    let mut len = 0usize;
    // SAFETY: the caller guarantees `max` readable bytes; we stop at the NUL or
    // at the `min(max, MAX_CSTR_LEN)` cap against a malformed image.
    while len < max && len < MAX_CSTR_LEN && unsafe { *p.add(len) } != 0 {
        len += 1;
    }
    // SAFETY: `len` bytes were verified readable above.
    let bytes = unsafe { core::slice::from_raw_parts(p, len) };
    String::from_utf8_lossy(bytes).into_owned()
}

/// True when `[off, off + need)` lies within `[0, end)`; also false when the
/// addition would overflow.
fn in_bounds(off: usize, need: usize, end: usize) -> bool {
    off <= end && need <= end - off
}

/// Read the `(RVA, size)` of data-directory entry `index`, or `None` when the
/// optional header declares fewer directories than `index + 1`, or the entry
/// lies outside `end`.
///
/// # Safety
/// `base` must point at an image whose optional header starts at `opt`, and at
/// least `end` readable bytes.
unsafe fn data_dir(base: *const u8, opt: usize, end: usize, index: usize) -> Option<(u32, u32)> {
    if !in_bounds(opt + OPT_NUM_DIR_OFF, 4, end) {
        return None;
    }
    // SAFETY: the count field is inside the bounded optional header.
    let count = unsafe { u32_at(base.add(opt + OPT_NUM_DIR_OFF)) } as usize;
    if index >= count {
        return None;
    }
    // SAFETY: `index < count`, so this entry is inside the declared array.
    let entry = opt + OPT_DATA_DIR_OFF + index * DATA_DIR_ENTRY;
    if !in_bounds(entry, DATA_DIR_ENTRY, end) {
        return None;
    }
    // SAFETY: a data-directory entry is 8 readable bytes within `end`.
    Some(unsafe {
        (
            u32_at(base.add(entry)),
            u32_at(base.add(entry + DATA_DIR_SIZE_OFF)),
        )
    })
}

/// Push every named import of one descriptor.
///
/// `lookup`/`iat` are byte offsets (RVAs) of the name table and the
/// import-address table; both are PE32+ 8-byte thunk arrays terminated by a zero
/// entry. `end` is the bound of the image, so neither the thunk array nor a
/// function name can be read past it even when the terminator is missing.
///
/// # Safety
/// `base` must point at `end` readable bytes of the image.
unsafe fn walk_thunks(
    base: *const u8,
    lookup: usize,
    iat: usize,
    end: usize,
    dll: &str,
    delay: bool,
    out: &mut Vec<ImportRef>,
) {
    if !in_bounds(lookup, THUNK_SIZE, end) || !in_bounds(iat, THUNK_SIZE, end) {
        return;
    }
    // Every entry is inside `end`, so the loop cannot run past the image even
    // without an all-zero terminator.
    let max = (end - lookup) / THUNK_SIZE;
    let mut i = 0usize;
    while i < max {
        // SAFETY: entry `i` is inside `end` (bounded by `max`).
        let thunk = unsafe { usize_at(base.add(lookup + i * THUNK_SIZE)) };
        if thunk == 0 {
            break;
        }
        // Skip ordinal imports (high bit set): they carry no name to match a
        // hook against. The walk must not stop on one, or a named import after
        // it is lost.
        if thunk & (1usize << (usize::BITS - 1)) == 0 {
            // The thunk is an RVA to an IMAGE_IMPORT_BY_NAME (2-byte hint then
            // the name). An RVA outside the image is unreadable; skip it.
            if in_bounds(thunk, 3, end) {
                // SAFETY: the name bytes are inside `end`.
                let func = unsafe { cstr_at(base.add(thunk + 2), end - (thunk + 2)) };
                // SAFETY: `iat`'s entry `i` is inside `end` (bounded by `max`).
                let slot = unsafe { base.add(iat + i * THUNK_SIZE) } as *mut usize;
                out.push(ImportRef {
                    dll: dll.to_string(),
                    func,
                    slot,
                    delay,
                });
            }
        }
        i += 1;
    }
}

/// The three RVAs one import descriptor contributes.
struct DescFields {
    /// RVA of the DLL name.
    name: u32,
    /// RVA of the lookup (name/thunk) table.
    lookup: u32,
    /// RVA of the import-address table (the slots to patch).
    iat: u32,
}

/// The outcome of parsing one import descriptor.
enum Descriptor {
    /// The all-zero terminator; stop the walk.
    Terminator,
    /// Present but unusable in a 64-bit process (a legacy non-RVA-based delay
    /// descriptor, whose fields are absolute 32-bit VAs); skip it.
    Skip,
    /// A usable descriptor.
    Fields(DescFields),
}

/// Parse a standard `IMAGE_IMPORT_DESCRIPTOR` (20 bytes).
///
/// # Safety
/// `desc` must point at 20 readable bytes inside the mapped image.
unsafe fn read_standard_descriptor(desc: *const u8) -> Descriptor {
    // SAFETY: the caller guarantees the descriptor is readable.
    let original_first_thunk = unsafe { u32_at(desc.add(STD_ORIGINAL_FIRST_THUNK_OFF)) };
    let name = unsafe { u32_at(desc.add(STD_NAME_OFF)) };
    let first_thunk = unsafe { u32_at(desc.add(STD_FIRST_THUNK_OFF)) };
    if original_first_thunk == 0 && name == 0 && first_thunk == 0 {
        return Descriptor::Terminator;
    }
    // The lookup table is OriginalFirstThunk, falling back to FirstThunk.
    let lookup = if original_first_thunk != 0 {
        original_first_thunk
    } else {
        first_thunk
    };
    Descriptor::Fields(DescFields {
        name,
        lookup,
        iat: first_thunk,
    })
}

/// Parse an `IMAGE_DELAYLOAD_DESCRIPTOR` (32 bytes).
///
/// # Safety
/// `desc` must point at 32 readable bytes inside the mapped image.
unsafe fn read_delay_descriptor(desc: *const u8) -> Descriptor {
    // SAFETY: the caller guarantees the descriptor is readable.
    let attrs = unsafe { u32_at(desc.add(DELAY_ATTRS_OFF)) };
    let name = unsafe { u32_at(desc.add(DELAY_DLL_NAME_OFF)) };
    let iat = unsafe { u32_at(desc.add(DELAY_IAT_OFF)) };
    let lookup = unsafe { u32_at(desc.add(DELAY_INT_OFF)) };
    if attrs == 0 && name == 0 && iat == 0 && lookup == 0 {
        return Descriptor::Terminator;
    }
    // A non-RVA-based descriptor stores absolute (32-bit) VAs, which cannot be
    // relocated in a 64-bit process: decline it rather than add the module base.
    if attrs & 1 == 0 {
        return Descriptor::Skip;
    }
    Descriptor::Fields(DescFields { name, lookup, iat })
}

/// Walk one import directory. `delay` selects the delay-load descriptor layout
/// (32 bytes: `Attributes`/`DllName`/`ModuleHandle`/`IAT`/`INT`/…) over the
/// standard one (20 bytes: `OriginalFirstThunk`/…/`Name`/`FirstThunk`).
///
/// Every RVA is bounded by `end` (the image size): a lying directory `Size`, an
/// unterminated descriptor array, or a descriptor offset past the image all stop
/// the walk instead of reading out of bounds.
///
/// # Safety
/// `base` must point at `end` readable bytes and `opt` be the optional-header
/// offset within them.
unsafe fn walk_import_directory(
    base: *const u8,
    opt: usize,
    end: usize,
    index: usize,
    desc_size: usize,
    delay: bool,
    out: &mut Vec<ImportRef>,
) {
    // SAFETY: `opt` is this image's optional header, bounded by `end`.
    let Some((rva, size)) = (unsafe { data_dir(base, opt, end, index) }) else {
        return;
    };
    let rva = rva as usize;
    if rva == 0 || !in_bounds(rva, desc_size, end) {
        return;
    }
    // Bound the walk by the declared size when usable, by the image otherwise; a
    // value too small to hold one descriptor (e.g. 0) falls back to the image
    // bound, and `MAX_DESCRIPTORS` backstops a malformed one.
    let max_by_image = (end - rva) / desc_size;
    let declared = if (size as usize) >= desc_size {
        (size as usize / desc_size).min(MAX_DESCRIPTORS)
    } else {
        MAX_DESCRIPTORS
    }
    .min(max_by_image);
    let mut off = rva;
    let mut n = 0usize;
    while n < declared {
        // SAFETY: descriptor `n` is inside `end` (bounded by `declared`).
        let parsed = if delay {
            unsafe { read_delay_descriptor(base.add(off)) }
        } else {
            unsafe { read_standard_descriptor(base.add(off)) }
        };
        let DescFields { name, lookup, iat } = match parsed {
            Descriptor::Terminator => break,
            Descriptor::Skip => {
                off += desc_size;
                n += 1;
                continue;
            }
            Descriptor::Fields(f) => f,
        };
        let name = name as usize;
        let lookup = lookup as usize;
        let iat = iat as usize;
        // A descriptor whose name/lookup/iat RVAs are zero or outside the image
        // contributes nothing; skip it rather than dereference a bad pointer.
        if name != 0
            && lookup != 0
            && iat != 0
            && in_bounds(name, 1, end)
            && in_bounds(lookup, THUNK_SIZE, end)
            && in_bounds(iat, THUNK_SIZE, end)
        {
            // SAFETY: `name` is a bounded RVA to a NUL-terminated DLL name.
            let dll = unsafe { cstr_at(base.add(name), end - name) };
            // SAFETY: `lookup`/`iat` are bounded RVAs to parallel thunk arrays.
            unsafe { walk_thunks(base, lookup, iat, end, &dll, delay, out) };
        }
        off += desc_size;
        n += 1;
    }
}

/// Walk the standard and delay-load import directories of the module at `base`.
///
/// # Safety
/// `base` must be the load address of a valid, mapped PE image.
pub unsafe fn imports(base: *mut u8) -> Vec<ImportRef> {
    // A live module: the OS maps the whole image, so trust the loader-validated
    // header (with a loose `e_lfanew` ceiling) and bound by `SizeOfImage`.
    unsafe { imports_bounded(base, usize::MAX) }
}

/// Bounds-aware import walk. `len` is the number of readable bytes at `base`
/// (the caller's buffer); [`imports`] passes `usize::MAX` for a live mapping.
/// Every read is bounded by `min(len, SizeOfImage)`, so a truncated or
/// malformed image yields no (or partial) imports and never reads out of
/// bounds.
///
/// # Safety
/// `base` must point at `len` readable bytes (or, for `usize::MAX`, a live
/// mapped PE image).
pub(crate) unsafe fn imports_bounded(base: *mut u8, len: usize) -> Vec<ImportRef> {
    let mut out = Vec::new();
    if base.is_null() {
        return out;
    }
    let base = base as *const u8;
    // DOS header.
    if !in_bounds(0, 2, len) || unsafe { u16_at(base) } != DOS_MAGIC {
        return out;
    }
    if !in_bounds(DOS_E_LFANEW_OFF, 4, len) {
        return out;
    }
    // SAFETY: `e_lfanew` is inside `len`.
    let e_lfanew = unsafe { u32_at(base.add(DOS_E_LFANEW_OFF)) } as usize;
    // The NT headers must sit past the DOS header and inside a sane envelope; a
    // corrupt `e_lfanew` must not send the read out of bounds.
    let nt_need = NT_SIGNATURE_SIZE + FILE_HEADER_SIZE + 2;
    if !(DOS_HEADER_SIZE..=MAX_NT_OFFSET).contains(&e_lfanew) || !in_bounds(e_lfanew, nt_need, len)
    {
        return out;
    }
    // SAFETY: the NT signature is inside `len`.
    if unsafe { u32_at(base.add(e_lfanew)) } != PE_SIGNATURE {
        return out;
    }
    let opt = e_lfanew + NT_SIGNATURE_SIZE + FILE_HEADER_SIZE;
    // SAFETY: `opt` is inside the NT headers verified above.
    if unsafe { u16_at(base.add(opt)) } != PE32_PLUS_MAGIC {
        return out;
    }
    // Bound every subsequent RVA by the image size the optional header declares
    // (and by the caller's length, so a truncated buffer cannot be over-read).
    let soi_off = opt + OPT_SIZE_OF_IMAGE_OFF;
    let end = if in_bounds(soi_off, 4, len) {
        // SAFETY: `SizeOfImage` is inside `len`.
        let soi = unsafe { u32_at(base.add(soi_off)) } as usize;
        if soi == 0 {
            len
        } else {
            soi.min(len)
        }
    } else {
        len
    };
    if end == 0 {
        return out;
    }
    // Standard import directory (index 1).
    // SAFETY: `base`/`opt` are a bounded PE32+ image.
    unsafe {
        walk_import_directory(
            base,
            opt,
            end,
            IMPORT_DIR_INDEX,
            IMPORT_DESC_SIZE,
            false,
            &mut out,
        )
    };
    // Delay-load import directory (index 13): a DLL imported through a delay
    // descriptor is absent from the standard table and would otherwise be missed.
    // SAFETY: as above.
    unsafe {
        walk_import_directory(
            base,
            opt,
            end,
            DELAY_IMPORT_DIR_INDEX,
            DELAY_DESC_SIZE,
            true,
            &mut out,
        )
    };
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Minimal in-memory PE32+ image whose single import descriptor lists two
    /// named imports from one DLL, so the parser's thunk stride is exercised.
    fn two_import_image() -> Vec<u8> {
        // Layout (RVAs): NT at 0x80, optional header at 0x98, data directory
        // import entry at 0x110 -> 0x200; one descriptor at 0x200 (terminator
        // at 0x214); lookup table at 0x300; IAT at 0x500; DLL name at 0x400;
        // the two IMAGE_IMPORT_BY_NAME records at 0x600 / 0x620.
        let mut b = vec![0u8; 0x700];
        let w16 = |b: &mut [u8], at: usize, v: u16| b[at..at + 2].copy_from_slice(&v.to_le_bytes());
        let w32 = |b: &mut [u8], at: usize, v: u32| b[at..at + 4].copy_from_slice(&v.to_le_bytes());
        let w64 = |b: &mut [u8], at: usize, v: u64| b[at..at + 8].copy_from_slice(&v.to_le_bytes());
        b[0] = b'M';
        b[1] = b'Z';
        w32(&mut b, 0x3C, 0x80); // e_lfanew
        b[0x80..0x84].copy_from_slice(b"PE\0\0");
        w16(&mut b, 0x98, PE32_PLUS_MAGIC); // optional-header magic
        w32(&mut b, 0xD0, 0x700); // SizeOfImage (optional header + 0x38); bounds every RVA
        w32(&mut b, 0x104, 16); // NumberOfRvaAndSizes (data directory present)
        w32(&mut b, 0x110, 0x200); // import data-directory RVA
        w32(&mut b, 0x114, 0x28); // import directory Size (2 descriptors incl. terminator)
                                  // Import descriptor.
        w32(&mut b, 0x200, 0x300); // OriginalFirstThunk (lookup)
        w32(&mut b, 0x20C, 0x400); // Name
        w32(&mut b, 0x210, 0x500); // FirstThunk (IAT)
                                   // Lookup table: two name RVAs, then terminator.
        w64(&mut b, 0x300, 0x600);
        w64(&mut b, 0x308, 0x620);
        // DLL name.
        b[0x400..0x40C].copy_from_slice(b"TESTDLL.dll\0");
        // IMAGE_IMPORT_BY_NAME: 2-byte hint then the name.
        w16(&mut b, 0x600, 0);
        b[0x602..0x608].copy_from_slice(b"Alpha\0");
        w16(&mut b, 0x620, 0);
        b[0x622..0x627].copy_from_slice(b"Beta\0");
        b
    }

    #[test]
    fn reads_every_import_in_one_descriptor_not_just_the_first() {
        // Catches: a 4-byte thunk stride in the lookup table. PE32+ thunks are
        // 8 bytes; reading 4 bytes makes `i == 1` see the high half of the first
        // entry (zero) and stop, so only the *first* import of each DLL is ever
        // seen. A target whose hooked import is not first (e.g. `gdi32.dll!
        // SwapBuffers` after `ChoosePixelFormat`) is then silently never hooked.
        let mut image = two_import_image();
        // SAFETY: `image` is a well-formed PE32+ image per `two_import_image`.
        let list = unsafe { imports(image.as_mut_ptr()) };
        let names: Vec<&str> = list.iter().map(|i| i.func.as_str()).collect();
        assert!(
            names.contains(&"Alpha"),
            "first import must be read: {names:?}"
        );
        assert!(
            names.contains(&"Beta"),
            "second import must be read too: {names:?}"
        );
        assert_eq!(names.len(), 2, "exactly the two imports: {names:?}");
        assert!(
            list.iter().all(|i| !i.delay),
            "a standard-directory import must not be flagged as delay-loaded"
        );
    }

    #[test]
    fn rejects_null_and_non_pe_bases() {
        // SAFETY: null base is explicitly handled; a zero buffer is not a PE.
        assert!(unsafe { imports(core::ptr::null_mut()) }.is_empty());
        let junk = [0u8; 4096];
        // SAFETY: the buffer is readable; it is not a PE image, so the parser
        // must reject it via the MZ check rather than read out of bounds.
        assert!(unsafe { imports(junk.as_ptr() as *mut u8) }.is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn parses_the_current_module_imports() {
        // SAFETY: `GetModuleHandleW(None)` returns this module's base address.
        let base = unsafe {
            windows::Win32::System::LibraryLoader::GetModuleHandleW(None)
                .expect("module handle")
                .0 as *mut u8
        };
        // SAFETY: `base` is this process's own mapped image.
        let list = unsafe { imports(base) };
        println!("CURRENT MODULE IMPORTS: {}", list.len());
        for i in &list {
            if i.func.contains("D3D11") || i.func.contains("Swap") || i.func.contains("Direct3D") {
                println!("SELF IMPORT {}!{}", i.dll, i.func);
            }
        }
        assert!(!list.is_empty(), "a Windows exe imports something");
        assert!(
            list.iter().all(|i| !i.dll.is_empty() && !i.func.is_empty()),
            "every import has a dll and a function name"
        );
        assert!(
            list.iter()
                .any(|i| i.dll.to_ascii_lowercase().ends_with(".dll")),
            "import DLL names look like DLL names"
        );
    }

    /// Minimal PE32+ image whose **delay-load** import directory (index 13)
    /// lists two named imports (one per descriptor), so the delay descriptor
    /// walk and its 8-byte thunk stride are exercised. The standard import
    /// directory (index 1) is empty.
    ///
    /// `num_dirs` sets `NumberOfRvaAndSizes`; `dir_size` the delay directory's
    /// `Size` (32 bytes per descriptor; the third descriptor at 0x240 is the
    /// all-zero terminator).
    fn delay_import_image(num_dirs: u32, dir_size: u32) -> Vec<u8> {
        // Layout (RVAs): NT 0x80, optional header 0x98, data dir 0x108; delay
        // entry at 0x170 -> 0x200. Descriptors at 0x200/0x220, terminator at
        // 0x240. DLL name 0x300; INTs 0x400/0x420; IATs 0x500/0x520; names
        // 0x600 ("Gamma") / 0x620 ("Epsilon").
        let mut b = vec![0u8; 0x700];
        let w16 = |b: &mut [u8], at: usize, v: u16| b[at..at + 2].copy_from_slice(&v.to_le_bytes());
        let w32 = |b: &mut [u8], at: usize, v: u32| b[at..at + 4].copy_from_slice(&v.to_le_bytes());
        let w64 = |b: &mut [u8], at: usize, v: u64| b[at..at + 8].copy_from_slice(&v.to_le_bytes());
        b[0] = b'M';
        b[1] = b'Z';
        w32(&mut b, 0x3C, 0x80); // e_lfanew
        b[0x80..0x84].copy_from_slice(b"PE\0\0");
        w16(&mut b, 0x98, PE32_PLUS_MAGIC); // optional-header magic
        w32(&mut b, 0xD0, 0x700); // SizeOfImage (optional header + 0x38); bounds every RVA
        w32(&mut b, 0x104, num_dirs); // NumberOfRvaAndSizes
        w32(&mut b, 0x170, 0x200); // delay-import data-directory RVA
        w32(&mut b, 0x174, dir_size); // delay-import data-directory Size
                                      // Descriptor 0 (RVA-based): Attributes=1.
        w32(&mut b, 0x200, 1);
        w32(&mut b, 0x204, 0x300); // DllNameRVA
        w32(&mut b, 0x20C, 0x500); // ImportAddressTableRVA
        w32(&mut b, 0x210, 0x400); // ImportNameTableRVA
                                   // Descriptor 1 (RVA-based): Attributes=1.
        w32(&mut b, 0x220, 1);
        w32(&mut b, 0x224, 0x300);
        w32(&mut b, 0x22C, 0x520);
        w32(&mut b, 0x230, 0x420);
        // Descriptor 2 at 0x240 stays all-zero: the terminator.
        b[0x300..0x30D].copy_from_slice(b"DELAYDLL.dll\0");
        w64(&mut b, 0x400, 0x600);
        w64(&mut b, 0x408, 0);
        w64(&mut b, 0x420, 0x620);
        w64(&mut b, 0x428, 0);
        w16(&mut b, 0x600, 0);
        b[0x602..0x608].copy_from_slice(b"Gamma\0");
        w16(&mut b, 0x620, 0);
        b[0x622..0x62A].copy_from_slice(b"Epsilon\0");
        b
    }

    #[test]
    fn reads_delay_loaded_imports_from_descriptor_13() {
        // Catches: an import reader that only walks the standard import directory
        // (index 1). A DLL imported via the delay-load directory (index 13) is
        // absent from the standard table, so an IAT hook would silently never
        // patch it — a delay-loaded `d3d9.dll`/`dxgi.dll`/`opengl32.dll` would be
        // captured zero times.
        let mut image = delay_import_image(16, 96); // 3 descriptors (incl. terminator)
        let base = image.as_mut_ptr();
        // SAFETY: `image` is a well-formed PE32+ image with a delay directory.
        let list = unsafe { imports(base) };
        let names: Vec<&str> = list.iter().map(|i| i.func.as_str()).collect();
        assert!(
            names.contains(&"Gamma") && names.contains(&"Epsilon"),
            "both delay imports must be read: {names:?}"
        );
        assert_eq!(names.len(), 2, "exactly the two delay imports: {names:?}");
        assert!(
            list.iter().all(|i| i.delay),
            "an import from the delay directory must be flagged as delay-loaded"
        );
        // The slot is the delay IAT: `base + ImportAddressTableRVA + i * 8` — an
        // RVA off the module base, not a file offset. Both 8-byte slots are IATs.
        assert_eq!(list[0].slot, (base as usize + 0x500) as *mut usize);
        assert_eq!(list[1].slot, (base as usize + 0x520) as *mut usize);
    }

    #[test]
    fn stops_at_the_declared_delay_directory_size() {
        // Catches: ignoring the data directory's `Size` and walking until an
        // all-zero descriptor. A truncated/malformed directory (no terminator
        // within the declared size) would otherwise run off the array into
        // unrelated image bytes. Declared size 32 = one descriptor only.
        let mut image = delay_import_image(16, 32);
        let list = unsafe { imports(image.as_mut_ptr()) };
        let names: Vec<&str> = list.iter().map(|i| i.func.as_str()).collect();
        assert_eq!(
            names,
            ["Gamma"],
            "only the descriptor inside the declared size is read: {names:?}"
        );
    }

    #[test]
    fn ignores_a_data_directory_the_optional_header_does_not_declare() {
        // Catches: reading data directory 13 when `NumberOfRvaAndSizes` says the
        // optional header has fewer entries — that reads optional-header padding
        // as a descriptor. Decline the directory instead.
        let mut image = delay_import_image(2, 96); // header declares only 2 dirs
        let list = unsafe { imports(image.as_mut_ptr()) };
        assert!(
            list.is_empty(),
            "an undeclared delay directory must be ignored: {:?}",
            list.iter().map(|i| &i.func).collect::<Vec<_>>()
        );
    }

    #[test]
    fn skips_a_non_rva_based_delay_descriptor_instead_of_misreading_it() {
        // Catches: treating a legacy non-RVA-based delay descriptor's fields as
        // RVAs. In that format the fields are absolute (32-bit) VAs; in a 64-bit
        // process they cannot be relocated, so the descriptor must be declined,
        // not added to the module base.
        let mut image = delay_import_image(16, 96);
        let w32 = |b: &mut [u8], at: usize, v: u32| b[at..at + 4].copy_from_slice(&v.to_le_bytes());
        w32(&mut image, 0x200, 0); // clear the RvaBased attribute
        w32(&mut image, 0x220, 0);
        let list = unsafe { imports(image.as_mut_ptr()) };
        assert!(
            list.is_empty(),
            "a non-RVA-based delay descriptor is not parseable in 64-bit: {:?}",
            list.iter().map(|i| &i.func).collect::<Vec<_>>()
        );
    }

    #[test]
    fn skips_an_ordinal_import_but_still_reads_the_named_ones() {
        // Catches: an ordinal (high-bit) lookup entry stopping the walk instead
        // of being skipped, or a wrong stride that misreads the named import
        // that follows it. Ordinal-only imports carry no name to match a hook
        // against; a named import after one must still be read.
        let mut image = two_import_image();
        let w64 = |b: &mut [u8], at: usize, v: u64| b[at..at + 8].copy_from_slice(&v.to_le_bytes());
        w64(&mut image, 0x300, 0x8000_0000_0000_0042); // lookup[0] = ordinal 0x42
        let list = unsafe { imports(image.as_mut_ptr()) };
        let names: Vec<&str> = list.iter().map(|i| i.func.as_str()).collect();
        assert_eq!(
            names,
            ["Beta"],
            "the ordinal is skipped and the named import after it is read: {names:?}"
        );
    }

    #[test]
    fn refuses_an_import_rva_beyond_the_declared_image() {
        // Catches: ignoring `SizeOfImage` when walking the import directory. A
        // directory RVA at or past the image end points at unrelated memory;
        // following it reads out of bounds and fabricates imports. It must fail
        // open instead.
        let mut image = two_import_image();
        image[0xD0..0xD4].copy_from_slice(&0x180u32.to_le_bytes()); // SizeOfImage < import RVA 0x200
        let list = unsafe { imports(image.as_mut_ptr()) };
        assert!(
            list.is_empty(),
            "an import directory past the image must be refused: {:?}",
            list.iter().map(|i| &i.func).collect::<Vec<_>>()
        );
    }

    #[test]
    fn refuses_a_name_rva_beyond_the_declared_image() {
        // Catches: reading a DLL/function name whose RVA is past `SizeOfImage`.
        // The descriptor itself is in-image; only the name pointer lies outside.
        // A blob read there is garbage, not a name.
        let mut image = two_import_image();
        image[0xD0..0xD4].copy_from_slice(&0x300u32.to_le_bytes()); // SizeOfImage < DLL-name RVA 0x400
        let list = unsafe { imports(image.as_mut_ptr()) };
        assert!(
            list.is_empty(),
            "a name RVA past the image must be refused: {:?}",
            list.iter().map(|i| &i.func).collect::<Vec<_>>()
        );
    }

    #[test]
    fn skips_a_descriptor_with_a_zero_thunk_rva() {
        // Catches: treating a descriptor with no IAT slot (FirstThunk == 0) as a
        // usable import — the "slot" would be the image base, so an IAT hook
        // would patch arbitrary image bytes. Such a descriptor carries no name to
        // match anyway and must be skipped.
        let mut image = two_import_image();
        image[0x210..0x214].copy_from_slice(&0u32.to_le_bytes()); // FirstThunk (IAT) = 0
        let list = unsafe { imports(image.as_mut_ptr()) };
        assert!(list.is_empty(), "a descriptor with no IAT slot is skipped");
    }

    #[test]
    fn refuses_a_truncated_header_without_reading_past_the_buffer() {
        // Catches: reading the NT header at an `e_lfanew` past a truncated
        // buffer. With no length bound the read lands outside the allocation
        // (OOB / access violation). The bounded entry point must fail open.
        let mut buf = vec![0u8; 0x40];
        buf[0] = b'M';
        buf[1] = b'Z';
        buf[0x3C..0x40].copy_from_slice(&0x1000u32.to_le_bytes()); // e_lfanew past the end
        let list = unsafe { imports_bounded(buf.as_mut_ptr(), buf.len()) };
        assert!(list.is_empty(), "a truncated header must yield no imports");
    }

    #[test]
    fn a_length_bound_cuts_the_walk_off_before_the_names() {
        // Catches: walking descriptors/thunks past the readable length. The
        // delay image places its names at 0x300+; given only 0x300 readable
        // bytes, no import may be produced (the name RVAs are out of range).
        let mut image = delay_import_image(16, 96);
        let list = unsafe { imports_bounded(image.as_mut_ptr(), 0x300) };
        assert!(
            list.is_empty(),
            "no import may be read past the given length: {:?}",
            list.iter().map(|i| &i.func).collect::<Vec<_>>()
        );
    }
}
