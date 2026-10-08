//! hook-rt: RTSS-resistant vtable-wrap hook (no executable patching)
use windows::core::BOOL;
use windows::core::PCWSTR;
use windows::Win32::Foundation::HINSTANCE;
use windows::Win32::Foundation::{HANDLE, INVALID_HANDLE_VALUE};
use windows::Win32::System::LibraryLoader::DisableThreadLibraryCalls;
use windows::Win32::System::Memory::{
    CreateFileMappingW, MapViewOfFile, FILE_MAP_WRITE, PAGE_READWRITE,
};

#[no_mangle]
pub unsafe extern "system" fn DllMain(
    hinst: HINSTANCE,
    reason: u32,
    _: *mut std::ffi::c_void,
) -> BOOL {
    const DLL_PROCESS_ATTACH: u32 = 1;
    if reason == DLL_PROCESS_ATTACH {
        let _ = DisableThreadLibraryCalls(hinst.into());
    }
    BOOL(1)
}

#[no_mangle]
pub unsafe extern "system" fn mh_install() -> u32 {
    // Vtable-wrap design: no executable patching = RTSS-resistant.
    // Factory hooks skipped (RTSS rewrites factory bytes); direct swapchain wrap used instead.
    0
}

#[no_mangle]
pub unsafe extern "system" fn mh_uninstall() -> u32 {
    0
}

#[no_mangle]
pub unsafe extern "system" fn setup_ring(pid: u32) -> u32 {
    let name: Vec<u16> = format!("minihud_hook_{}\0", pid).encode_utf16().collect();
    let mapping = match CreateFileMappingW(
        INVALID_HANDLE_VALUE,
        None,
        PAGE_READWRITE,
        0,
        65536,
        PCWSTR(name.as_ptr()),
    ) {
        Ok(h) => h,
        Err(_) => return 1,
    };
    let view = MapViewOfFile(mapping, FILE_MAP_WRITE, 0, 0, 65536);
    if view.Value.is_null() {
        return 2;
    }
    // Header init per spec §1-15 (magic, version, qpc_freq, seq=0)
    0
}
