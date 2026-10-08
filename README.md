# minihud

Minimal Windows hardware overlay. System stats only, no per-app tracking.
Rust + `windows-rs` + Direct2D. One data source: LibreHardwareMonitor.

## Shows ("--" wherever no data exists)

- CPU: load %, temp °C, power W, RAM used/total
- GPU: load %, temp °C, power W, core/mem clocks, VRAM used/total
- Two-tier text (dim labels, bright values); digits step exactly when
  source data steps

## Keys

- F7: show/hide overlay
- F8: click-through on/off. With it OFF, drag the overlay anywhere
  with the left mouse button; position is saved next to the exe.
- Shift+F7: clean quit

## Config

`minihud.toml` next to the exe (position, text size, opacity for text,
hw poll interval, click-through default). Any listed key overrides its
default; unknown keys are ignored.

Cadence is adaptive with a 50ms floor, not fixed: HW polls at
`update_hw_ms` (default 50) while values move and backs off toward
`idle_hw_ms` (default 2000) when quiet; frames only present when the
pixels would differ. The frame log shows live `hwms=` and
skipped-frame counts. Deliberate exceptions: the 8/30ms main-loop
heartbeat (hotkey latency, costs nothing when skipping) and the 2s
device-recovery timer (recovery path, not a data rate).

## Hardware backend: LibreHardwareMonitor (only)

LHM is .NET-only, so a persistent PowerShell sidecar
(`tools/lhm/lhm-bridge.ps1`) hosts `LibreHardwareMonitorLib.dll` and
emits one JSON sensor dump per poll. Rust reads it on a dedicated
thread — the main loop never blocks on it. There are no fallback
backends: anything LHM lacks shows `--`, never a guess.

One-time DLL fetch (gitignored, ~700KB, pinned 0.9.4):

```powershell
curl -L "https://www.nuget.org/api/v2/package/LibreHardwareMonitorLib/0.9.4" -o lhm.zip
Expand-Archive lhm.zip lhm-pkg
Copy-Item lhm-pkg\lib\net472\LibreHardwareMonitorLib.dll tools\lhm\
```

For runs outside cargo, copy `tools\lhm\lhm-bridge.ps1` and the DLL
next to `minihud.exe`. Without them every row reads `--`.

## Notes

- No per-app frame/FPS/frametime tracking: the only external sources
  on Windows are ETW present events and API hooking (injection,
  anti-cheat consequences). Neither ships here by decision.
- Core voltage (mV) has no readable source (no NVML API, LHM SuperIO
  absent on most boards) and is omitted, not faked.
- Window is opaque for now; true per-pixel alpha is a future milestone.
