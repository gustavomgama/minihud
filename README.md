# minihud

Minimal Windows performance overlay. FPS + frametime + light system stats.
Rust + `windows-rs` + Direct2D. No UI framework.

## Shows (RTSS-style vertical stack, "--" when silent)

- App name + big FPS (tinted: green ≥120, amber ≥60, red below;
  digits always accompany color), game frametime graph (fixed 50ms
  ceiling so steady rates read flat, 16.7ms target line), detected
  graphics API (D3D12/D3D11/Vulkan/D3D9/OpenGL from loaded runtime
  dlls; `--` when the process blocks inspection)
- Two-tier text (dim labels, bright values); digits step exactly when
  source data steps (no animated transitions between polls)
- min / avg / max / 1% low frametime, game Hz + display Hz
- CPU: load %, avg clock MHz (via CallNtPowerInformation), temp °C,
  power W (via LibreHardwareMonitor; `--` where the board yields
  nothing), RAM used/total
- GPU: load %, temp °C, power W, core/mem clocks (via NVML on NVIDIA;
  core voltage in mV has no NVML API and is omitted, not faked),
  VRAM used/total

## Keys

- F7: show/hide overlay
- F8: click-through on/off. With it OFF, drag the overlay anywhere
  with the left mouse button; position is saved next to the exe.

## Config

`minihud.toml` next to the exe (position, text size, opacity for text,
graph on/off, hw poll interval, click-through default).

Cadence is adaptive around a 700ms global default, not fixed: HW polls
at `update_hw_ms` (default 700) while values move and backs off toward
`idle_hw_ms` (default 2000) when quiet; the APP row recomputes at
50ms minimum while tracking a game, 1000ms while listening (ETW delivers in
~1s batches regardless); frames only present when the pixels would
differ. The frame log shows live `hwms=` and skipped-frame counts. Exceptions, all deliberate: the 8/30ms main-loop
heartbeat (hotkey latency, costs nothing when skipping), the 1s ETW
flush floor (platform minimum), ETW retry backoff and the 2s device-
recovery timer (recovery paths, not data rates).

## Hardware backend: LibreHardwareMonitor (primary)

LHM is .NET-only, so a persistent PowerShell sidecar
(`tools/lhm/lhm-bridge.ps1`) hosts `LibreHardwareMonitorLib.dll` and
emits one JSON sensor dump per poll. Rust reads it on a dedicated
thread — the main loop never blocks on it. PDH/NVML/DXGI remain as
automatic fallbacks wherever LHM has no data.

One-time DLL fetch (gitignored, ~700KB, pinned 0.9.4):

```powershell
curl -L "https://www.nuget.org/api/v2/package/LibreHardwareMonitorLib/0.9.4" -o lhm.zip
Expand-Archive lhm.zip lhm-pkg
Copy-Item lhm-pkg\lib\net472\LibreHardwareMonitorLib.dll tools\lhm\
```

For runs outside cargo, copy `tools\lhm\lhm-bridge.ps1` and the DLL
next to `minihud.exe`. Without them the HUD degrades to the legacy
backends (CPU temp/power read `--`).

## Notes

- Per-app FPS comes from ETW DXGI present events (flip + multiplane
  overlay); needs elevation, otherwise the APP row says so.
- Anti-cheat / protected games deny process inspection: their API row
  reads `--` (module list unreadable). Detection is proven on
  inspectable processes.
- GPU % prefers NVML utilization, falls back to the `GPU Engine(*)`
  PDH counter; VRAM prefers NVML, falls back to DXGI (discrete adapter
  with the most dedicated memory, Basic Render Driver skipped).
- GPU temperature needs vendor APIs (NVAPI/ADL) and is not wired yet.
- Window is opaque for now; true per-pixel alpha is a future milestone.
