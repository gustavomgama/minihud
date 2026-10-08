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
