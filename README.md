# minihud

Minimal Windows performance overlay. FPS + frametime + light system stats.
Rust + `windows-rs` + Direct2D. No UI framework.

## Shows (RTSS-style vertical stack, "--" when silent)

- App name + big FPS, game frametime graph (fixed 50ms ceiling, flat at
  steady rates), detected graphics API (D3D12/D3D11/Vulkan/D3D9/OpenGL
  from loaded runtime dlls; `--` when the process blocks inspection)
- min / avg / max / 1% low frametime, game Hz + display Hz
- CPU: load %, RAM used/total (50ms poll)
- GPU: load %, VRAM used/total

## Keys

- F7: show/hide overlay
- F8: click-through on/off. With it OFF, drag the overlay anywhere
  with the left mouse button; position is saved next to the exe.

## Config

`minihud.toml` next to the exe (position, text size, opacity for text,
graph on/off, hw poll interval, click-through default).

## Notes

- Per-app FPS comes from ETW DXGI present events (flip + multiplane
  overlay); needs elevation, otherwise the APP row says so.
- GPU % uses the `GPU Engine(*)` PDH counter; shows `--` when unavailable.
- GPU temperature needs vendor APIs (NVAPI/ADL) and is not wired yet.
- Window is opaque for now; true per-pixel alpha is a future milestone.
