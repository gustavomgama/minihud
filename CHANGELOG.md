# Changelog

## Unreleased

### Features

- **Diagnostics record (every run).** Every run writes its own pair of
  artifacts into `target\<profile>\diagnostics\run-DD_MM_YYYY-HH-MM-SS\` next to
  the exe — a verbose text log and a structured JSONL — so `cargo clean`
  reclaims them. One timestamped folder per run, so prior runs are preserved.
  Nothing is omitted: each poll's complete sensor dump, every poll cycle's span
  and duration, the bridge's lifecycle and retries, and each rendered stats
  line with every value behind it, at trace depth in both the live stream and
  the files. `process.start` / `process.stop` bracket the run.
- **`--diagnostics`.** Developer-only live trace surface: the console stream
  moves to `trace`, every event's span is shown, and span timings print live.
  Without the flag the console is byte-for-byte what it always was (`info`, or
  `RUST_LOG`), because the record persists silently and the console formatter
  hides span scope.
- **Complete LHM sensor inventory.** The bridge now opens every LibreHardwareMonitor
  category — CPU, GPU, memory, motherboard, storage/NVMe, network, battery,
  controller and PSU — and the feed carries every sensor through the flat
  `(hw, type, name, value)` payload. Nodes that share a label (two same-model
  GPUs, a multi-socket board's duplicated SuperIO) are numbered (`GPU #1`,
  `GPU #2`) instead of overwriting each other.
- **`--dump-inventory`.** Prints every sensor the bridge reports, grouped by
  hardware node, then exits: a development-inspection surface, so the full data
  set can be reviewed without waiting for the curated status line. The shipped
  console output is unchanged.
- **HidSharp staging.** `tools/lhm/HidSharp.dll` is the USB-HID dependency the
  controller and PSU categories need; it is staged with the other bridge assets
  and documented in the one-time fetch. Without it the bridge still serves every
  other category instead of failing to open at all.
- **OSD overlay (`--gui`).** A transparent, always-on-top, click-through OSD
  showing FPS plus CPU / GPU / RAM, drawn with `egui`/`eframe`. It opens on a
  bare run; `--follow` and `--stats-only` select the headless console modes.
  `cargo xtask build` includes it; plain `cargo build` needs `--features gui`.
- **`minihud.toml`.** Fully optional config: density (`zen` / `standard` /
  `full`), corner `anchor`, `margin_px`, `opacity`, `scale`, `accent`, `poll_ms`,
  `fade_idle_secs`, `target`, `accent_color`, `metrics`. Missing file = sensible
  defaults; out-of-range numbers clamp; unknown keys error. See
  `minihud.toml.example`.
- **Thermal accent.** The accent hue drifts cool→warm with temperature; load bars
  run calm → amber → alarm.
- **Quit hotkey.** The overlay is click-through with no taskbar entry; press
  **Ctrl+Alt+H** to close it.
- **Demo mode.** `MINIHUD_DEMO=1` feeds synthetic, animated values, so the
  overlay (and its themes) can be previewed without hardware or the LHM DLL.
- **Densities + sparkline.** `zen` (FPS + one hero metric), `standard` (FPS +
  CPU / RAM / GPU), `full` (every field plus an FPS sparkline); `metrics`
  overrides `density` with an explicit row list.
- **Live FPS in the overlay.** `--gui` follows the foreground target (or
  `--match <glob>` / `target` in config, `--follow-secs <n>` to bound it) and
  shows FPS + 1% low; the readout clears when the target is lost and the panel
  names the followed process.
- **Quiet motion.** Changed numbers roll to their new values; the panel fades
  after `fade_idle_secs` and repaints slowly once settled.
- **Live console under the overlay.** GUI runs print the same hardware + fps
  status line as the headless modes.
- **Staged default config.** `cargo xtask build` writes a default `minihud.toml`
  next to the exe when none exists (never overwrites), so every setting is
  visible without hunting the repo.
- **Live config reload.** Saving `minihud.toml` while the overlay runs applies
  visual settings immediately (a broken save keeps the running config;
  `target` and `poll_ms` need a restart).
- **Inspection always on.** The overlay defaults `EGUI_INSPECTION=1` itself, so
  the diagnosis port survives an environment-dropping (UAC) relaunch;
  `EGUI_INSPECTION=0` opts out.
- **Every CLI option pinnable in `minihud.toml`.** New keys `mode`, `follow_secs`,
  `fps`, `fps_poll_ms`, `fps_pid`, and `stats` join the existing `target`, so a
  bare run can pin the run mode, time bound, fps switch, cadence, target, and
  reporter without flags. An explicit CLI flag always wins per-field. This also
  wires up the long-parsed-but-never-read `poll_ms`: the hardware poller
  finally honors it (a CLI `--hardware-poll` still wins).

### Fixes

- The poll loop no longer busy-spins. `wait_for_due` reads the run flag as
  "running", so the wait used to return at once and the loop spun at full
  tilt while the cadence was already enforced downstream: a core burned on
  every run. The flag's contract is now covered by
  `the_wait_treats_the_run_flag_as_running`.
- Settled (fully faded) overlay no longer repaints at 20 fps.
- The followed-process name on the overlay is the file name, never a full
  install path (a long path stretched the panel and painted over the hero).
- The idle fade now bottoms out at half opacity instead of near-invisible, so
  a static panel stays readable over a bright background.
- The overlay no longer composites empty on Windows: mouse passthrough used
  to bring `WS_EX_LAYERED` (via winit), which hides the glow swapchain from
  DWM. The overlay now skips passthrough at creation and sets
  `WS_EX_TRANSPARENT` alone (re-asserted while running, since winit rewrites
  the style on flag changes), so clicks still pass through to the game.

## 1.0.0

First stable release: tier-0 ETW FPS and LibreHardwareMonitor hardware stats,
with Release It! production hardening.

### Features

- **Tier-0 ETW FPS** (default / `--follow`). Reads the target's fps, frametime
  and 1% low from ETW present events via a **locally-tuned `PresentMon.exe`**
  sidecar (stdout CSV) — **no DLL injection, no game hook**, anti-cheat-safe by
  construction.
- **LibreHardwareMonitor hardware stats.** CPU / GPU / RAM sensors served by a
  persistent PowerShell bridge (`lhm-bridge.ps1` + `LibreHardwareMonitorLib.dll`).
- **Two poll flags** — `--fps-poll` and `--hardware-poll`, both defaulting to
  **500 ms**.
- **Fixed 500 ms fps averaging window**, decoupled from the poll cadence, so a
  fast refresh never makes the number noisy.

### Hardening (Release It!)

- **Deterministic child cleanup.** The LHM bridge child is stopped and reaped on
  drop; killing it closes its stdout pipe and unblocks the reader, so no orphan
  PowerShell survives minihud and a hung child cannot block a reader forever.
- **Bounded, jittered retry.** Both feed supervisors back off **5 s ± up to 2 s**
  (seeded/injectable jitter) so repeated failures do not retry in lockstep.
- **Observability.** Per-feed retry counts are logged (first failure `warn`,
  then `debug`, with a periodic `info` while a dependency stays wedged) and
  surfaced in the `--stats` line.
- **Prompt shutdown.** The hardware poller is stopped and its child reaped before
  the process exits, at any `--hardware-poll` cadence.
