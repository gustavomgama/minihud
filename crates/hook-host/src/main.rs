//! hook-host: launcher/injector + frame reader.
//!
//! Flows:
//! - `hook-host launch <exe> [-- args]`: spawn suspended, inject
//!   hook-rt pre-entry, resume, print fps lines until the game exits.
//! - `hook-host attach <pid>`: inject into a RUNNING process. Explicit
//!   opt-in only, AC-deny checked first, logged loudly.
//!
//! Anti-cheat stance: informed consent + default-deny, never stealth.
//! If known AC modules are present, we refuse (attach) or unhook and
//! walk away leaving the game running (launcher). Bans are possible
//! with any usermode hook; protected titles stay out by default.
//!
//! Every wait is bounded; every failure resumes or releases the game
//! instead of leaving it suspended.

use hook_ipc as ipc;
use std::ffi::c_void;
use std::time::{Duration, Instant};
use windows::core::{PCSTR, PCWSTR, PWSTR, BOOL};
use windows::Win32::Foundation::*;
use windows::Win32::System::Diagnostics::Debug::WriteProcessMemory;
use windows::Win32::System::Diagnostics::ToolHelp::*;
use windows::Win32::System::LibraryLoader::{GetModuleHandleW, GetProcAddress, LoadLibraryW};
use windows::Win32::System::Memory::*;
use windows::Win32::System::Threading::*;

const WAIT_STEP_MS: u32 = 10_000;
/// Module-name fragments that refuse injection outright.
const AC_MODULES: &[&str] = &[
    "easyanticheat",
    "bedaisy",
    "battleye",
    "vgc",
    "vgk",
    "vanguard",
];

fn wstr(s: &str) -> Vec<u16> {
    let mut v: Vec<u16> = s.encode_utf16().collect();
    v.push(0);
    v
}

fn usage() -> ! {
    eprintln!("usage:");
    eprintln!("  hook-host launch <exe> [-- args...]   spawn suspended, inject, resume, report fps");
    eprintln!("  hook-host attach <pid>                 inject into a running process (explicit opt-in)");
    std::process::exit(2);
}

fn main() {
    let argv: Vec<String> = std::env::args().collect();
    if argv.len() < 3 {
        usage();
    }
    let rc = match argv[1].as_str() {
        "launch" => cmd_launch(&argv[2..]),
        "attach" => cmd_attach(&argv[2..]),
        _ => usage(),
    };
    std::process::exit(rc);
}

// ---------------------------------------------------------------------------
// Injection primitives (bounded waits throughout).
// ---------------------------------------------------------------------------

fn access_rights() -> PROCESS_ACCESS_RIGHTS {
    PROCESS_ACCESS_RIGHTS(
        PROCESS_CREATE_THREAD.0
            | PROCESS_QUERY_INFORMATION.0
            | PROCESS_VM_OPERATION.0
            | PROCESS_VM_WRITE.0
            | PROCESS_VM_READ.0,
    )
}

/// Address of an exported hook-rt function inside the TARGET process:
/// remote_base + (local_fn - local_base).
unsafe fn remote_proc(proc: HANDLE, local_dll: &str, export: &str) -> windows::core::Result<usize> {
    // Our x64 host cannot reach into x86 targets (phase 4 owns that).
    let mut wow = BOOL(0);
    IsWow64Process(proc, &mut wow)?;
    if wow.0 != 0 {
        eprintln!("refusing 32-bit target from 64-bit host (x86 injector is phase 4)");
        return Err(windows::core::Error::from_win32());
    }
    let local = match GetModuleHandleW(PCWSTR(wstr(local_dll).as_ptr())) {
        Ok(h) => h,
        // Not loaded locally yet: load it (kept loaded for the session).
        Err(_) => match std::path::Path::new(local_dll).canonicalize() {
            Ok(p) => {
                let w = wstr(&p.to_string_lossy());
                LoadLibraryW(PCWSTR(w.as_ptr()))?
            }
            Err(_) => {
                let w = wstr(local_dll);
                LoadLibraryW(PCWSTR(w.as_ptr()))?
            }
        },
    };
    let export_c = format!("{export}\0");
    let local_fn = GetProcAddress(local, PCSTR(export_c.as_ptr())).ok_or_else(|| {
        eprintln!("{export} not exported by local {local_dll}");
        windows::core::Error::from_win32()
    })?;
    let snap = CreateToolhelp32Snapshot(TH32CS_SNAPMODULE, GetProcessId(proc))?;
    let mut me = MODULEENTRY32W {
        dwSize: std::mem::size_of::<MODULEENTRY32W>() as u32,
        ..Default::default()
    };
    let mut base = 0usize;
    if Module32FirstW(snap, &mut me).is_ok() {
        loop {
            let name = String::from_utf16_lossy(&me.szModule)
                .trim_end_matches('\0')
                .to_lowercase();
            if name == local_dll.to_lowercase() {
                base = me.modBaseAddr as usize;
                break;
            }
            if Module32NextW(snap, &mut me).is_err() {
                break;
            }
        }
    }
    let _ = CloseHandle(snap);
    if base == 0 {
        eprintln!("{local_dll} not loaded in target (hook-rt must be injected first for mh_install)");
        return Err(windows::core::Error::from_win32());
    }
    Ok(base.wrapping_add((local_fn as usize).wrapping_sub(local.0 as usize)))
}

/// Load hook-rt.dll into the target via remote LoadLibraryW. Bounded 10s.
unsafe fn inject_dll(proc: HANDLE, dll_path: &str) -> windows::core::Result<()> {
    let w = wstr(dll_path);
    let bytes = w.len() * 2;
    let remote = VirtualAllocEx(
        proc,
        None,
        bytes,
        VIRTUAL_ALLOCATION_TYPE(MEM_COMMIT.0 | MEM_RESERVE.0),
        PAGE_READWRITE,
    );
    if remote.is_null() {
        return Err(windows::core::Error::from_win32());
    }
    let mut written = 0usize;
    WriteProcessMemory(
        proc,
        remote as *const c_void,
        w.as_ptr() as *const c_void,
        bytes,
        Some(&mut written as *mut usize),
    )?;
    let k32 = GetModuleHandleW(PCWSTR(wstr("kernel32.dll").as_ptr()))?;
    let load_name = "LoadLibraryW\0";
    let load = GetProcAddress(k32, PCSTR(load_name.as_ptr())).ok_or_else(|| {
        eprintln!("LoadLibraryW not found");
        windows::core::Error::from_win32()
    })?;
    let th = CreateRemoteThread(
        proc,
        None,
        0,
        std::mem::transmute::<_, LPTHREAD_START_ROUTINE>(load),
        Some(remote as *const c_void),
        0,
        None,
    )?;
    let wait = WaitForSingleObject(th, WAIT_STEP_MS);
    let mut code = 0u32;
    let _ = GetExitCodeThread(th, &mut code);
    let _ = CloseHandle(th);
    let _ = VirtualFreeEx(proc, remote, 0, MEM_RELEASE);
    if wait != WAIT_OBJECT_0 || code == 0 {
        eprintln!("LoadLibrary remote thread failed (wait={wait:?} base={code:#x})");
        return Err(windows::core::Error::from_win32());
    }
    Ok(())
}

/// Call an exported no-arg hook-rt function inside the target. Bounded 10s.
unsafe fn remote_call(proc: HANDLE, dll: &str, export: &str) -> windows::core::Result<u32> {
    let addr = remote_proc(proc, dll, export)?;
    let th = CreateRemoteThread(
        proc,
        None,
        0,
        std::mem::transmute::<_, LPTHREAD_START_ROUTINE>(addr),
        None,
        0,
        None,
    )?;
    let wait = WaitForSingleObject(th, WAIT_STEP_MS);
    let mut code = 0u32;
    let _ = GetExitCodeThread(th, &mut code);
    let _ = CloseHandle(th);
    if wait != WAIT_OBJECT_0 {
        return Err(windows::core::Error::from_win32());
    }
    Ok(code)
}

/// True when known anti-cheat modules are loaded in the target.
unsafe fn ac_present(pid: u32) -> bool {
    let snap = match CreateToolhelp32Snapshot(TH32CS_SNAPMODULE, pid) {
        Ok(s) => s,
        Err(_) => return false,
    };
    let mut me = MODULEENTRY32W {
        dwSize: std::mem::size_of::<MODULEENTRY32W>() as u32,
        ..Default::default()
    };
    let mut hit = false;
    if Module32FirstW(snap, &mut me).is_ok() {
        loop {
            let name = String::from_utf16_lossy(&me.szModule)
                .trim_end_matches('\0')
                .to_lowercase();
            if AC_MODULES.iter().any(|m| name.contains(m)) {
                eprintln!("anti-cheat module detected: {name}");
                hit = true;
                break;
            }
            if Module32NextW(snap, &mut me).is_err() {
                break;
            }
        }
    }
    let _ = CloseHandle(snap);
    hit
}

/// File name of the hook DLL. NOTE: Rust converts the `hook-rt` crate
/// name to `hook_rt.dll` on disk — look for the real file name.
const HOOK_DLL: &str = "hook_rt.dll";

fn hook_dll_path() -> String {
    // Same directory as hook-host (release layout); dev fallback below.
    // Canonicalized: LoadLibrary chokes on `..` segments in rare cases.
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let p = dir.join(HOOK_DLL);
            if p.exists() {
                return p.to_string_lossy().into_owned();
            }
        }
    }
    let dev = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join("..")
        .join("target")
        .join("debug")
        .join(HOOK_DLL);
    std::fs::canonicalize(&dev)
        .unwrap_or(dev)
        .to_string_lossy()
        .trim_start_matches(r"\\?\")
        .to_owned()
}

// ---------------------------------------------------------------------------
// Flows.
// ---------------------------------------------------------------------------

fn cmd_launch(args: &[String]) -> i32 {
    let (exe, tail): (String, Vec<String>) = match args.iter().position(|a| a == "--") {
        Some(0) | None if args.is_empty() => usage(),
        Some(i) => (args[0].clone(), args[i + 1..].to_vec()),
        None => (args[0].clone(), args[1..].to_vec()),
    };
    let cmdline = std::iter::once(format!("\"{exe}\""))
        .chain(tail.iter().map(|a| format!("\"{a}\"")))
        .collect::<Vec<_>>()
        .join(" ");
    println!("launch: {cmdline}");
    eprintln!("[host] launching target");
    unsafe {
        let mut si = STARTUPINFOW::default();
        si.cb = std::mem::size_of::<STARTUPINFOW>() as u32;
        let mut pi = PROCESS_INFORMATION::default();
        let mut cmd: Vec<u16> = wstr(&cmdline);
        if CreateProcessW(
            PCWSTR(std::ptr::null()),
            Some(PWSTR(cmd.as_mut_ptr())),
            None,
            None,
            false,
            CREATE_SUSPENDED,
            None,
            PCWSTR(std::ptr::null()),
            &si,
            &mut pi,
        )
        .is_err()
        {
            eprintln!("CreateProcess failed");
            return 1;
        }
        let dll = hook_dll_path();
        println!("inject: {dll}");
        eprintln!("[host] injecting");
        let mut ok = inject_dll(pi.hProcess, &dll).is_ok();
        if ok {
            // Install runs on a remote thread, never in DllMain.
            match remote_call(pi.hProcess, HOOK_DLL, "mh_install") {
                Ok(0) => println!("hooks installed"),
                Ok(code) => {
                    eprintln!("mh_install failed with code {code}");
                    ok = false;
                }
                Err(_) => {
                    eprintln!("mh_install remote call failed");
                    ok = false;
                }
            }
            eprintln!("[host] install step done, ok={ok}");
        }
        // Never leave the child suspended: resume hooked or clean.
        let _ = ResumeThread(pi.hThread);
        let _ = CloseHandle(pi.hThread);
        if !ok {
            eprintln!("injection failed; game continues unhooked, host exiting");
            let _ = CloseHandle(pi.hProcess);
            return 1;
        }
        let rc = read_loop(pi.hProcess, pi.dwProcessId);
        let _ = CloseHandle(pi.hProcess);
        rc
    }
}

fn cmd_attach(args: &[String]) -> i32 {
    let pid: u32 = match args.first().and_then(|s| s.parse().ok()) {
        Some(p) => p,
        None => {
            eprintln!("attach needs a numeric pid");
            return 2;
        }
    };
    println!("ATTACH requested for pid {pid} (explicit opt-in, logged)");
    unsafe {
        if ac_present(pid) {
            eprintln!("refusing attach: anti-cheat modules present");
            return 3;
        }
        let proc = match OpenProcess(access_rights(), false, pid) {
            Ok(h) => h,
            Err(_) => {
                eprintln!("OpenProcess failed (protected/elevated target?)");
                return 1;
            }
        };
        let dll = hook_dll_path();
        if inject_dll(proc, &dll).is_err() {
            let _ = CloseHandle(proc);
            return 1;
        }
        match remote_call(proc, HOOK_DLL, "mh_install") {
            Ok(0) => println!("hooks installed"),
            other => {
                eprintln!("mh_install failed: {other:?}");
                let _ = CloseHandle(proc);
                return 1;
            }
        }
        let rc = read_loop(proc, pid);
        let _ = CloseHandle(proc);
        rc
    }
}

// ---------------------------------------------------------------------------
// Reader: shared-memory ring -> 1/sec fps lines until the game exits.
// ---------------------------------------------------------------------------

unsafe fn read_loop(proc: HANDLE, pid: u32) -> i32 {
    use std::collections::VecDeque;
    // Give the mapping a moment to appear (install just ran).
    std::thread::sleep(Duration::from_millis(500));
    let name: Vec<u16> = ipc::mapping_name(pid)
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();
    let map = match OpenFileMappingW(FILE_MAP_READ.0, false, PCWSTR(name.as_ptr())) {
        Ok(h) => h,
        Err(_) => {
            eprintln!("cannot open ring mapping (hook install failed silently?)");
            return 1;
        }
    };
    let view = MapViewOfFile(map, FILE_MAP_READ, 0, 0, ipc::MAPPING_SIZE);
    if view.Value.is_null() {
        eprintln!("MapViewOfFile failed");
        return 1;
    }
    let reader = match ipc::Reader::new(view.Value as *const u8) {
        Some(r) => r,
        None => {
            eprintln!("ring magic/version mismatch");
            return 1;
        }
    };
    let freq = reader.qpc_freq().max(1) as f64;
    let mut last_seq = 0u64;
    let mut stamps: VecDeque<u64> = VecDeque::with_capacity(2048);
    let mut lost = 0u64;
    let mut total = 0u64;
    let t0 = Instant::now();
    loop {
        // Game exit ends the loop (reader never blocks the game).
        let mut code = 0u32;
        // Exit code 259 means still running.
        if GetExitCodeProcess(proc, &mut code).is_ok() && code != 259 {
            println!("game exited with code {code}");
            break;
        }
        // The header counter is next-to-assign; assigned slots are
        // [1, counter], so the exclusive bound is counter + 1.
        let next = reader.next_seq().saturating_add(1);
        // Consume [last_seq, next). Cap per-tick work; jumps count as lost.
        let mut i = last_seq.max(1);
        if next.saturating_sub(i) > 2048 {
            lost += next - i - 2048;
            i = next - 2048;
        }
        while i < next {
            match reader.slot(i) {
                Some(s) => {
                    stamps.push_back(s.qpc_start);
                    total += 1;
                }
                None => lost += 1,
            }
            i += 1;
        }
        last_seq = next.max(1);
        while stamps.len() > 2048 {
            stamps.pop_front();
        }
        // Trailing-1s fps from validated stamps (newest valid = watermark).
        let win_from = stamps.back().copied().unwrap_or(0);
        let cutoff = win_from.saturating_sub(freq as u64);
        let n = stamps.iter().filter(|t| **t >= cutoff).count();
        let avg_ms = if n >= 2 {
            let mut v: Vec<u64> = stamps.iter().copied().filter(|t| *t >= cutoff).collect();
            v.sort_unstable();
            let sum: u64 = v.windows(2).map(|w| w[1] - w[0]).sum();
            sum as f64 / (v.len() - 1) as f64 * 1000.0 / freq
        } else {
            0.0
        };
        println!(
            "fps={} avg_ms={:.2} frames={} loss={} elapsed={}s",
            n,
            avg_ms,
            total,
            lost,
            t0.elapsed().as_secs()
        );
        std::thread::sleep(Duration::from_millis(1000));
    }
    println!("summary: frames={total} loss={lost} detour_calls={}", reader.calls());
    if lost > 0 && total > 0 {
        eprintln!(
            "note: {:.2}% slots torn (reader slower than writer bursts)",
            lost as f64 / total as f64 * 100.0
        );
    }
    0
}
