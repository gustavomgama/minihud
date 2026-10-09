//! minihud build helper: `cargo xtask <build|run|dist|clean>`.
//!
//! std-only and dependency-free. It builds the `minihud` package into the
//! canonical `target/<profile>/` and stages the LibreHardwareMonitor bridge
//! assets the exe resolves exe-relative next to it, so debug and release run
//! the same way. `dist` assembles the shipping folder.

use std::env;
use std::fs::{self, OpenOptions};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

const PKG: &str = "minihud";
const EXE: &str = "minihud.exe";

/// Assets copied next to the built exe and into `dist/`.
/// `(path relative to workspace root, required)`.
const ASSETS: &[(&str, bool)] = &[
    ("tools/lhm/lhm-bridge.ps1", true),
    ("tools/lhm/LibreHardwareMonitorLib.dll", false),
];

fn main() -> ExitCode {
    let args: Vec<String> = env::args().skip(1).collect();
    match run(&args) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("xtask: {e}");
            ExitCode::FAILURE
        }
    }
}

fn run(args: &[String]) -> Result<(), String> {
    let mut it = args.iter();
    let cmd = it.next().map(String::as_str).unwrap_or("");
    if cmd.is_empty() {
        return Err("usage: cargo xtask <build|run|dist|clean> [--release] [-- <args>]".into());
    }
    let mut release = false;
    let mut passthrough: Vec<String> = Vec::new();
    let mut after_dash = false;
    for a in it {
        if after_dash {
            passthrough.push(a.clone());
        } else if a == "--" {
            after_dash = true;
        } else if a == "--release" {
            release = true;
        } else {
            return Err(format!(
                "unknown arg {a:?}\nusage: cargo xtask <build|run|dist|clean> [--release] [-- <args>]"
            ));
        }
    }

    match cmd {
        "build" => {
            build(release)?;
            stage(release)?;
            println!("built {}", exe_path(release)?.display());
        }
        "run" => {
            build(release)?;
            stage(release)?;
            let exe = exe_path(release)?;
            let status = Command::new(&exe)
                .args(&passthrough)
                .status()
                .map_err(|e| format!("run {}: {e}", exe.display()))?;
            if !status.success() {
                return Err(format!("{} exited {status}", exe.display()));
            }
        }
        "dist" => dist()?,
        "clean" => clean()?,
        other => {
            return Err(format!(
                "unknown subcommand {other:?} (build|run|dist|clean)"
            ))
        }
    }
    Ok(())
}

/// Workspace root: this crate lives at `<root>/xtask`.
fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask crate lives in <workspace>/xtask")
        .to_path_buf()
}

fn profile(release: bool) -> &'static str {
    if release {
        "release"
    } else {
        "debug"
    }
}

/// Canonical target dir: `CARGO_TARGET_DIR` if set, else `<root>/target`.
/// No alternate directories are introduced.
fn target_dir() -> PathBuf {
    match env::var_os("CARGO_TARGET_DIR") {
        Some(v) => {
            let p = PathBuf::from(v);
            if p.is_absolute() {
                p
            } else {
                env::current_dir().unwrap_or_default().join(p)
            }
        }
        None => root().join("target"),
    }
}

fn profile_dir(release: bool) -> PathBuf {
    target_dir().join(profile(release))
}

fn exe_path(release: bool) -> Result<PathBuf, String> {
    let p = profile_dir(release).join(EXE);
    if p.exists() {
        Ok(p)
    } else {
        Err(format!(
            "{} not found (build failed, or Defender quarantined it?)",
            p.display()
        ))
    }
}

fn cargo() -> Command {
    Command::new(env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
}

fn build(release: bool) -> Result<(), String> {
    let exe = profile_dir(release).join(EXE);
    if is_locked(&exe) {
        return Err(locked_msg(&exe));
    }
    let mut c = cargo();
    c.current_dir(root()).arg("build").arg("-p").arg(PKG);
    if release {
        c.arg("--release");
    }
    let status = c.status().map_err(|e| format!("spawn cargo: {e}"))?;
    if !status.success() {
        if is_locked(&exe) {
            return Err(locked_msg(&exe));
        }
        return Err(format!("cargo build failed ({status})"));
    }
    if !exe.exists() {
        return Err(format!(
            "{} was not produced (Defender may have quarantined it)",
            exe.display()
        ));
    }
    Ok(())
}

/// Copy the runtime assets next to the built exe.
fn stage(release: bool) -> Result<(), String> {
    let dir = profile_dir(release);
    fs::create_dir_all(&dir).map_err(|e| format!("create {}: {e}", dir.display()))?;
    for (rel, required) in ASSETS {
        copy_asset(rel, &dir, *required)?;
    }
    Ok(())
}

fn dist() -> Result<(), String> {
    build(true)?;
    let exe = exe_path(true)?;
    let dist = root().join("dist");
    if dist.exists() {
        fs::remove_dir_all(&dist).map_err(|e| format!("clear {}: {e}", dist.display()))?;
    }
    fs::create_dir_all(&dist).map_err(|e| format!("create {}: {e}", dist.display()))?;
    copy_file(&exe, &dist.join(EXE))?;
    for (rel, required) in ASSETS {
        copy_asset(rel, &dist, *required)?;
    }
    copy_asset("README.md", &dist, true)?;

    let abs = fs::canonicalize(&dist).unwrap_or(dist);
    println!("shipping folder: {}", abs.display());
    list_dir(&abs)?;
    Ok(())
}

fn clean() -> Result<(), String> {
    let dist = root().join("dist");
    let me = env::current_exe().ok();

    // Primary path: `cargo clean`. On Windows it cannot delete the currently
    // running xtask.exe, so a failure there is expected — finish by hand.
    let out = cargo()
        .current_dir(root())
        .arg("clean")
        .output()
        .map_err(|e| format!("spawn cargo: {e}"))?;
    if out.status.success() {
        if dist.exists() {
            fs::remove_dir_all(&dist).map_err(|e| format!("remove {}: {e}", dist.display()))?;
        }
        println!("cleaned target/ and dist/");
        return Ok(());
    }

    let mut skipped: Vec<PathBuf> = Vec::new();
    remove_tree(&target_dir(), me.as_deref(), &mut skipped)?;
    remove_tree(&dist, None, &mut skipped)?;

    let others: Vec<&PathBuf> = skipped
        .iter()
        .filter(|p| !me.as_deref().is_some_and(|m| same_file(p, m)))
        .collect();
    if others.is_empty() {
        println!("cleaned target/ and dist/ (the running xtask image is released on exit)");
        Ok(())
    } else {
        Err(format!(
            "could not remove (locked): {}",
            others
                .iter()
                .map(|p| p.display().to_string())
                .collect::<Vec<_>>()
                .join(", ")
        ))
    }
}

/// Recursively remove `path`, recording (not failing on) files we cannot
/// delete — specifically the running xtask image. Other locked files are
/// reported by the caller.
fn remove_tree(
    path: &Path,
    self_exe: Option<&Path>,
    skipped: &mut Vec<PathBuf>,
) -> Result<(), String> {
    if !path.exists() {
        return Ok(());
    }
    if path.is_dir() {
        let entries = fs::read_dir(path).map_err(|e| format!("read {}: {e}", path.display()))?;
        for entry in entries {
            let child = entry
                .map_err(|e| format!("read {}: {e}", path.display()))?
                .path();
            remove_tree(&child, self_exe, skipped)?;
        }
        match fs::remove_dir(path) {
            Ok(()) => {}
            // Non-empty because it still holds a skipped/locked child.
            Err(e) if raw(&e) == 145 || is_lock_err(&e) => {}
            Err(e) => return Err(format!("remove dir {}: {e}", path.display())),
        }
    } else if self_exe.is_some_and(|m| same_file(path, m)) {
        skipped.push(path.to_path_buf());
    } else {
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(e) if is_lock_err(&e) => skipped.push(path.to_path_buf()),
            Err(e) => return Err(format!("remove file {}: {e}", path.display())),
        }
    }
    Ok(())
}

fn raw(e: &std::io::Error) -> i32 {
    e.raw_os_error().unwrap_or(0)
}

/// 5 = ERROR_ACCESS_DENIED, 32 = ERROR_SHARING_VIOLATION.
fn is_lock_err(e: &std::io::Error) -> bool {
    e.kind() == ErrorKind::PermissionDenied || matches!(raw(e), 5 | 32)
}

fn same_file(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => a == b,
    }
}

/// Copy `<root>/<rel>` into `dst_dir` under its file name. `required=false`
/// means a missing source is a warning, not an error (the DLL is fetched
/// separately; see README).
fn copy_asset(rel: &str, dst_dir: &Path, required: bool) -> Result<(), String> {
    let src = root().join(rel);
    if !src.exists() {
        if required {
            return Err(format!(
                "missing asset {rel} (expected at {})",
                src.display()
            ));
        }
        eprintln!("xtask: note: {rel} not present; hardware rows read -- until it is");
        return Ok(());
    }
    let name = Path::new(rel)
        .file_name()
        .ok_or_else(|| format!("bad asset path {rel:?}"))?;
    copy_file(&src, &dst_dir.join(name))
}

fn copy_file(src: &Path, dst: &Path) -> Result<(), String> {
    fs::copy(src, dst).map(|_| ()).map_err(|e| {
        let raw = e.raw_os_error().unwrap_or(0);
        // 32 = ERROR_SHARING_VIOLATION, 5 = ERROR_ACCESS_DENIED.
        if e.kind() == ErrorKind::PermissionDenied || raw == 32 || raw == 5 {
            format!(
                "cannot write {} (locked? close the running minihud and retry): {e}",
                dst.display()
            )
        } else {
            format!("copy {} -> {}: {e}", src.display(), dst.display())
        }
    })
}

/// True if `path` exists and cannot be opened for writing (a running exe).
fn is_locked(path: &Path) -> bool {
    match OpenOptions::new().write(true).open(path) {
        Ok(_) => false,
        Err(e) => {
            let raw = e.raw_os_error().unwrap_or(0);
            e.kind() == ErrorKind::PermissionDenied || raw == 32 || raw == 5
        }
    }
}

fn locked_msg(exe: &Path) -> String {
    format!(
        "{} is locked — a running minihud holds it open. Close it and retry; refusing to divert to an alternate target dir.",
        exe.display()
    )
}

fn list_dir(dir: &Path) -> Result<(), String> {
    let mut entries: Vec<PathBuf> = fs::read_dir(dir)
        .map_err(|e| format!("read {}: {e}", dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .collect();
    entries.sort();
    for p in entries {
        let size = fs::metadata(&p).map(|m| m.len()).unwrap_or(0);
        let name = p
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_default();
        println!("  {name:34} {size:>10} bytes");
    }
    Ok(())
}
