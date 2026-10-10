//! minihud build helper: `cargo xtask <build|run|dist|clean>`.
//!
//! std-only and dependency-free. It builds the `minihud` package into the
//! canonical `target/<profile>/` and stages the LibreHardwareMonitor bridge
//! assets the exe resolves exe-relative next to it, so debug and release run
//! the same way. `dist` assembles the shipping folder; `clean` lives in
//! [`clean`]; build/stage/dist live in [`build`].

mod build;
mod clean;

use std::env;
use std::fs::{self, ReadDir};
use std::io::ErrorKind;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

pub(crate) const PKG: &str = "minihud";
pub(crate) const EXE: &str = "minihud.exe";

/// The injected runtime, a `cdylib` built alongside the exe as `hook_rt.dll`
/// (not an exe). Staged next to the product so `--capture-hook` works.
pub(crate) const HOOK_RT_PKG: &str = "hook-rt";
pub(crate) const HOOK_RT_DLL: &str = "hook_rt.dll";

/// The Vulkan implicit layer, a `cdylib` built as `hook_vk_layer.dll`, and its
/// loader manifest. Both are staged together so the manifest's relative
/// `library_path` resolves; registering the manifest is documented in HOOKING.md.
pub(crate) const HOOK_VK_LAYER_PKG: &str = "hook-vk-layer";
pub(crate) const HOOK_VK_LAYER_DLL: &str = "hook_vk_layer.dll";
pub(crate) const HOOK_VK_LAYER_MANIFEST: &str = "crates/hook-vk-layer/hook_vk_layer.json";

/// Assets copied next to the built exe and into `dist/`.
/// `(path relative to workspace root, required)`.
pub(crate) const ASSETS: &[(&str, bool)] = &[
    ("tools/lhm/lhm-bridge.ps1", true),
    ("tools/lhm/LibreHardwareMonitorLib.dll", false),
    (HOOK_VK_LAYER_MANIFEST, false),
];

/// Win32 error codes we treat as "locked" (a running exe holds the file).
const ERROR_ACCESS_DENIED: i32 = 5;
const ERROR_SHARING_VIOLATION: i32 = 32;
/// ERROR_DIR_NOT_EMPTY: a directory still holds a skipped/locked child.
pub(crate) const ERROR_DIR_NOT_EMPTY: i32 = 145;

const USAGE: &str = "usage: cargo xtask <build|run|dist|clean> [--release] [-- <args>]";

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

/// Parsed command line: the subcommand, `--release`, and anything after `--`.
struct Args {
    cmd: String,
    release: bool,
    passthrough: Vec<String>,
}

/// Parse `args` (everything after the program name).
fn parse_args(args: &[String]) -> Result<Args, String> {
    let mut it = args.iter();
    let cmd = it.next().map(String::as_str).unwrap_or("");
    if cmd.is_empty() {
        return Err(USAGE.into());
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
            return Err(format!("unknown arg {a:?}\n{USAGE}"));
        }
    }
    Ok(Args {
        cmd: cmd.to_string(),
        release,
        passthrough,
    })
}

fn run(args: &[String]) -> Result<(), String> {
    let Args {
        cmd,
        release,
        passthrough,
    } = parse_args(args)?;
    match cmd.as_str() {
        "build" => build::build_and_stage(release)?,
        "run" => build::run_exe(release, &passthrough)?,
        "dist" => build::dist()?,
        "clean" => clean::clean()?,
        other => {
            return Err(format!(
                "unknown subcommand {other:?} (build|run|dist|clean)"
            ))
        }
    }
    Ok(())
}

/// Workspace root: this crate lives at `<root>/xtask`.
pub(crate) fn root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask crate lives in <workspace>/xtask")
        .to_path_buf()
}

/// Canonical target dir: `CARGO_TARGET_DIR` if set, else `<root>/target`.
/// No alternate directories are introduced.
pub(crate) fn target_dir() -> PathBuf {
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

pub(crate) fn cargo() -> Command {
    Command::new(env::var_os("CARGO").unwrap_or_else(|| "cargo".into()))
}

/// `read_dir` with a consistent error message (the format appears in several
/// call sites; kept in one place).
pub(crate) fn read_dir(path: &Path) -> Result<ReadDir, String> {
    fs::read_dir(path).map_err(|e| format!("read {}: {e}", path.display()))
}

pub(crate) fn raw(e: &std::io::Error) -> i32 {
    e.raw_os_error().unwrap_or(0)
}

/// A locked file/dir: `ERROR_ACCESS_DENIED` or `ERROR_SHARING_VIOLATION`.
pub(crate) fn is_lock_err(e: &std::io::Error) -> bool {
    e.kind() == ErrorKind::PermissionDenied
        || matches!(raw(e), ERROR_ACCESS_DENIED | ERROR_SHARING_VIOLATION)
}

pub(crate) fn same_file(a: &Path, b: &Path) -> bool {
    match (fs::canonicalize(a), fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => a == b,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn parse_args_handles_release_and_passthrough() {
        // Catches: dropping `--release` (a debug build shipped by mistake) or
        // leaking `--name value` into the passthrough args.
        let p = parse_args(&a(&["run", "--release", "--", "--capture-hook", "42"])).unwrap();
        assert_eq!(p.cmd, "run");
        assert!(p.release);
        assert_eq!(p.passthrough, a(&["--capture-hook", "42"]));
    }

    #[test]
    fn parse_args_defaults_to_debug_with_no_passthrough() {
        // Catches: defaulting `release` to true, or inventing passthrough args.
        let p = parse_args(&a(&["build"])).unwrap();
        assert_eq!(p.cmd, "build");
        assert!(!p.release);
        assert!(p.passthrough.is_empty());
    }

    #[test]
    fn parse_args_rejects_a_missing_command_and_unknown_args() {
        // Catches: silently accepting a typo'd flag (e.g. `--relase`), which
        // would build debug and confuse the operator.
        assert!(parse_args(&a(&[])).is_err(), "no command is an error");
        assert!(
            parse_args(&a(&["build", "--nope"])).is_err(),
            "an unknown flag is an error"
        );
    }
}
