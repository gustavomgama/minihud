mod appstats;
mod config;
mod hw;
mod timing;
mod ui;
mod win;

use appstats::AppTracker;
use config::Config;
use hw::HwPoller;
use timing::PresentTimer;
use ui::{draw_graph, draw_hline, draw_text, TextBrush};
use win::overlay::Overlay;
use windows::core::Result;
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_F7, VK_F8, VK_SHIFT};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE, WM_QUIT,
};

fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();
    tracing::info!("minihud v{}", env!("CARGO_PKG_VERSION"));
    let args = parse_args();
    let cfg = Config::load();
    tracing::info!("config at {:?}: {:?}", Config::path(), cfg);
    tracing::info!(
        "poll cadence: hw={}ms active/{}ms idle, app=700ms recompute (ETW delivers ~1s batches)",
        cfg.update_hw_ms,
        cfg.idle_hw_ms
    );
    if let Some(f) = &args.process {
        tracing::info!("process filter: {}", f.label());
    }
    let win_h = 486; // fixed: stable rows, no resize flicker
    let mut overlay = Overlay::new("minihud", cfg.x, cfg.y, 500, win_h, cfg.text_size)?;
    tracing::info!("overlay hwnd: {:?}", overlay.hwnd);
    overlay.set_click_through(cfg.click_through);
    tracing::info!("hotkeys: F7 toggle, F8 click-through, Shift+F7 quit (polled)");
    let mut timer = PresentTimer::new(180);
    let mut hw = HwPoller::new(cfg.update_hw_ms, cfg.idle_hw_ms);
    // Per-app presents via ETW (needs elevation; degrades to placeholder).
    let apps = AppTracker::start();
    let self_pid = unsafe { GetCurrentProcessId() };
    let mut visible = true;
    let mut click_through = cfg.click_through;
    let mut msg = MSG::default();
    let mut frames: u64 = 0;
    // Render-on-demand state: redraw only when the pixels would differ.
    // `force_draw` fires on toggles/recovery/startup; otherwise the
    // frame key (row strings + graph fingerprint) decides.
    let mut last_drawn: Option<(Vec<String>, (usize, i32))> = None;
    let mut force_draw = true;
    let mut skipped: u64 = 0;
    // Single path: edge-triggered GetAsyncKeyState poll. RegisterHotKey was
    // removed — it double-fired with the poll on the same press.
    let mut f7_down = false;
    let mut f8_down = false;
    // Create brushes once outside the loop (cheaper, less flicker).
    // Text alpha follows cfg.opacity (window itself is opaque for now).
    // Dropped + rebuilt if the D2D device is lost (see end_draw below).
    let text_alpha = cfg.opacity.clamp(0.2, 1.0);
    // Brush set, built once and rebuilt after device loss. Tiers:
    // values near-white, labels dim slate, big FPS tinted by threshold
    // (numbers always accompany color: never color-only meaning).
    struct Brushes {
        values: TextBrush,
        labels: TextBrush,
        good: TextBrush,
        warn: TextBrush,
        bad: TextBrush,
    }
    fn make_brushes(overlay: &Overlay, text_alpha: f32) -> Option<Brushes> {
        let rt = overlay.rt()?;
        Some(Brushes {
            values: TextBrush::new(rt, 1.0, 1.0, 1.0, text_alpha).ok()?,
            labels: TextBrush::new(rt, 0.60, 0.64, 0.72, text_alpha).ok()?,
            good: TextBrush::new(rt, 0.35, 0.87, 0.55, text_alpha).ok()?,
            warn: TextBrush::new(rt, 1.0, 0.75, 0.25, text_alpha).ok()?,
            bad: TextBrush::new(rt, 1.0, 0.42, 0.40, text_alpha).ok()?,
        })
    }
    let mut brushes = make_brushes(&overlay, text_alpha);
    if brushes.is_none() {
        tracing::warn!("no render target; overlay will be empty");
    }
    // Last device-recovery attempt (throttled: retry every 2s, not per frame).
    let mut last_recover: Option<std::time::Instant> = None;
    // 'run: the WM_QUIT arm below sits inside the message-pump `while`,
    // so a bare `break` would only exit the pump and the overlay would
    // keep running. Labeled break quits for real.
    'run: loop {
        // Non-blocking pump: GetMessageW would block and freeze rendering.
        while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
            match msg.message {
                WM_QUIT => {
                    tracing::info!("quit requested (WM_QUIT)");
                    break 'run;
                }
                _ => unsafe {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                },
            }
        }
        let _ = timer.tick();
        if let Some(s) = hw.update() {
            let _ = s;
        }
        let stats = hw.cached().clone();
        frames += 1;
        // Per-iteration: did this loop present anything? Idle loops
        // sleep longer (hotkeys stay responsive either way).
        let mut drew = false;
        // Poll fallback for hotkeys (edge-triggered).
        let shift_down = unsafe { GetAsyncKeyState(VK_SHIFT.0 as i32) } < 0;
        let f7_now = unsafe { GetAsyncKeyState(VK_F7.0 as i32) } < 0;
        if f7_now && !f7_down {
            // Shift+F7: clean quit (stops the ETW session, no lingering
            // kernel trace). Plain F7 toggles visibility.
            if shift_down {
                tracing::info!("quit requested (Shift+F7)");
                break 'run;
            }
            visible = !visible;
            overlay.set_visible(visible);
            force_draw = true;
            tracing::info!("visible={visible} (F7)");
        }
        f7_down = f7_now;
        let f8_now = unsafe { GetAsyncKeyState(VK_F8.0 as i32) } < 0;
        if f8_now && !f8_down {
            click_through = !click_through;
            overlay.set_click_through(click_through);
            force_draw = true; // background shade changes with the mode
            tracing::info!("click_through={click_through} (F8)");
        }
        f8_down = f8_now;
        if frames % 300 == 1 {
            let s = timer.stats();
            let app_txt = match apps.top(self_pid, args.process.as_ref()) {
                Some(a) => format!("{} {:.0}fps {:.2}ms", a.name, a.fps, a.avg_ms),
                None => apps.status_text(args.process.as_ref()),
            };
            let (etw_start, etw_stop) = appstats::etw::seen();
            let snap = apps
                .snapshot()
                .iter()
                .map(|(n, _, c, a)| format!("{n}:{c}/{a}"))
                .collect::<Vec<_>>()
                .join(",");
            let nv_txt = format!(
                "{}/{}/{}",
                stats
                    .gpu_temp_c
                    .map(|t| format!("{t:.0}C"))
                    .as_deref()
                    .unwrap_or("--"),
                stats
                    .gpu_power_w
                    .map(|p| format!("{p:.0}W"))
                    .as_deref()
                    .unwrap_or("--"),
                match (stats.gpu_core_mhz, stats.gpu_mem_mhz) {
                    (Some(c), Some(m)) => format!("{c}/{m}MHz"),
                    _ => "--".to_string(),
                },
            );
            tracing::info!(
                "frame {frames}: fps={:.0} avg_ms={:.2} cpu={:.0}% ram={}/{}MB vram={}/{}MB gpu={:?} app=[{app_txt}] etw_dropped={} etw_start={etw_start} etw_stop={etw_stop} pids=[{snap}] lagmax={}ms nv=[{nv_txt}] cpumhz={} hwms={} skip={} cputp={}",
                s.fps,
                s.avg_ms,
                stats.cpu_percent,
                stats.ram_used_mb,
                stats.ram_total_mb,
                stats.gpu_vram_used_mb,
                stats.gpu_vram_total_mb,
                stats.gpu_percent,
                appstats::etw::dropped(),
                appstats::etw::max_lag_ms(),
                stats.cpu_mhz.map(|m| m.to_string()).as_deref().unwrap_or("--"),
                hw.interval_ms(),
                std::mem::replace(&mut skipped, 0),
                format!(
                    "{}/{}",
                    stats.cpu_temp_c.map(|t| format!("{t:.0}C")).as_deref().unwrap_or("--"),
                    stats.cpu_power_w.map(|p| format!("{p:.0}W")).as_deref().unwrap_or("--"),
                ),
            );
        }
        if visible {
            // RTSS-style vertical stack, label|value columns. "--"
            // wherever the game is silent; layout never shifts.
            let app = apps.top(self_pid, args.process.as_ref());
            // Raw poll values, no per-frame smoothing: digits step
            // exactly when source data steps (200ms HW / ~1s ETW),
            // so the perceived rate IS the poll rate.
            let app_fps = app.as_ref().map(|a| a.fps).unwrap_or(0.0);
            let cpu_txt = format!("{:3.0} %", stats.cpu_percent.max(0.0));
            let gpu_txt = match stats.gpu_percent {
                Some(g) => format!("{:3.0} %", g.max(0.0)),
                None => "--".to_string(),
            };
            // Threshold tint on the headline number only; every value
            // keeps its digits (never color-only meaning). Resolved
            // to a brush at draw time below.
            #[derive(Clone, Copy, PartialEq)]
            enum Tint {
                Plain,
                Good,
                Warn,
                Bad,
            }
            let tint = if app.is_none() {
                Tint::Plain
            } else if app_fps >= 120.0 {
                Tint::Good
            } else if app_fps >= 60.0 {
                Tint::Warn
            } else {
                Tint::Bad
            };
            let (app_name, fps_txt, min_t, avg_t, max_t, low_t, hz_game) = match &app {
                Some(a) => {
                    let iv = &a.recent_ms;
                    let min = iv.iter().cloned().fold(f32::INFINITY, f32::min);
                    let max = iv.iter().cloned().fold(0.0f32, f32::max);
                    (
                        a.name.clone(),
                        format!("{app_fps:3.0} FPS"),
                        fmt_ms(min),
                        format!("{:5.2} ms", a.avg_ms),
                        fmt_ms(max),
                        match low1_fps(iv) {
                            Some(f) => format!("{f:3.0} FPS"),
                            None => "--".to_string(),
                        },
                        format!("{app_fps:3.0} Hz"),
                    )
                }
                None => (
                    apps.status_text(args.process.as_ref()),
                    "--".to_string(),
                    "--".to_string(),
                    "--".to_string(),
                    "--".to_string(),
                    "--".to_string(),
                    "--".to_string(),
                ),
            };
            // Values in fixed draw order for the dirty check below.
            let vals: Vec<String> = vec![
                app_name.clone(),
                fps_txt.clone(),
                match &app {
                    Some(a) => a.api.clone(),
                    None => "--".to_string(),
                },
                min_t.clone(),
                avg_t.clone(),
                max_t.clone(),
                low_t.clone(),
                cpu_txt,
                opt_u32(stats.cpu_mhz, "MHz"),
                opt_f32(stats.cpu_temp_c, "°C", 0),
                opt_f32(stats.cpu_power_w, "W", 0),
                mem_txt(stats.ram_used_mb, stats.ram_total_mb),
                gpu_txt.clone(),
                opt_f32(stats.gpu_temp_c, "°C", 0),
                opt_f32(stats.gpu_power_w, "W", 0),
                opt_u32(stats.gpu_core_mhz, "MHz"),
                opt_u32(stats.gpu_mem_mhz, "MHz"),
                mem_txt(stats.gpu_vram_used_mb, stats.gpu_vram_total_mb),
                hz_game.clone(),
                display_hz(),
            ];
            // Graph fingerprint: length + last interval quantized.
            // Frozen data => stable fingerprint => frame skipped.
            let graph_fp: (usize, i32) = match &app {
                Some(a) => (
                    a.recent_ms.len(),
                    a.recent_ms.last().map(|v| (v * 2.0) as i32).unwrap_or(-1),
                ),
                None => (0, -1),
            };
            let dirty = force_draw || last_drawn.as_ref() != Some(&(vals.clone(), graph_fp));
            if dirty {
                if let (Some(rt), Some(fmt), Some(big), Some(b)) = (
                    overlay.rt(),
                    overlay.fmt(),
                    overlay.fmt_big(),
                    brushes.as_ref(),
                ) {
                    let bv = &b.values.brush;
                    let bl = &b.labels.brush;
                    let tint_brush = match tint {
                        Tint::Good => &b.good.brush,
                        Tint::Warn => &b.warn.brush,
                        Tint::Bad => &b.bad.brush,
                        Tint::Plain => bv,
                    };
                    // Two-column row: dim label at x=8, bright value.
                    let row = |y: f32, label: &str, value: &str| {
                        let _ = draw_text(rt, fmt, bl, 8.0, y, label);
                        let _ = draw_text(rt, fmt, bv, 104.0, y, value);
                    };
                    overlay.begin_draw();
                    let _ = draw_text(rt, fmt, bv, 8.0, 4.0, &vals[0]);
                    let _ = draw_text(rt, big, tint_brush, 8.0, 20.0, &vals[1]);
                    if cfg.show_frametime_graph {
                        if let Some(a) = &app {
                            // Fixed 50ms ceiling: steady rates render flat.
                            let _ = draw_graph(rt, bv, &a.recent_ms, 150.0, 8.0, 300.0, 44.0, 50.0);
                            // 16.7ms = 60fps target line.
                            let _ = draw_hline(rt, bl, 150.0, 450.0, 37.3);
                        }
                    }
                    row(108.0, "API", &vals[2]);
                    row(126.0, "min", &vals[3]);
                    row(144.0, "avg", &vals[4]);
                    row(162.0, "max", &vals[5]);
                    row(180.0, "1%", &vals[6]);
                    let _ = draw_text(rt, fmt, bl, 8.0, 202.0, "CPU:");
                    row(220.0, "load", &vals[7]);
                    row(238.0, "clock", &vals[8]);
                    row(256.0, "temp", &vals[9]);
                    row(274.0, "power", &vals[10]);
                    row(292.0, "RAM", &vals[11]);
                    let _ = draw_text(rt, fmt, bl, 8.0, 314.0, "GPU:");
                    row(332.0, "load", &vals[12]);
                    row(350.0, "temp", &vals[13]);
                    row(368.0, "power", &vals[14]);
                    row(386.0, "core", &vals[15]);
                    row(404.0, "mem", &vals[16]);
                    row(422.0, "VRAM", &vals[17]);
                    row(440.0, "GAME", &vals[18]);
                    row(458.0, "DISP", &vals[19]);
                    // EndDraw fails when the D2D device is lost (driver
                    // update, TDR, GPU switch): drop the dead resources
                    // and rebuild on a 2s timer until the device is back.
                    if let Err(e) = overlay.end_draw() {
                        tracing::warn!("end_draw failed (device lost?): {e:?}; rebuilding");
                        overlay.invalidate();
                        brushes = None;
                        last_recover = Some(std::time::Instant::now());
                    } else {
                        last_drawn = Some((vals, graph_fp));
                        force_draw = false;
                        drew = true;
                    }
                }
            } else {
                skipped += 1;
            }
        }
        // Device recovery runs even when hidden so un-hiding never shows
        // a dead target. Throttled to 2s to avoid log spam while the
        // device is gone. A rebuilt target forces the next redraw.
        if overlay.rt().is_none()
            && last_recover.is_none_or(|t| t.elapsed() >= std::time::Duration::from_secs(2))
        {
            match overlay.ensure_target() {
                Ok(()) => {
                    brushes = make_brushes(&overlay, text_alpha);
                    tracing::info!("render target rebuilt");
                    last_recover = None;
                    force_draw = true;
                }
                Err(e) => {
                    tracing::debug!("render target rebuild failed: {e:?}");
                    last_recover = Some(std::time::Instant::now());
                }
            }
        }
        // Heartbeat, not a poll rate: 8ms keeps hotkeys/quit snappy,
        // 30ms when frames are skipped (render-on-demand idle).
        // Data cadence lives in HwPoller (adaptive) and TOP_TTL (200ms).
        std::thread::sleep(std::time::Duration::from_millis(if drew { 8 } else { 30 }));
    }
    // Clean exit: stop the ETW session so no kernel trace lingers, then
    // report how to restart.
    appstats::etw::stop_session();
    tracing::info!("minihud stopped cleanly");
    Ok(())
}

/// "12.1/31.9 GB" or "512/1024 MB" — GB when the total is 2+ GB.
fn mem_txt(used_mb: u64, total_mb: u64) -> String {
    if total_mb >= 2048 {
        format!(
            "{:.1}/{:.1} GB",
            used_mb as f64 / 1024.0,
            total_mb as f64 / 1024.0
        )
    } else {
        format!("{used_mb}/{total_mb} MB")
    }
}

/// "4442 MHz" / "70 °C" / "187 W", or "--" when the sensor is missing.
fn opt_u32(v: Option<u32>, unit: &str) -> String {
    match v {
        Some(x) => format!("{x} {unit}"),
        None => "--".to_string(),
    }
}

/// Same for f32 sensors (temp, power), with decimals.
fn opt_f32(v: Option<f32>, unit: &str, decimals: usize) -> String {
    match v {
        Some(x) => format!("{x:.decimals$} {unit}"),
        None => "--".to_string(),
    }
}

/// "6.34 ms", or "--" when there is no data (infinite/NaN).
fn fmt_ms(v: f32) -> String {
    if v.is_finite() && v >= 0.0 {
        format!("{v:5.2} ms")
    } else {
        "--".to_string()
    }
}

/// 1% low FPS: mean of the worst 1% frame intervals, as a rate.
fn low1_fps(samples: &[f32]) -> Option<f32> {
    if samples.is_empty() {
        return None;
    }
    let mut sorted = samples.to_vec();
    sorted.sort_by(|a, b| b.partial_cmp(a).unwrap_or(std::cmp::Ordering::Equal));
    let k = ((sorted.len() as f64 * 0.01).ceil() as usize)
        .max(1)
        .min(sorted.len());
    let avg = sorted[..k].iter().sum::<f32>() / k as f32;
    if avg > 0.0 {
        Some(1000.0 / avg)
    } else {
        None
    }
}

/// Display refresh rate ("60 Hz"), cached. "--" when unreadable.
fn display_hz() -> String {
    use std::sync::OnceLock;
    use windows::Win32::Graphics::Gdi::{EnumDisplaySettingsW, DEVMODEW, ENUM_CURRENT_SETTINGS};
    static HZ: OnceLock<String> = OnceLock::new();
    HZ.get_or_init(|| unsafe {
        let mut dm = DEVMODEW::default();
        dm.dmSize = std::mem::size_of::<DEVMODEW>() as u16;
        if EnumDisplaySettingsW(
            windows::core::PCWSTR::null(),
            ENUM_CURRENT_SETTINGS,
            &mut dm,
        )
        .as_bool()
            && dm.dmDisplayFrequency > 1
        {
            format!("{} Hz", dm.dmDisplayFrequency)
        } else {
            "--".to_string()
        }
    })
    .clone()
}

/// Minimal CLI: `--process NAME|PID` pins the APP row to one process,
/// `--help` prints usage. No arg library on purpose (one flag).
struct Args {
    process: Option<appstats::ProcessFilter>,
}

fn parse_args() -> Args {
    use appstats::ProcessFilter;
    let mut it = std::env::args().skip(1);
    let mut process = None;
    while let Some(a) = it.next() {
        match a.as_str() {
            "--help" | "-h" => {
                println!("minihud — minimal Windows performance overlay");
                println!();
                println!("Usage: minihud.exe [--process NAME|PID]");
                println!();
                println!("  --process NAME   track only processes whose exe name");
                println!("                   contains NAME (case-insensitive),");
                println!("                   e.g. --process Overwatch.exe");
                println!("  --process PID    track only that process id");
                println!();
                println!("Keys: F7 show/hide, F8 click-through on/off.");
                std::process::exit(0);
            }
            "--process" => match it.next() {
                Some(v) => process = Some(ProcessFilter::parse(&v)),
                None => {
                    eprintln!("minihud: --process needs a NAME or PID");
                    std::process::exit(2);
                }
            },
            other => {
                eprintln!("minihud: unknown arg {other:?} (try --help)");
                std::process::exit(2);
            }
        }
    }
    Args { process }
}
