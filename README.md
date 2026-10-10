# minihud

A **headless hardware-stats backend for Windows**, sourced entirely from
[LibreHardwareMonitor](https://github.com/LibreHardwareMonitor/LibreHardwareMonitor) (LHM).

minihud polls CPU / GPU / RAM sensors and prints them on an interval. There is
no UI, no overlay, no framerate capture, no injection, and no application
detection — just the LHM hardware integration. Anything LHM does not report
prints as `--`, never a guess.

```
CPU 12% 55C 45W 4200MHz | RAM 8192/16384MB | GPU 30% 60C 120W 1800x7000MHz | VRAM 4096/8192MB [AMD Ryzen 7 5800X / NVIDIA GeForce RTX 3070]
```

## What it collects

| Group | Fields (all optional except CPU load) |
|-------|----------------------------------------|
| CPU | load %, temperature, package power, average clock, name |
| RAM | used / total (MB) |
| GPU | load %, temperature, power, core / memory clock, core voltage, VRAM used / total, D3D-dedicated memory, name |

LHM is the only data source: CPU temperature and package power have no usable
user-mode alternative, so no fallback backends exist.

## How it works

LHM is .NET-only, so a persistent PowerShell sidecar
(`tools/lhm/lhm-bridge.ps1`) hosts `LibreHardwareMonitorLib.dll` and emits one
JSON sensor dump per poll. Rust reads it on a dedicated thread — the main loop
never blocks on it and keeps the last good sample until it goes stale.

```text
powershell  lhm-bridge.ps1  ->  LhmFeed (thread)  ->  HwPoller  ->  stdout
   |              |
   |              +-- LibreHardwareMonitorLib.dll
   +-- stdin "\n" ticks, stdout JSON
```

The poll cadence is adaptive: fast while values are moving, backing off when
they settle (see `ACTIVE_MS` / `IDLE_MS` in `src/main.rs`).

## Requirements

- Windows 10/11
- Rust (MSVC toolchain)
- **.NET Framework 4.7.2** (for the LHM DLL, already present on most Windows installs)
- Administrator rights for full CPU temperature/power (SuperIO). The exe
  self-elevates at runtime; `MINIHUD_NO_ELEVATE=1` skips it.

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

`cargo build` / `cargo build --release` also work, but `cargo xtask build` is
what stages `lhm-bridge.ps1` and `LibreHardwareMonitorLib.dll` next to the exe
so it can find them — assets are always resolved from the exe's own directory.

## Output

`minihud.exe` prints one text line per hardware poll. Stop it with Ctrl+C.
There is no config file; cadence is compiled in.

## Project layout

```
src/main.rs          entry point: elevation + poll loop
src/render.rs        text rendering of one HwStats sample (pure, tested)
src/hw/lhm.rs        LHM sidecar feed (spawns the bridge, parses JSON, applies)
src/hw/sensors.rs    HwStats fields + adaptive HwPoller + noise thresholds
tools/lhm/           lhm-bridge.ps1 + LibreHardwareMonitorLib.dll
xtask/               build/run/dist/clean helper
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
cargo nextest run --workspace
```

Tooling used here: `cargo-nextest` (test runner), `bacon` (watch/rebuild),
`cargo-deny` (`deny.toml`) and `cargo-audit` (dependency health),
`cargo-llvm-cov` (coverage), `cargo-mutants`, `cargo-expand`, `cargo-machete`,
and `rust-analyzer` (LSP + MCP). `src/render.rs` is fully covered; the LHM
sidecar I/O and the entry point are exercised by running the binary.

### Code quality ("critic")

```powershell
powershell -ExecutionPolicy Bypass -File tools/critic.ps1   # gate
```

Runs coverage (`cargo-llvm-cov`), CRAP (`cargo-crap --fail-above 30`),
duplication (`jscpd --threshold 5`), and advisory `rustqual` / `mete` reports.
Non-destructive (writes `lcov.info` + `target/`).

### Full audit (local == CI)

```powershell
powershell -ExecutionPolicy Bypass -File tools/audit.ps1   # everything
```

The same checks run in CI (`.github/workflows/audit.yml`): `fmt`, `clippy`,
`nextest`, `cargo-machete`, `cargo-deny`, `cargo-audit`, coverage, the CRAP and
duplication gates, and advisory `rustqual` / `mete`. `tools/critic.ps1` is the
quality subset of the same gate.

## License

MIT.
