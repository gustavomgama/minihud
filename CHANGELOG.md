# Changelog

## Unreleased

### Features

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

### Fixes

- Settled (fully faded) overlay no longer repaints at 20 fps.

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
