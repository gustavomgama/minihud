# minihud

Minimal Windows performance overlay. FPS + frametime + light system stats.
Rust + `windows-rs` + Direct2D. No UI framework.

## Shows (RTSS-style vertical stack, "--" when silent)

- App name + big FPS, game frametime graph
- API (DXGI for now), min / avg / max / 1% low
- CPU: load %, RAM used/total
- GPU: load %, VRAM used/total
- Game Hz + display Hz

## Keys

- F7: show/hide overlay
- F8: click-through on/off. With it OFF, drag the overlay anywhere
  with the left mouse button; position is saved next to the exe.

## Config

`minihud.toml` next to the exe (position, text size, opacity for text,
graph on/off, hw poll interval, click-through default).

## Notes

- FPS is the overlay's refresh, not per-game presents. Per-game ETW
  present tracking is a future milestone.
- GPU % uses the `GPU Engine(*)` PDH counter; shows `--` when unavailable.
- GPU temperature needs vendor APIs (NVAPI/ADL) and is not wired yet.
- Window is opaque for now; true per-pixel alpha is a future milestone.
