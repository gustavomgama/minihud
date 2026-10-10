//! `hook-rt` — the in-process present-hook recorder injected into a target.
//!
//! It patches its own IAT entries and COM vtable slots for the exact hook set
//! in [`api`], records one `QueryPerformanceCounter`-stamped frame per present
//! into the shared ring ([`hook_ipc`]), and records swapchain metadata for a
//! future OSD. It renders nothing.
//!
//! Exports:
//! - `mh_install(param)` — install the recorder and hooks; returns the
//!   installed-API bitmask.
//! - `mh_uninstall(param)` — restore every patch.
//!
//! Both match `LPTHREAD_START_ROUTINE` so the host can call them with
//! `CreateRemoteThread`. They are panic-free across the FFI boundary.

#![cfg(windows)]

mod api;
mod detour;
mod install;
mod patch;
mod pe;
mod record;
mod vk;

use core::ffi::c_void;

/// Install the recorder and hooks. Returns the installed-API bitmask
/// (`0` on failure). Called by the host as a remote thread entry point.
#[no_mangle]
pub extern "system" fn mh_install(_param: *mut c_void) -> u32 {
    catch(|| {
        let mask = install::install();
        if mask != 0 {
            install::start_rescan_thread();
        }
        mask
    })
}

/// Restore every vtable and IAT patch. Returns `0` on success.
#[no_mangle]
pub extern "system" fn mh_uninstall(_param: *mut c_void) -> u32 {
    catch(|| {
        install::uninstall();
        0
    })
}

/// Run `f`, converting any panic into `u32::MAX` so nothing unwinds across FFI.
fn catch(f: impl FnOnce() -> u32) -> u32 {
    std::panic::catch_unwind(std::panic::AssertUnwindSafe(f)).unwrap_or(u32::MAX)
}
