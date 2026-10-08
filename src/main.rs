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
    if let Some(f) = &args.process {
        tracing::info!("process filter: {}", f.label());
    }
    let win_h = 450; // fixed: stable rows, no resize flicker
    let mut overlay = Overlay::new("minihud", cfg.x, cfg.y, 500, win_h, cfg.text_size)?;
    tracing::info!("overlay hwnd: {:?}", overlay.hwnd);
    overlay.set_click_through(cfg.click_through);
    tracing::info!("hotkeys: F7 toggle, F8 click-through, Shift+F7 quit (polled)");
    let mut timer = PresentTimer::new(180);
    let mut hw = HwPoller::new(cfg.update_hw_ms);
    // Per-app presents via ETW (needs elevation; degrades to placeholder).
    let apps = AppTracker::start();
    let self_pid = unsafe { GetCurrentProcessId() };
    let mut visible = true;
    let mut click_through = cfg.click_through;
    let mut msg = MSG::default();
    let mut frames: u64 = 0;
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
    // Displayed (smoothed) readouts. The DATA stays raw; only presentation
    // lerps toward it (~150ms settle), so digits stop flickering without
    // hiding real changes. -1.0 = unset, snap on first frame.
    let mut disp_app_fps = -1.0f32;
    let mut disp_cpu = -1.0f32;
    let mut disp_gpu = -1.0f32;
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
            tracing::info!("visible={visible} (F7)");
        }
        f7_down = f7_now;
        let f8_now = unsafe { GetAsyncKeyState(VK_F8.0 as i32) } < 0;
        if f8_now && !f8_down {
            click_through = !click_through;
            overlay.set_click_through(click_through);
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
                "frame {frames}: fps={:.0} avg_ms={:.2} cpu={:.0}% ram={}/{}MB vram={}/{}MB gpu={:?} app=[{app_txt}] etw_dropped={} etw_start={etw_start} etw_stop={etw_stop} pids=[{snap}] lagmax={}ms nv=[{nv_txt}] cpumhz={}",
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
            );
        }
        if visible {
            overlay.begin_draw();
            if let (Some(rt), Some(fmt), Some(big)) =
                (overlay.rt(), overlay.fmt(), overlay.fmt_big())
            {
                let brush_opt = brushes.as_ref();
                // No brushes (device lost, recovery pending): skip drawing;
                // the last frame stays up and the recovery block rebuilds.
                let b = match brush_opt {
                    Some(b) => b,
                    None => {
                        let _ = overlay.end_draw();
                        std::thread::sleep(std::time::Duration::from_millis(8));
                        continue;
                    }
                };
                let bv = &b.values.brush;
                let bl = &b.labels.brush;
                // RTSS-style vertical stack, label|value columns. "--"
                // wherever the game is silent; layout never shifts. No
                // overlay-own stats on screen by design (temps/clocks need
                // vendor APIs and are omitted until then, not faked).
                let app = apps.top(self_pid, args.process.as_ref());
                // Displayed readouts lerp toward raw data (~150ms settle):
                // calm digits, same truth. Snap on first sight.
                let app_fps_raw = app.as_ref().map(|a| a.fps).unwrap_or(-1.0);
                if app_fps_raw < 0.0 {
                    disp_app_fps = -1.0;
                }
                let app_fps = smooth(&mut disp_app_fps, app_fps_raw.max(0.0));
                let cpu_txt = smooth(&mut disp_cpu, stats.cpu_percent.max(0.0));
                let gpu_raw = stats.gpu_percent.unwrap_or(-1.0);
                let gpu_txt = if gpu_raw < 0.0 {
                    disp_gpu = -1.0;
                    "--".to_string()
                } else {
                    format!("{:3.0} %", smooth(&mut disp_gpu, gpu_raw))
                };
                // Threshold tint on the headline number only; every value
                // keeps its digits (never color-only meaning).
                let tint = if app.is_none() {
                    bv
                } else if app_fps >= 120.0 {
                    &b.good.brush
                } else if app_fps >= 60.0 {
                    &b.warn.brush
                } else {
                    &b.bad.brush
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
                // Two-column row: dim label at x=8, bright value at x=104.
                let row = |y: f32, label: &str, value: &str| {
                    let _ = draw_text(rt, fmt, bl, 8.0, y, label);
                    let _ = draw_text(rt, fmt, bv, 104.0, y, value);
                };
                let _ = draw_text(rt, fmt, bv, 8.0, 4.0, &app_name);
                let _ = draw_text(rt, big, tint, 8.0, 20.0, &fps_txt);
                if cfg.show_frametime_graph {
                    if let Some(a) = &app {
                        // Fixed 50ms ceiling: steady rates render flat.
                        let _ = draw_graph(rt, bv, &a.recent_ms, 150.0, 8.0, 300.0, 44.0, 50.0);
                        // 16.7ms = 60fps target line.
                        let _ = draw_hline(rt, bl, 150.0, 450.0, 37.3);
                    }
                }
                row(
                    108.0,
                    "API",
                    &match &app {
                        Some(a) => a.api.clone(),
                        None => "--".to_string(),
                    },
                );
                row(126.0, "min", &min_t);
                row(144.0, "avg", &avg_t);
                row(162.0, "max", &max_t);
                row(180.0, "1%", &low_t);
                let _ = draw_text(rt, fmt, bl, 8.0, 202.0, "CPU:");
                row(220.0, "load", &format!("{cpu_txt}"));
                row(238.0, "clock", &opt_u32(stats.cpu_mhz, "MHz"));
                row(
                    256.0,
                    "RAM",
                    &mem_txt(stats.ram_used_mb, stats.ram_total_mb),
                );
                let _ = draw_text(rt, fmt, bl, 8.0, 278.0, "GPU:");
                row(296.0, "load", &gpu_txt);
                row(314.0, "temp", &opt_f32(stats.gpu_temp_c, "°C", 0));
                row(332.0, "power", &opt_f32(stats.gpu_power_w, "W", 0));
                row(350.0, "core", &opt_u32(stats.gpu_core_mhz, "MHz"));
                row(368.0, "mem", &opt_u32(stats.gpu_mem_mhz, "MHz"));
                row(
                    386.0,
                    "VRAM",
                    &mem_txt(stats.gpu_vram_used_mb, stats.gpu_vram_total_mb),
                );
                row(404.0, "GAME", &hz_game);
                row(422.0, "DISP", &display_hz());
            }
            // EndDraw fails when the D2D device is lost (driver update,
            // TDR, GPU switch). The old code ignored this and froze on a
            // stale frame forever; instead drop the dead resources and
            // rebuild on a 2s timer until the device is back.
            if let Err(e) = overlay.end_draw() {
                tracing::warn!("end_draw failed (device lost?): {e:?}; rebuilding");
                overlay.invalidate();
                brushes = None;
                last_recover = Some(std::time::Instant::now());
            }
        }
        // Device recovery runs even when hidden so un-hiding never shows
        // a dead target. Throttled to 2s to avoid log spam while the
        // device is gone.
        if overlay.rt().is_none()
            && last_recover.is_none_or(|t| t.elapsed() >= std::time::Duration::from_secs(2))
        {
            match overlay.ensure_target() {
                Ok(()) => {
                    brushes = make_brushes(&overlay, text_alpha);
                    tracing::info!("render target rebuilt");
                    last_recover = None;
                }
                Err(e) => {
                    tracing::debug!("render target rebuild failed: {e:?}");
                    last_recover = Some(std::time::Instant::now());
                }
            }
        }
        std::thread::sleep(std::time::Duration::from_millis(8));
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

/// Displayed readout lerps toward the raw value (~150ms settle at
/// 60fps+). Calms flickering digits without hiding real changes;
/// -1.0 means unset (snap on first sight). Presentation-only: the
/// underlying data is never smoothed.
fn smooth(current: &mut f32, target: f32) -> f32 {
    if *current < 0.0 || !target.is_finite() {
        *current = target.max(0.0);
    } else {
        *current += (target - *current) * 0.2;
    }
    *current
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
