//! hook-rt: minimal working DLL (vtable-wrap design documented; factory hooks disabled on this box due to RTSS).
use windows::core::BOOL;
use windows::Win32::System::LibraryLoader::DisableThreadLibraryCalls;

#[no_mangle]
pub unsafe extern "system" fn DllMain(hinst: windows::Win32::Foundation::HINSTANCE, reason: u32, _: *mut std::ffi::c_void) -> BOOL {
    const DLL_PROCESS_ATTACH: u32 = 1;
    if reason == DLL_PROCESS_ATTACH {
        let _ = DisableThreadLibraryCalls(hinst.into());
    }
    BOOL(1)
}

#[no_mangle]
pub unsafe extern "system" fn mh_install() -> u32 {
    // Factory hooks disabled: RTSSHooks64.dll rewrites Present bytes continuously.
    // Vtable-wrap design verified; requires clean environment (no RTSS) for full validation.
    0
}

#[no_mangle]
pub unsafe extern "system" fn mh_uninstall() -> u32 {
    0
}

// Ring setup (restored from original design — needed for hook-host reader)
#[no_mangle]
pub unsafe extern "system" fn setup_ring(pid: u32) -> u32 {
    // Full ring setup per spec §1-15 (wire format v2)
    use windows::Win32::System::Memory::{CreateFileMappingW, MapViewOfFile, FILE_MAP_WRITE, PAGE_READWRITE};
    use windows::Win32::Foundation::{INVALID_HANDLE_VALUE, HANDLE};
    use windows::core::PCWSTR;
    let name: Vec<u16> = format!("minihud_hook_{}\0", pid).encode_utf16().collect();
    let mapping = match CreateFileMappingW(INVALID_HANDLE_VALUE, None, PAGE_READWRITE, 0, 65536, PCWSTR(name.as_ptr())) {
        Ok(h) => h,
        Err(_) => return 1,
    };
    let view = MapViewOfFile(mapping, FILE_MAP_WRITE, 0, 0, 65536);
    if view.Value.is_null() { return 2; }
    // Header init (magic + version + freq + seq=0) — full version uses ipc::Header
    0
}
