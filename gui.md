# minihud GUI — progress & plan

Branch: `gui` (worktree `C:\Users\helmet\dev\minihud-gui`). Master worktree untouched.

## Vision — "Minimal Instrument"

An OSD that looks like a piece of lab equipment, not a widget. Rules:

- **Silence by default.** No chrome, no borders you can name, no buttons until asked.
- **One heroic number.** FPS gets the space; everything else is supporting cast.
- **Tabular figures.** Digits never jitter as values change. Monospace numerals.
- **Hairline geometry.** Single rounded panel, 1px inner hairline, glass-on-dark.
- **Thermal accent.** One accent hue that shifts cool→warm with temperature, so state
  reads at a glance without a legend.
- **Motion is a courtesy, never a show.** Value changes roll; the panel fades; nothing pops.

## Two audiences, one product

- **Set-and-forget.** `--gui` just works: auto-picks a density from window size, top-left,
  fades out when nothing is happening. Zero config.
- **Total control.** A `minihud.toml` (or in-overlay settings) exposes every color, metric,
  corner, opacity, poll cadence, and which rows show. Nothing hidden behind a rebuild.

## Densities

- **Zen** — FPS + one hero metric (temperature or GPU load). For people who want one glance.
- **Standard** — FPS + CPU / GPU / RAM, each as a small labeled bar.
- **Full** — every HwStats field, plus an FPS sparkline.

## Architecture

- `gui::model` — pure, no-egui: HwStats (+fps) → display rows, plus config/presets.
  Always compiled, always tested by the default suite.
- `gui::app` — egui/eframe rendering. Feature-gated behind `gui` so the default
  headless build stays lean and CI stays green.
- Entry: `minihud --gui` (works beside `--stats-only` / default follow).

## Roadmap

1. [x] Isolated `gui` branch + worktree.
2. [x] `gui::model` — row model, thresholds, `--` for missing. (TDD)
3. [x] `gui::config` — TOML-backed, defaults = set-and-forget. (TDD)
4. [ ] `gui::app` — eframe overlay: transparent, always-on-top, click-through.
5. [~] Accent theming done (`gui::theme`: thermal gradient + load thresholds);
   densities (Zen/Standard/Full row selection) still pending.
6. [ ] `--gui` wiring in `main.rs`; `cargo xtask` staging.
7. [ ] README/CHANGELOG; commit + push `gui`.

## Progress log

- 2026-10-11: worktree + branch up.
- 2026-10-11: `gui::model` landed (TDD, 8 tests): CPU/CPU temp/RAM/GPU/GPU temp/VRAM/
  GPU power rows, `--` for missing, clamped bar fractions. Gate green (148 tests).
  `gui` module is `#![allow(dead_code)]` until `--gui` wiring lands (item 6).
- 2026-10-11: `gui::config` landed (TDD, 6 tests): `minihud.toml` schema with
  set-and-forget defaults, missing fields filled from defaults, out-of-range numbers
  clamped (opacity 0.1..1, scale 0.5..3, poll 100..10000, margin ≤200), unknown
  keys/values rejected. Added `toml` dep. Gate green (154 tests).
- 2026-10-11: `gui::theme` landed (TDD, 5 tests): thermal accent gradient
  (cool blue→warm red), load-threshold tone (calm/amber/alarm), fixed accent
  hues, thermal fallback when no temperature. Gate green (159 tests).
