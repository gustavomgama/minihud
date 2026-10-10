//! Recursive `clean` for the xtask build helper.
//!
//! On Windows `cargo clean` cannot delete the currently running `xtask.exe`,
//! so the fallback removes `target/` and `dist/` by hand, recording (not
//! failing on) the one file it cannot — the running image.

use std::fs;
use std::path::{Path, PathBuf};

use crate::{cargo, is_lock_err, raw, read_dir, root, same_file, target_dir, ERROR_DIR_NOT_EMPTY};

pub fn clean() -> Result<(), String> {
    let dist = root().join("dist");
    let me = std::env::current_exe().ok();

    // Primary path: `cargo clean`. A failure here is expected on Windows when
    // the running xtask holds a file — finish by hand.
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
        return Ok(());
    }
    let list = others
        .iter()
        .map(|p| p.display().to_string())
        .collect::<Vec<_>>()
        .join(", ");
    Err(format!("could not remove (locked): {list}"))
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
        for entry in read_dir(path)? {
            let child = entry
                .map_err(|e| format!("read {}: {e}", path.display()))?
                .path();
            remove_tree(&child, self_exe, skipped)?;
        }
        match fs::remove_dir(path) {
            Ok(()) => {}
            // Non-empty because it still holds a skipped/locked child.
            Err(e) if raw(&e) == ERROR_DIR_NOT_EMPTY || is_lock_err(&e) => {}
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{SystemTime, UNIX_EPOCH};

    fn temp_dir(tag: &str) -> PathBuf {
        let n = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let d = std::env::temp_dir().join(format!("minihud-clean-{tag}-{n}"));
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn remove_tree_removes_only_the_subtree_it_is_given() {
        // Catches: `clean` deleting user data outside `target/` — following a
        // path out of the tree, or removing a sibling of the directory it was
        // asked to remove.
        let base = temp_dir("scope");
        let tree = base.join("target");
        fs::create_dir_all(tree.join("nested")).unwrap();
        fs::write(tree.join("nested").join("a.txt"), b"x").unwrap();
        let sibling = base.join("keep-me.txt");
        fs::write(&sibling, b"user data").unwrap();

        let mut skipped = Vec::new();
        remove_tree(&tree, None, &mut skipped).unwrap();

        assert!(!tree.exists(), "the given tree must be removed");
        assert!(sibling.exists(), "data outside the tree must be untouched");
        assert!(skipped.is_empty(), "nothing was locked: {skipped:?}");
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn remove_tree_skips_and_reports_the_running_image() {
        // Catches: `clean` aborting (instead of reporting) when it cannot delete
        // the currently running xtask image, which is the normal Windows case.
        let base = temp_dir("self");
        let me = base.join("xtask.exe");
        fs::write(&me, b"running").unwrap();

        let mut skipped = Vec::new();
        remove_tree(base.as_path(), Some(&me), &mut skipped).unwrap();

        assert!(me.exists(), "the running image must be left in place");
        assert_eq!(
            skipped,
            vec![me.clone()],
            "the running image must be reported as skipped"
        );
        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn remove_tree_is_a_noop_for_a_path_that_does_not_exist() {
        // Catches: `clean` failing when `dist/` (or a partially built target)
        // is already absent — it must be idempotent.
        let missing = std::env::temp_dir().join("minihud-clean-definitely-absent");
        let mut skipped = Vec::new();
        remove_tree(&missing, None, &mut skipped).unwrap();
        assert!(skipped.is_empty());
    }
}
