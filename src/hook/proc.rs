//! Win32 process-enumeration helpers for follow mode.
//!
//! Everything here is best-effort and fail-open: a missing handle, a denied
//! query, or an empty snapshot returns `None`/an empty list rather than
//! panicking, so the follow loop simply sees "no target" and keeps polling.

use windows::core::PWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE, STILL_ACTIVE};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, PROCESSENTRY32W, TH32CS_SNAPPROCESS,
};
use windows::Win32::System::Threading::{
    GetExitCodeProcess, OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::WindowsAndMessaging::{GetForegroundWindow, GetWindowThreadProcessId};

/// An owned process handle closed on drop.
struct ProcHandle(HANDLE);

impl Drop for ProcHandle {
    fn drop(&mut self) {
        // SAFETY: the handle was returned by `OpenProcess` and is closed once.
        unsafe {
            let _ = CloseHandle(self.0);
        }
    }
}

/// Pid of the foreground (focused) window's process, if any.
pub fn foreground_pid() -> Option<u32> {
    // SAFETY: `GetForegroundWindow` takes no arguments; the result may be null.
    let hwnd = unsafe { GetForegroundWindow() };
    if hwnd.is_invalid() {
        return None;
    }
    let mut pid: u32 = 0;
    // SAFETY: `hwnd` is a window handle and `pid` a valid out-pointer.
    unsafe {
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
    }
    (pid != 0).then_some(pid)
}

/// Full image path of `pid`, queried with `PROCESS_QUERY_LIMITED_INFORMATION`
/// (works without full access rights).
pub fn process_name(pid: u32) -> Option<String> {
    // SAFETY: opening with a pid and limited query rights.
    let handle = unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }.ok()?;
    let handle = ProcHandle(handle);
    let mut buf = [0u16; 32 * 1024];
    let mut len = buf.len() as u32;
    // SAFETY: `handle` is live; `buf`/`len` describe a valid writable buffer.
    unsafe {
        QueryFullProcessImageNameW(
            handle.0,
            PROCESS_NAME_WIN32,
            PWSTR(buf.as_mut_ptr()),
            &mut len,
        )
    }
    .ok()?;
    Some(String::from_utf16_lossy(&buf[..len as usize]))
}

/// True when `pid` is still running. Fail-open: a process we cannot open is
/// reported as not running, so a capture loop ends rather than spinning.
pub fn process_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    // SAFETY: opening with a pid and limited query rights.
    let Ok(handle) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) }) else {
        return false;
    };
    let handle = ProcHandle(handle);
    let mut code: u32 = 0;
    // SAFETY: `handle` is live and `code` a valid out-pointer.
    let ok = unsafe { GetExitCodeProcess(handle.0, &mut code) };
    ok.is_ok() && code == STILL_ACTIVE.0 as u32
}

/// Every process as `(pid, exe name)` from a `TH32CS_SNAPPROCESS` snapshot.
///
/// The name is the executable's file name (as reported by Toolhelp), not a
/// full path.
pub fn list_processes() -> Vec<(u32, String)> {
    // SAFETY: snapshotting the system process list.
    let Ok(snap) = (unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    // SAFETY: `snap` is live and `entry` is a sized, initialized struct.
    if unsafe { Process32FirstW(snap, &mut entry) }.is_ok() {
        loop {
            out.push((entry.th32ProcessID, exe_name(&entry)));
            entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
            // SAFETY: `snap` live and `entry` valid; stops at the first error/end.
            if unsafe { Process32NextW(snap, &mut entry) }.is_err() {
                break;
            }
        }
    }
    // SAFETY: `snap` is closed once.
    unsafe {
        let _ = CloseHandle(snap);
    }
    out
}

/// NUL-terminated UTF-16 exe name from a snapshot entry.
fn exe_name(e: &PROCESSENTRY32W) -> String {
    let len = e
        .szExeFile
        .iter()
        .position(|&c| c == 0)
        .unwrap_or(e.szExeFile.len());
    String::from_utf16_lossy(&e.szExeFile[..len])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exe_name_stops_at_the_nul_terminator() {
        // Catches: reading the whole fixed 260-wide buffer instead of up to the
        // NUL, so every name carries trailing junk and never matches a glob.
        let mut e = PROCESSENTRY32W::default();
        for (i, c) in "deadlock.exe".encode_utf16().enumerate() {
            e.szExeFile[i] = c;
        }
        assert_eq!(exe_name(&e), "deadlock.exe");
    }

    #[test]
    fn list_processes_includes_our_own_pid() {
        // Catches: a snapshot loop that reads zero entries (e.g. a wrong struct
        // size), which would make follow mode blind to every process.
        let own = std::process::id();
        let list = list_processes();
        assert!(
            list.iter().any(|(pid, _)| *pid == own),
            "own pid {own} missing from {} processes",
            list.len()
        );
        assert!(list.iter().all(|(_, name)| !name.is_empty()));
    }

    #[test]
    fn process_name_resolves_our_own_exe() {
        // Catches: a QueryFullProcessImageNameW buffer/length bug that returns
        // nothing for a process whose exe we can plainly see.
        let name = process_name(std::process::id()).expect("own image name");
        assert!(
            name.to_ascii_lowercase().ends_with(".exe"),
            "expected an exe path, got {name:?}"
        );
    }

    #[test]
    fn process_alive_reports_our_own_pid_and_not_an_impossible_one() {
        // Catches: a liveness check stuck at true (capture loop never ends) or
        // false (capture loop ends immediately).
        assert!(process_alive(std::process::id()), "our own pid is alive");
        assert!(!process_alive(0), "pid 0 is never a live user process");
        assert!(
            !process_alive(0xFFFF_FFFE),
            "a pid that cannot exist must read as dead"
        );
    }
}
