//! Build / stage / dist for the xtask helper.
//!
//! Builds the `minihud` exe into the canonical `target/<profile>/`, stages the
//! LibreHardwareMonitor bridge assets and the tier-0 `PresentMon.exe` sidecar
//! next to the exe, and assembles the shipping folder in `dist/`.

use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{cargo, is_lock_err, read_dir, root, target_dir, ASSETS, EXE, PKG};

/// `cargo xtask build`: build, stage, and print the exe path.
pub fn build_and_stage(release: bool) -> Result<(), String> {
    build(release)?;
    stage(release)?;
    println!("built {}", exe_path(release)?.display());
    Ok(())
}

/// `cargo xtask run`: build, stage, then launch the exe with the passthrough args.
pub fn run_exe(release: bool, passthrough: &[String]) -> Result<(), String> {
    build(release)?;
    stage(release)?;
    let exe = exe_path(release)?;
    let status = Command::new(&exe)
        .args(passthrough)
        .status()
        .map_err(|e| format!("run {}: {e}", exe.display()))?;
    if !status.success() {
        return Err(format!("{} exited {status}", exe.display()));
    }
    Ok(())
}

/// `cargo xtask dist`: build release, then assemble `dist/`.
pub fn dist() -> Result<(), String> {
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

fn profile(release: bool) -> &'static str {
    if release {
        "release"
    } else {
        "debug"
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

/// The packages `build` compiles in one invocation: just the product exe.
fn build_packages() -> &'static [&'static str] {
    &[PKG]
}

fn build(release: bool) -> Result<(), String> {
    let exe = profile_dir(release).join(EXE);
    if is_locked(&exe) {
        return Err(locked_msg(&exe));
    }
    let mut c = cargo();
    c.current_dir(root()).arg("build");
    for pkg in build_packages() {
        c.arg("-p").arg(pkg);
    }
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
        if is_lock_err(&e) {
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
        Err(e) => is_lock_err(&e),
    }
}

fn locked_msg(exe: &Path) -> String {
    format!(
        "{} is locked — a running minihud holds it open. Close it and retry; refusing to divert to an alternate target dir.",
        exe.display()
    )
}

fn list_dir(dir: &Path) -> Result<(), String> {
    let mut entries: Vec<PathBuf> = read_dir(dir)?
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    /// A fresh unique directory under the system temp dir (std-only, no deps).
    fn temp_dir(tag: &str) -> PathBuf {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let d = std::env::temp_dir().join(format!("minihud-xtask-{tag}-{n}"));
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn build_packages_include_the_product() {
        // Catches: `build` no longer compiling the product exe, so
        // `cargo xtask build` would not produce the artifact it stages.
        let pkgs = build_packages();
        assert!(pkgs.contains(&PKG), "product exe package missing: {pkgs:?}");
    }

    #[test]
    fn copy_asset_errors_loudly_when_a_required_asset_is_missing() {
        // Catches: a required asset (lhm-bridge.ps1) being silently skipped, so
        // the shipped exe resolves no bridge and the hardware rows stay `--`.
        let to = temp_dir("required-missing");
        let err = copy_asset("tools/lhm/definitely-not-here.ps1", &to, true)
            .expect_err("a missing required asset must be an error, not a skip");
        assert!(
            err.contains("missing asset") && err.contains("definitely-not-here.ps1"),
            "the error must name the missing asset: {err}"
        );
        let _ = fs::remove_dir_all(&to);
    }

    #[test]
    fn copy_asset_warns_but_succeeds_when_an_optional_asset_is_missing() {
        // Catches: the optional LibreHardwareMonitorLib.dll (fetched separately)
        // turning a normal build into a hard failure, or fabricating a file.
        let to = temp_dir("optional-missing");
        copy_asset("tools/lhm/definitely-not-here.dll", &to, false)
            .expect("an optional missing asset is a warning, not an error");
        assert!(
            read_dir(&to).unwrap().next().is_none(),
            "a missing optional asset must not fabricate a file"
        );
        let _ = fs::remove_dir_all(&to);
    }

    #[test]
    fn copy_asset_stages_presentmon_under_its_file_name() {
        // Catches: staging the tier-0 FPS collector at a path that does not
        // match the name `src/fps/presentmon.rs` resolves, so a shipped build
        // spawns no PresentMon and the fps row stays `--`. The binary is a real
        // repo file (see README); this asserts it lands under `PresentMon.exe`.
        let to = temp_dir("presentmon");
        copy_asset(crate::PRESENTMON_REL, &to, false).expect("PresentMon.exe is present");
        let staged = to.join("PresentMon.exe");
        assert!(staged.exists(), "PresentMon must land as PresentMon.exe");
        assert_eq!(
            fs::read(&staged).unwrap(),
            fs::read(root().join(crate::PRESENTMON_REL)).unwrap(),
            "the staged PresentMon must be byte-identical"
        );
        let _ = fs::remove_dir_all(&to);
    }

    #[test]
    fn the_staging_list_contains_presentmon_as_optional() {
        // Catches: PresentMon becoming a required asset (a missing binary would
        // hard-fail every build) or dropping out of the staged assets entirely
        // (the fps path would silently be dead in a shipped folder).
        let entry = ASSETS.iter().find(|(rel, _)| *rel == crate::PRESENTMON_REL);
        assert_eq!(
            entry,
            Some(&(crate::PRESENTMON_REL, false)),
            "PresentMon must be staged as an optional asset: {ASSETS:?}"
        );
    }
}
