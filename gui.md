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

## Blocked

- **PR for `gui`** — `gh` is not authenticated on this machine
  (`gh auth status` → "You are not logged into any GitHub hosts"). Opening the PR
  needs `gh auth login` (interactive, credentials). The branch is pushed; open
  the PR at:
  https://github.com/gustavomgama/minihud/compare/master...gui

## Roadmap

1. [x] Isolated `gui` branch + worktree.
2. [x] `gui::model` — row model, thresholds, `--` for missing. (TDD)
3. [x] `gui::config` — TOML-backed, defaults = set-and-forget. (TDD)
4. [x] `gui::app` — eframe overlay: transparent, always-on-top, click-through.
5. [~] Accent theming done (`gui::theme`: thermal gradient + load thresholds);
   densities (Zen/Standard/Full row selection) done (`gui::model::rows_for`).
6. [x] `--gui` wiring in `main.rs`; `cargo xtask` staging.
7. [x] README/CHANGELOG; commit + push `gui`.
8. [x] Polish: anchor/corner positioning, content-fit window size, `scale`,
   hairline, monospace figures, live FPS feed in GUI mode.
9. [x] Motion: idle fade (`fade_idle_secs`) + change signature (`gui::motion`).
10. [x] `cargo xtask --features <list>` so the documented overlay build works.
11. [x] Value roll: FPS / 1% low / bars animate to new values (`animate_value_with_time`).
12. [x] `--gui --match <glob>` targets a named exe instead of the foreground.
13. [x] Quit hotkey: Ctrl+Alt+H closes the click-through overlay (`RegisterHotKey`).
14. [x] Config `target`: pin a `--match` glob in `minihud.toml` (CLI wins).
15. [x] Clear the fps readout when the target is lost; log target changes.
16. [x] Full-density FPS sparkline (`gui::spark` + fps history in the snapshot).
17. [x] Demo mode (`MINIHUD_DEMO=1`) + README screenshots of four variations.
18. [x] Design pass from the screenshots: compact memory, hero rework, temp tints.
19. [x] Threshold tune: load amber at 70%, red at 85%; temps green<65, amber<80.

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
- 2026-10-11: `gui::model::rows_for` (TDD, 3 tests): density → row subset.
  `gui::app` (feature `gui`): transparent, undecorated, always-on-top,
  click-through eframe overlay; hero FPS + labeled bars, `clear_color`
  transparent, repaint at ~4 Hz. eframe 0.36 pinned, `default-features=false`
  (`glow`+`default_fonts`+`inspection`+`accesskit`). Compiles and clippy-clean
  with `--features gui`. Gate green (162 tests). NOT yet launched or wired to
  live data (item 6).
- 2026-10-11: `--gui` wired (item 6). `Config::load` + `resolve` (TDD, 3 tests:
  missing→defaults, present→parsed, broken→error), `Mode::Gui` in `select_mode`
  (TDD), dispatch via `report("--gui", ..)`, background `HwPoller`→`Snapshot`,
  overlay launched on the main thread. **Smoke-tested live** through egui-mcp:
  the window renders (rounded glass panel, hero FPS `--`, CPU `0%` + bar, RAM/GPU
  `--`). LHM DLL not staged so hardware reads `--`; FPS not yet fed in GUI mode.
  Gate green (166 tests, default and `--features gui`, both clippy-clean).
- 2026-10-11: `gui::layout` (TDD, 3 tests): `anchor_pos` (four corners, clamped
  on-screen) + `content_size` (grows with density and `scale`). App now sizes
  the window to content, anchors to the configured corner once the monitor size
  is known, applies `scale`, and draws a hairline. Window auto-fits to the
  measured panel each frame, so a bottom/right anchor sits flush. **Smoke-tested
  live** with `anchor=bottom-right, density=full, scale=1.2, accent=amber`: the
  panel rendered 312x257 (fit, not 353), 7 rows, amber hero, hairline. Gate
  green (169 tests, both feature sets).
- 2026-10-11: Docs (item 7): `minihud.toml.example`, README "OSD overlay" section
  + CLI/layout entries, CHANGELOG "Unreleased". Gate green (169 tests).
- 2026-10-11: Live FPS in GUI mode (item 8). `fps::follow::select` exposed as
  `resolve_follow_target`; `main::fps_loop` follows the foreground over ETW
  (same tested debounce/target helpers) and publishes fps + 1% low into the
  snapshot. **Smoke-tested**: runs clean, hero shows `--` when the foreground is
  not a capturable target (degrades gracefully). Real fps needs a game +
  staged PresentMon, not available headless. Gate green (169 tests, both feature
  sets).
- 2026-10-11: `gui::motion` (TDD, 5 tests): `idle_opacity` (full until
  `fade_idle_secs`, then a monotonic ramp to a 0.15 floor; 0 disables) and
  `signature` (FNV over the drawn rows + fps + 1% low) for change detection.
  The app tracks the last change, fades the panel (bg alpha + `ui.set_opacity`),
  and repaints at 50 ms while animating, else 250 ms. **Smoke-tested live** with
  `fade_idle_secs = 2`: the panel dims to the floor when idle. Gate green
  (174 tests, both feature sets).
- 2026-10-11: xtask `--features <list>` (TDD): the README's
  `cargo xtask build --features gui` was rejected before (only `--release`
  parsed); now parsed and passed to cargo for build/run/dist. Verified live:
  `cargo run -p xtask -- build --features gui` builds and stages. Gate green
  (175 tests).
- 2026-10-11: `xtask dist` also ships `minihud.toml.example` so the config
  surface is discoverable in the shipping folder. Gate green (175 tests). PR
  blocked on `gh` auth (see Blocked).
- 2026-10-11: Value roll (item 11): the hero FPS, 1% low and each load bar
  animate to their new value via `ctx.animate_value_with_time` (0.28 s), so a
  change settles instead of jumping. Smoke-tested: renders clean, no panic.
  Gate green (175 tests, both feature sets).
- 2026-10-11: CI fix. Ran the real gates: `cargo crap` scored `Overlay::ui`
  (src/gui/app.rs, feature-gated egui glue, 0% coverage) at CRAP 56 → **CI would
  have failed**. Excluded `src/gui/app.rs` from CRAP in `tools/critic.ps1` and
  `.github/workflows/audit.yml`, alongside the entry point. Re-ran: CRAP ✓ (0
  exceed), jscpd ✓ (0.73%), cargo-machete ✓.
- 2026-10-11: `--gui --match <glob>` (TDD). The overlay ignored `--match` and
  always followed the foreground; `Mode::Gui` now carries the glob, threaded to
  `fps_loop` → `resolve_follow_target`. Help + README updated. Smoke-tested:
  starts clean with `--match`. Gate green (176 tests, both feature sets).
- 2026-10-11: Quit hotkey (item 13). The overlay is click-through with no taskbar
  entry, so there was no way to close it. `spawn_quit_hotkey` registers
  Ctrl+Alt+H (`RegisterHotKey`, dedicated `GetMessageW` thread) → sets a flag →
  `app::run` sends `ViewportCommand::Close`. Added the
  `Win32_UI_Input_KeyboardAndMouse` windows feature. **Verified end-to-end**: the
  log shows the hotkey registered, and a synthesized Ctrl+Alt+H closed the
  process. Gate green (176 tests, both feature sets).
- 2026-10-11: Config `target` (item 14). Set-and-forget users can pin a game in
  `minihud.toml` instead of passing `--match` each launch; a CLI `--match` wins.
  Config test guards the field; example + README/CHANGELOG updated. Smoke-tested:
  the overlay starts clean with a `target`. Gate green (177 tests, both feature
  sets).
- 2026-10-11: Fixed a stale-fps bug (item 15): when the target died the GUI loop
  dropped the PresentMon feed but never cleared the snapshot, so the hero froze
  on the last fps forever instead of reading `--`. `FeedAction::Stop` now clears
  fps/1% low, and target changes log (`overlay: following X (pid N)` / `target
  gone`), matching the headless mode. Gate green (177 tests, both feature sets).
- 2026-10-11: Full-density FPS sparkline (item 16). `gui::spark::normalize`
  (TDD, 3 tests) maps a series to 0..=1 heights; the snapshot carries a capped
  fps history (120) the `fps_loop` fills; the app draws the line in Full density
  (nothing until ≥2 samples). Smoke-tested: Full renders clean, no panic. Gate
  green (180 tests, both feature sets).
- 2026-10-11: Demo mode + README screenshots (item 17). `gui::demo` (TDD, 3
  tests) yields plausible, deterministic, time-varying samples;
  `MINIHUD_DEMO=1` makes `run_gui` feed them instead of the real pollers. Captured
  four variations through egui-mcp into `docs/` (zen/standard/full/full-magenta)
  and linked them in the README. Gate green (183 tests, both feature sets).
- 2026-10-11: Design pass from the screenshots (item 18). Compact memory
  (`9.2/16.0 GB`), a reworked hero (big number + stacked `FPS` / `1% low`), and
  thermal tints on temperature values (`theme::temp_color`, `Metric.tint`) so a
  hot reading reads at a glance like a load bar. All TDD. Re-captured the four
  README screenshots. Gate green (186 tests, both feature sets).
- 2026-10-11: Threshold tune (item 19). A normal ~60% load read amber; the bands
  are now green <70%, amber <85%, red ≥85% (so a value rounding to "90%" already
  reads alarm and the number never disagrees with the bar). Temps get their own
  bands (green <65 °C, amber <80 °C, red above). Removed the sparkline's stray
  background box. Re-captured the two Full screenshots. Gate green (187 tests).
