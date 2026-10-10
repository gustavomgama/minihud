//! Build / stage / dist for the xtask helper.
//!
//! Builds the `minihud` exe and its injected `hook_rt.dll` runtime into the
//! canonical `target/<profile>/`, stages the LibreHardwareMonitor bridge assets
//! and that runtime next to the exe, and assembles the shipping folder in
//! `dist/`.

use std::fs::{self, OpenOptions};
use std::path::{Path, PathBuf};
use std::process::Command;

use crate::{
    cargo, is_lock_err, read_dir, root, same_file, target_dir, ASSETS, EXE, HOOK_RT_DLL,
    HOOK_RT_PKG, HOOK_VK_LAYER_DLL, HOOK_VK_LAYER_PKG, PKG,
};

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
    stage_hook_rt(&profile_dir(true), &dist)?;
    stage_hook_vk_layer(&profile_dir(true), &dist)?;
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

/// The packages `build` compiles in one invocation: the product exe and its
/// injected runtime plus the Vulkan implicit layer. The validation-only
/// `hook-test` tool is deliberately not here; build it on demand with
/// `cargo build -p hook-test`.
fn build_packages() -> &'static [&'static str] {
    &[PKG, HOOK_RT_PKG, HOOK_VK_LAYER_PKG]
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
    stage_hook_rt(&dir, &dir)?;
    stage_hook_vk_layer(&dir, &dir)?;
    Ok(())
}

/// Copy the injected runtime from `from_dir` next to the product in `to_dir`.
/// `build` already compiles it into the profile dir, so this is a no-op there
/// and only does real work for `dist`. Missing is a warning, not an error (the
/// default LHM path does not need it).
fn stage_hook_rt(from_dir: &Path, to_dir: &Path) -> Result<(), String> {
    let src = from_dir.join(HOOK_RT_DLL);
    let dst = to_dir.join(HOOK_RT_DLL);
    if !src.exists() {
        eprintln!(
            "xtask: note: {HOOK_RT_DLL} not present; --capture-hook unavailable until it is built"
        );
        return Ok(());
    }
    if same_file(&src, &dst) {
        return Ok(());
    }
    copy_file(&src, &dst)
}

/// Copy the Vulkan implicit layer DLL from `from_dir` next to the product in
/// `to_dir`. `build` compiles it into the profile dir, so this is a no-op there
/// and only does real work for `dist`. Missing is a warning (the layer needs
/// registration to be active; see HOOKING.md).
fn stage_hook_vk_layer(from_dir: &Path, to_dir: &Path) -> Result<(), String> {
    let src = from_dir.join(HOOK_VK_LAYER_DLL);
    let dst = to_dir.join(HOOK_VK_LAYER_DLL);
    if !src.exists() {
        eprintln!(
            "xtask: note: {HOOK_VK_LAYER_DLL} not present; the Vulkan layer is unavailable until it is built"
        );
        return Ok(());
    }
    if same_file(&src, &dst) {
        return Ok(());
    }
    copy_file(&src, &dst)
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
    fn build_packages_are_the_product_and_runtime_but_not_hook_test() {
        let pkgs = build_packages();
        assert!(pkgs.contains(&PKG), "product exe package missing: {pkgs:?}");
        assert!(
            pkgs.contains(&HOOK_RT_PKG),
            "injected runtime package missing: {pkgs:?}"
        );
        assert!(
            pkgs.contains(&HOOK_VK_LAYER_PKG),
            "Vulkan implicit layer package missing: {pkgs:?}"
        );
        assert!(
            !pkgs.contains(&"hook-test"),
            "hook-test is validation-only and must not be built by `build`: {pkgs:?}"
        );
    }

    #[test]
    fn stage_hook_vk_layer_copies_the_layer_next_to_the_product() {
        let from = temp_dir("vk-from");
        let to = temp_dir("vk-to");
        fs::write(from.join(HOOK_VK_LAYER_DLL), b"layer-bytes").unwrap();
        stage_hook_vk_layer(&from, &to).unwrap();
        assert_eq!(
            fs::read(to.join(HOOK_VK_LAYER_DLL)).unwrap(),
            b"layer-bytes"
        );
        let _ = fs::remove_dir_all(&from);
        let _ = fs::remove_dir_all(&to);
    }

    #[test]
    fn stage_hook_rt_copies_the_runtime_next_to_the_product() {
        let from = temp_dir("from");
        let to = temp_dir("to");
        fs::write(from.join(HOOK_RT_DLL), b"dll-bytes").unwrap();
        stage_hook_rt(&from, &to).unwrap();
        assert_eq!(fs::read(to.join(HOOK_RT_DLL)).unwrap(), b"dll-bytes");
        let _ = fs::remove_dir_all(&from);
        let _ = fs::remove_dir_all(&to);
    }

    #[test]
    fn stage_hook_rt_warns_but_succeeds_when_runtime_is_missing() {
        let from = temp_dir("from");
        let to = temp_dir("to");
        stage_hook_rt(&from, &to).unwrap();
        assert!(!to.join(HOOK_RT_DLL).exists(), "must not fabricate the dll");
        let _ = fs::remove_dir_all(&from);
        let _ = fs::remove_dir_all(&to);
    }

    #[test]
    fn stage_hook_rt_leaves_the_runtime_intact_when_source_is_the_destination() {
        let dir = temp_dir("self");
        fs::write(dir.join(HOOK_RT_DLL), b"dll-bytes").unwrap();
        stage_hook_rt(&dir, &dir).unwrap();
        assert_eq!(
            fs::read(dir.join(HOOK_RT_DLL)).unwrap(),
            b"dll-bytes",
            "a self-copy must not truncate the runtime"
        );
        let _ = fs::remove_dir_all(&dir);
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
    fn copy_asset_stages_the_layer_manifest_under_its_file_name() {
        // Catches: staging the manifest at a path that does not match the layer
        // DLL's relative `library_path`, so the layer loads its DLL from the
        // wrong place (or not at all). The manifest is a real repo file here.
        let to = temp_dir("manifest");
        copy_asset(crate::HOOK_VK_LAYER_MANIFEST, &to, false).expect("the manifest is present");
        let staged = to.join("hook_vk_layer.json");
        assert!(staged.exists(), "manifest must land as hook_vk_layer.json");
        assert_eq!(
            fs::read(&staged).unwrap(),
            fs::read(root().join(crate::HOOK_VK_LAYER_MANIFEST)).unwrap(),
            "the staged manifest must be byte-identical"
        );
        let _ = fs::remove_dir_all(&to);
    }

    #[test]
    fn stage_hook_rt_does_not_stage_a_stale_hook_test_beside_the_source() {
        // Catches: a copy-the-whole-directory staging step that would ship the
        // validation-only `hook-test.exe` (and its PDB) in `dist/`. Only the
        // named runtime must be staged.
        let from = temp_dir("stale-from");
        let to = temp_dir("stale-to");
        fs::write(from.join(HOOK_RT_DLL), b"dll-bytes").unwrap();
        fs::write(from.join("hook-test.exe"), b"validation-only").unwrap();
        stage_hook_rt(&from, &to).unwrap();
        assert!(to.join(HOOK_RT_DLL).exists(), "the runtime must be staged");
        assert!(
            !to.join("hook-test.exe").exists(),
            "a stale hook-test.exe must never be staged"
        );
        let _ = fs::remove_dir_all(&from);
        let _ = fs::remove_dir_all(&to);
    }

    #[test]
    fn the_staging_list_contains_the_layer_manifest_and_never_the_validation_target() {
        // Catches: adding the validation-only `hook-test` to the staged assets,
        // or dropping the layer manifest so the shipped layer cannot load.
        assert!(
            ASSETS
                .iter()
                .any(|(rel, _)| *rel == crate::HOOK_VK_LAYER_MANIFEST),
            "the layer manifest must be staged next to the layer DLL: {ASSETS:?}"
        );
        for (rel, _) in ASSETS {
            assert!(
                !rel.contains("hook-test"),
                "validation target leaked into the staged assets: {rel}"
            );
        }
    }
}
