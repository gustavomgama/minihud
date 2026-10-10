# minihud

A **headless Windows backend** with two independent halves:

1. **Hardware stats** — polls CPU / GPU / RAM sensors sourced entirely from
   [LibreHardwareMonitor](https://github.com/LibreHardwareMonitor/LibreHardwareMonitor) (LHM)
   and prints them on an interval.
2. **Present-hook capture** — an **in-process present-hook backend** that injects
   a recorder into another process and captures one `QueryPerformanceCounter`
   timestamp per frame (DXGI / D3D9 / OpenGL / Vulkan), the capture foundation
   for a future on-screen display.

There is **no UI, no overlay, and no rendering**: the recorder records frames and
the host prints numbers. The OSD (M2) is deliberately not built — see
[`HOOKING.md`](HOOKING.md).

```
CPU 12% 55C 45W 4200MHz | RAM 8192/16384MB | GPU 30% 60C 120W 1800x7000MHz | VRAM 4096/8192MB [AMD Ryzen 7 5800X / NVIDIA GeForce RTX 3070]
```

---

## Part 1 — Hardware stats

minihud polls CPU / GPU / RAM sensors and prints one line per poll. Anything LHM
does not report prints as `--`, never a guess.

| Group | Fields (all optional except CPU load) |
|-------|----------------------------------------|
| CPU | load %, temperature, package power, average clock, name |
| RAM | used / total (MB) |
| GPU | load %, temperature, power, core / memory clock, core voltage, VRAM used / total, D3D-dedicated memory, name |

LHM is the only data source: CPU temperature and package power have no usable
user-mode alternative, so no fallback backends exist.

**How it works.** LHM is .NET-only, so a persistent PowerShell sidecar
(`tools/lhm/lhm-bridge.ps1`) hosts `LibreHardwareMonitorLib.dll` and emits one
JSON sensor dump per poll. Rust reads it on a dedicated thread — the main loop
never blocks on it and keeps the last good sample until it goes stale.

```text
powershell  lhm-bridge.ps1  ->  LhmFeed (thread)  ->  HwPoller  ->  stdout
   |              |
   |              +-- LibreHardwareMonitorLib.dll
   +-- stdin "\n" ticks, stdout JSON
```

The poll cadence is adaptive: fast while values are moving, backing off when they
settle (see `ACTIVE_MS` / `IDLE_MS` in `src/main.rs`).

---

## Part 2 — Present-hook capture (backend)

An in-process present hook that records **frames only** — no Direct2D/DirectWrite,
no in-swapchain quad, no overlay window. Full design, hook set, wire format, and
safety rationale live in [`HOOKING.md`](HOOKING.md); this is the summary.

### What it hooks

| API | Entry points | Mechanism |
|---|---|---|
| **DXGI** (D3D11/D3D12) | `Present` (vtbl 8), `Present1` (22), `ResizeBuffers` (13), `SetFullscreenState` (10), `CreateSwapChain` (10), `CreateSwapChainForHwnd` (15), `CreateSwapChainForCoreWindow` (16), `CreateSwapChainForComposition` (24) | COM vtable-slot patch |
| **D3D12** | `ID3D12CommandQueue::ExecuteCommandLists` (vtbl 10) | COM vtable-slot patch (queue taken from the factory detour) |
| **D3D9** | `Present` (17), `EndScene` (42), `PresentEx` (121), `ResetEx` (132), `CreateDevice` (16), `CreateDeviceEx` (20) | COM vtable-slot patch |
| **OpenGL** | `wglSwapBuffers`, `wglSwapLayerBuffers`, `gdi32!SwapBuffers`, `libEGL!eglSwapBuffers` | IAT patch |
| **Vulkan** | `vkQueuePresentKHR` (+ the loader proc-addr chain) | IAT patch + **implicit layer** |

Vtable indices are copied from the research project's VERIFIED facts and pinned
by tests in `crates/hook-rt/src/api.rs`. IAT hooks patch the **application's own
import table** (not the graphics DLL's), including delay-load imports. Vulkan is
covered three ways: the static `vulkan-1.dll!vkQueuePresentKHR` import, the
loader's `vkGetInstanceProcAddr`/`vkGetDeviceProcAddr` chain (so however the app
resolves present, it lands on the recorder), and — for fully-dynamic apps that
never import the loader — the `hook-vk-layer` **implicit layer**
(`VK_LAYER_MINIHUD_capture`).

Mechanism: own IAT + COM vtable-slot patching. No third-party hook library. Every
hook is **fail-open** and the injected code is **panic-free across the FFI
boundary**.

### Injection pipeline

1. `OpenProcess`, refuse WOW64 targets (64-bit only), refuse known anti-cheat.
2. `VirtualAllocEx` + `WriteProcessMemory` the recorder path; remote `LoadLibraryW`.
3. Resolve the `mh_install` export and remote-call it; the thread exit code is the
   installed-API bitmask.

`--launch <exe>` starts the target **suspended**, injects before it creates its
device, then resumes — closing the "device created before injection" gap.

### Wire format

One named shared mapping, `minihud-frames-<pid>`: a header, a status block
(installed mask, errors, rescan counters), and a **4096-slot seqlock ring** of
40-byte records (`seq, qpc_start, qpc_stop, swapchain, flags, api`). The writer
sets a slot's `seq` odd → payload → even; the reader accepts only a stable slot
and never returns a torn record. The host computes trailing fps/frametime from
the QPC timestamps. See `crates/hook-ipc`.

### Safety (tier 3, opt-in)

An in-process present hook is **tier 3** — detectable. Therefore: **opt-in only**
(nothing injects unless `--capture-hook`/`--launch`/`--follow` is passed), a
**deny-list** of known anti-cheat modules is refused, and **no Stealth/evasion**
techniques are implemented — hooks go at the ordinary vtable/IAT sites. Never
inject a protected title. Details in [`HOOKING.md`](HOOKING.md) §4.

---

## Requirements

- Windows 10/11, x86-64
- Rust (MSVC toolchain)
- **.NET Framework 4.7.2** (for the LHM DLL, already present on most installs)
- Administrator rights for full CPU temp/power (SuperIO) and for cross-process
  injection. The exe self-elevates at runtime; `MINIHUD_NO_ELEVATE=1` skips it.
- For capture: a GPU/driver. Vulkan capture needs a Vulkan loader; the implicit
  layer needs the loader to discover its manifest (see below).

## One-time DLL fetch

The LHM assembly is gitignored (~700 KB), pinned to 0.9.4:

```powershell
curl -L "https://www.nuget.org/api/v2/package/LibreHardwareMonitorLib/0.9.4" -o lhm.zip
Expand-Archive lhm.zip lhm-pkg
Copy-Item lhm-pkg\lib\net472\LibreHardwareMonitorLib.dll tools\lhm\
```

Without it the exe runs but every hardware value reads `--`.

## Build & run

```powershell
cargo xtask build            # build + stage assets next to target/debug/minihud.exe
cargo xtask run              # build + stage, then run
cargo xtask build --release
cargo xtask dist             # assemble a shippable folder in dist/
cargo xtask clean            # clean target/ and dist/
```

`cargo xtask build` is what stages everything next to the exe so it resolves
assets from its own directory: `lhm-bridge.ps1`, `LibreHardwareMonitorLib.dll`,
the recorder `hook_rt.dll`, and the Vulkan layer `hook_vk_layer.dll` +
`hook_vk_layer.json`. Plain `cargo build` compiles but does not stage.

### Hardware stats

```powershell
target\debug\minihud.exe            # poll + print one line per interval; Ctrl+C to stop
```

### Capture

```powershell
# 1) run a presenter (ships as the validation target `hook-test`), note its pid
target\debug\hook-test.exe --api d3d11 --frames 2000

# 2a) launch + inject (recommended): starts the target suspended, injects, prints fps
target\debug\minihud.exe --launch target\debug\hook-test.exe --api d3d11 --frames 600

# 2b) or inject into an already-running pid, capture 5s
target\debug\minihud.exe --capture-hook <pid> 5

# 2c) or watch a target and manage the hook lifecycle automatically
target\debug\minihud.exe --follow --match deadlock*.exe
```

Vulkan **implicit layer** (fully-dynamic apps), no injection — point the loader at
the manifest dir (non-elevated testing only; production registration is a
machine-wide registry value documented in `HOOKING.md` §4.1):

```powershell
$env:VK_ADD_IMPLICIT_LAYER_PATH = "target\debug"
target\debug\hook-test.exe --api vulkan-dynamic --frames 2000
# observe the ring the layer created, read-only, no elevation:
$env:MINIHUD_NO_ELEVATE = "1"
target\debug\minihud.exe --read-frames <pid> 4
```

## CLI reference

```text
minihud                                            poll and print hardware stats
minihud --capture-hook <pid> [secs]                inject the recorder into <pid> and print fps
minihud --read-frames <pid> [secs]                 observe an existing frame ring (no injection)
minihud --launch <exe> [args...]                   start <exe> suspended, inject, print fps
minihud --follow [--match <glob>] [--poll-ms <n>] [--follow-secs <n>]
                                                   watch a target and manage the hook lifecycle
minihud -h | --help                                print help

--match <glob>       with --follow, target an exe by case-insensitive glob (e.g. deadlock*.exe)
--poll-ms <n>        with --follow, poll interval in ms (default 1000)
--follow-secs <n>    with --follow, stop after <n> seconds (default: until Ctrl+C)
```

The validation target `hook-test.exe` drives one real detour per presenter:

```text
hook-test.exe --api <d3d11|d3d11-factory|d3d9|d3d9ex|d3d12|vulkan|vulkan-dynamic|
                     opengl|opengl-delay|opengl-layer|angle|egl|dcomp> [--frames N] [--dynamic]
```

## Project layout

```text
src/main.rs          entry point: elevation, CLI dispatch, poll loop
src/render.rs        text rendering of one HwStats sample (pure, tested)
src/hw/              LHM sidecar feed + HwStats fields + adaptive poller
src/hook/mod.rs      host injector + ring reader + --capture-hook/--read-frames/--launch
src/hook/proc.rs     process/module helpers (Toolhelp, WOW64/anti-cheat refusal)
src/hook/follow.rs   --follow orchestrator (detect -> inject -> unhook -> re-target)

crates/hook-ipc/     wire format: block layout, seqlock ring, mapping, trailing fps
crates/hook-rt/      the injected recorder (cdylib): IAT + vtable patching, detours
crates/hook-vk-layer/ the Vulkan implicit layer (cdylib): vkQueuePresentKHR capture
crates/hook-test/    the presenter / validation target (not shipped product)

tools/lhm/           lhm-bridge.ps1 + LibreHardwareMonitorLib.dll
tools/audit.ps1      full audit (local == CI)
tools/critic.ps1     code-quality subset (coverage, CRAP, duplication)
tools/coverage-live.ps1  live-injection coverage of the injected DLLs
xtask/               build/run/dist/clean helper
HOOKING.md           the present-hook backend spec (source of truth)
audit.md             development log (per-pass findings, coverage numbers)
deny.toml            cargo-deny license/source policy
build.rs, minihud.manifest   elevation / DPI manifest embedding
```

## Development

This project is **test-driven** (see `AGENTS.md`): write a failing test first,
watch it fail for the right reason, then the minimal code to pass.

Definition of done — all must pass:

```sh
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace --no-tests=pass
```

Tooling used here: `cargo-nextest` (test runner), `bacon` (watch/rebuild),
`cargo-deny` (`deny.toml`) and `cargo-audit` (dependency health),
`cargo-llvm-cov` (coverage), `cargo-crap`, `cargo-mutants`, `cargo-expand`,
`cargo-machete`, `jscpd` (duplication), and `rust-analyzer` (LSP + MCP).

### Code quality ("critic")

```powershell
powershell -ExecutionPolicy Bypass -File tools/critic.ps1   # gate
```

Runs coverage (`cargo-llvm-cov`), CRAP (`cargo-crap --fail-above 30`),
duplication (`jscpd --threshold 5`), and advisory `rustqual` / `mete` reports.
The CRAP gate scores shipped product code: it excludes the entry point, the build
script, `xtask`, and `crates/hook-test` (the GPU validation tool, not shipped
product), and allows the functions reachable only through a live target injection
(`inject`/`unhook`/`capture_hook`/`run`, the `--launch` orchestrators) or a real
module base (`install_iat_in_module`). Non-destructive (writes `lcov.info` +
`target/`).

### Full audit (local == CI)

```powershell
powershell -ExecutionPolicy Bypass -File tools/audit.ps1   # everything
```

The same checks run in CI (`.github/workflows/audit.yml`): `fmt`, `clippy`,
`nextest`, `cargo-machete`, `cargo-deny`, `cargo-audit`, coverage, the CRAP and
duplication gates, and advisory `rustqual` / `mete`. `tools/critic.ps1` is the
quality subset of the same gate.

### Live-injection coverage (on demand)

The gate is deterministic and runs no real injection, so the host's live-only
functions and the injected DLLs' hot paths stay hidden. On a machine with a GPU,
measure them for real:

```powershell
powershell -ExecutionPolicy Bypass -File tools/coverage-live.ps1
```

It builds `hook-test` + the recorder, runs the host instrumented against the
presenters, checks each scenario's expected api labels, and prints the coverage
summary and per-function hit table for the host and the injected DLLs. Not part
of CI (no GPU there). Recorded numbers live in `audit.md` (Pass 25: `hook-rt`
~89% lines, `hook-vk-layer` ~79% lines, all detour arms firing, 0 errors).

## License

MIT.
