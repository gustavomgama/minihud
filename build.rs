//! Embed the single application manifest into every minihud binary.
//!
//! The manifest is the ONLY manifest and applies to all profiles. It gives
//! the process:
//!   - the comctl32 v6 side-by-side dependency (native controls + visual
//!     styles),
//!   - `asInvoker` (elevation stays runtime-only via `ensure_elevated`),
//!   - per-monitor-v2 DPI awareness.
//!
//! No `requireAdministrator`: launching unelevated must not raise a UAC
//! prompt, and the exe self-elevates at runtime instead.

use std::path::Path;

fn main() {
    // Only MSVC/Windows targets understand these linker flags.
    let target = std::env::var("TARGET").unwrap_or_default();
    if !target.contains("windows") {
        return;
    }

    let manifest = Path::new(env!("CARGO_MANIFEST_DIR")).join("minihud.manifest");
    println!("cargo:rerun-if-changed=minihud.manifest");
    println!("cargo:rerun-if-changed=build.rs");

    // /MANIFEST:EMBED           embed a manifest resource
    // /MANIFESTUAC:NO           do not let the linker invent its own UAC block
    // /MANIFESTINPUT:<path>     merge our manifest into the embedded one
    println!("cargo:rustc-link-arg-bins=/MANIFEST:EMBED");
    println!("cargo:rustc-link-arg-bins=/MANIFESTUAC:NO");
    println!(
        "cargo:rustc-link-arg-bins=/MANIFESTINPUT:{}",
        manifest.display()
    );
}
