# minihud

Minimal Windows performance overlay. FPS + frametime + light system stats.
Rust + `windows-rs` + Direct2D. No UI framework.

## Shows (RTSS-style vertical stack, "--" when silent)

- App name + big FPS (tinted: green ≥120, amber ≥60, red below;
  digits always accompany color), game frametime graph (fixed 50ms
  ceiling so steady rates read flat, 16.7ms target line), detected
  graphics API (D3D12/D3D11/Vulkan/D3D9/OpenGL from loaded runtime
  dlls; `--` when the process blocks inspection)
- Two-tier text (dim labels, bright values); displayed numbers ease
  toward raw values (~150ms settle), data itself never smoothed
- min / avg / max / 1% low frametime, game Hz + display Hz
- CPU: load %, avg clock MHz (via CallNtPowerInformation), RAM used/total
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

Cadence is adaptive, not fixed: HW polls at `update_hw_ms` (default
100) while values move and backs off toward `idle_hw_ms` (default 500)
when quiet; the APP row recomputes fast while tracking, relaxed while
listening; frames only present when the pixels would differ. The frame
log shows live `hwms=` and skipped-frame counts.

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
