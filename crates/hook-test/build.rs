//! Build script for the `hook-test` validation target.
//!
//! RTSS (RivaTuner Statistics Server) injects a present hook into most
//! processes, and that hook faults when a *custom present-wrapping Vulkan
//! layer* sits above it. RTSS has a sanctioned opt-out: a process that exports
//! a `RTSSHooksCompatibility` symbol is skipped. `hook-test` is a test target,
//! so we opt it out to isolate the layer test from RTSS. This changes nothing
//! about the hooking DLL under test; it only stops a third-party overlay from
//! interfering with the target process.
//!
//! `d3d9.dll` is **delay-loaded** on purpose: a delay-loaded import lives in
//! data directory 13, not the standard import table, so it validates the
//! recorder's delay-import walk (`hook-rt/src/pe.rs`). Without that walk the
//! `d3d9` presenter would never be captured (the `Direct3DCreate9` IAT slot is
//! never patched). `delayimp.lib` provides `__delayLoadHelper2`, which the
//! linker references when a `/DELAYLOAD:` import is emitted.
//!
//! `opengl32.dll` is delay-loaded too, for the `opengl-delay` presenter: it
//! calls the delay-loaded `wglSwapBuffers` **swap** export every frame, so the
//! recorder must *re-patch* the delay IAT slot after `__delayLoadHelper2`
//! overwrites it — the self-heal arm of the delay path.
//!
//! `libEGL.dll` (ANGLE) is delay-loaded so the `angle` presenter can be added
//! without making the whole target depend on ANGLE being installed. Steam's
//! bundled CEF ships a usable `libEGL.dll` + `libGLESv2.dll`.
fn main() {
    println!("cargo:rustc-link-arg-bin=hook-test=/EXPORT:RTSSHooksCompatibility");
    println!("cargo:rustc-link-arg-bin=hook-test=/DELAYLOAD:d3d9.dll");
    println!("cargo:rustc-link-arg-bin=hook-test=/DELAYLOAD:opengl32.dll");
    println!("cargo:rustc-link-arg-bin=hook-test=/DELAYLOAD:libEGL.dll");
    println!("cargo:rustc-link-arg-bin=hook-test=delayimp.lib");
    println!("cargo:rerun-if-changed=build.rs");
}
