# minihud

Minimal Windows performance overlay. FPS + frametime + light system stats.
Rust + `windows-rs` + Direct2D. No UI framework.

## Shows

- Line 1: FPS, avg/min/max frametime (overlay's own refresh rate)
- Line 2: CPU %, RAM used/total, GPU % (best-effort), VRAM used/total
- Frametime graph (last 180 frames)

## Keys

- F7: show/hide overlay
- F8: toggle click-through

## Config

`minihud.toml` next to the exe (position, text size, opacity for text,
graph on/off, hw poll interval, click-through default).

## Notes

- FPS is the overlay's refresh, not per-game presents. Per-game ETW
  present tracking is a future milestone.
- GPU % uses the `GPU Engine(*)` PDH counter; shows `--` when unavailable.
- GPU temperature needs vendor APIs (NVAPI/ADL) and is not wired yet.
- Window is opaque for now; true per-pixel alpha is a future milestone.
