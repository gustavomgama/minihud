# Changelog

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
