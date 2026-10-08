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
use ui::{draw_graph, draw_text, TextBrush};
use win::overlay::Overlay;
use windows::core::Result;
use windows::Win32::System::Threading::GetCurrentProcessId;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_F7, VK_F8};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE, WM_QUIT,
};

fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();
    let args = parse_args();
    let cfg = Config::load();
    tracing::info!("config at {:?}: {:?}", Config::path(), cfg);
    if let Some(f) = &args.process {
        tracing::info!("process filter: {}", f.label());
    }
    let win_h = 350; // fixed: stable rows, no resize flicker
    let overlay = Overlay::new("minihud", cfg.x, cfg.y, 500, win_h, cfg.text_size)?;
    tracing::info!("overlay hwnd: {:?}", overlay.hwnd);
    overlay.set_click_through(cfg.click_through);
    tracing::info!("hotkeys: F7 toggle, F8 click-through (polled)");
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
    let text_alpha = cfg.opacity.clamp(0.2, 1.0);
    let white_brush = overlay
        .rt()
        .and_then(|rt| TextBrush::new(rt, 1.0, 1.0, 1.0, text_alpha).ok());
    if white_brush.is_none() {
        tracing::warn!("no render target; overlay will be empty");
    }
    loop {
        // Non-blocking pump: GetMessageW would block and freeze rendering.
        while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
            match msg.message {
                WM_QUIT => {
                    return Ok(());
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
        let f7_now = unsafe { GetAsyncKeyState(VK_F7.0 as i32) } < 0;
        if f7_now && !f7_down {
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
            tracing::info!(
                "frame {frames}: fps={:.0} avg_ms={:.2} cpu={:.0}% ram={}/{}MB vram={}/{}MB gpu={:?} app=[{app_txt}] etw_dropped={} etw_start={etw_start} etw_stop={etw_stop} pids=[{snap}] lagmax={}ms",
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
            );
        }
        if visible {
            overlay.begin_draw();
            if let (Some(rt), Some(fmt), Some(big)) =
                (overlay.rt(), overlay.fmt(), overlay.fmt_big())
            {
                let brush_opt = white_brush.as_ref().map(|b| &b.brush);
                // Fall back to a per-frame brush if the cached one failed.
                let _fallback;
                let brush = match brush_opt {
                    Some(b) => b,
                    None => {
                        _fallback = TextBrush::new(rt, 1.0, 1.0, 1.0, 1.0).ok();
                        match _fallback.as_ref() {
                            Some(b) => &b.brush,
                            None => {
                                let _ = overlay.end_draw();
                                std::thread::sleep(std::time::Duration::from_millis(8));
                                continue;
                            }
                        }
                    }
                };
                // RTSS-style vertical stack. "--" wherever the game is
                // silent; layout never shifts. No overlay-own stats on
                // screen by design (temps/clocks/watts need vendor APIs
                // and are omitted until then, not faked).
                let app = apps.top(self_pid, args.process.as_ref());
                let (app_name, fps_txt, min_t, avg_t, max_t, low_t, hz_game) = match &app {
                    Some(a) => {
                        let iv = &a.recent_ms;
                        let min = iv.iter().cloned().fold(f32::INFINITY, f32::min);
                        let max = iv.iter().cloned().fold(0.0f32, f32::max);
                        (
                            a.name.clone(),
                            format!("{:3.0} FPS", a.fps),
                            fmt_ms(min),
                            format!("{:5.2} ms", a.avg_ms),
                            fmt_ms(max),
                            match low1_fps(iv) {
                                Some(f) => format!("{f:3.0} FPS"),
                                None => "--".to_string(),
                            },
                            format!("{:3.0} Hz", a.fps),
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
                let gpu_txt = match stats.gpu_percent {
                    Some(g) => format!("{g:3.0}%"),
                    None => "--".to_string(),
                };
                let _ = draw_text(rt, fmt, brush, 8.0, 4.0, &app_name);
                let _ = draw_text(rt, big, brush, 8.0, 20.0, &fps_txt);
                if cfg.show_frametime_graph {
                    if let Some(a) = &app {
                        // Fixed 50ms ceiling: steady rates render flat.
                        let _ = draw_graph(rt, brush, &a.recent_ms, 150.0, 8.0, 300.0, 44.0, 50.0);
                    }
                }
                let api_txt = match &app {
                    Some(a) => format!("API  {}", a.api),
                    None => "API  --".to_string(),
                };
                let _ = draw_text(rt, fmt, brush, 8.0, 108.0, &api_txt);
                let _ = draw_text(rt, fmt, brush, 8.0, 126.0, &format!("min  {min_t}"));
                let _ = draw_text(rt, fmt, brush, 8.0, 144.0, &format!("avg  {avg_t}"));
                let _ = draw_text(rt, fmt, brush, 8.0, 162.0, &format!("max  {max_t}"));
                let _ = draw_text(rt, fmt, brush, 8.0, 180.0, &format!("1%   {low_t}"));
                let _ = draw_text(rt, fmt, brush, 8.0, 198.0, "CPU:");
                let _ = draw_text(
                    rt,
                    fmt,
                    brush,
                    8.0,
                    216.0,
                    &format!("{:3.0} %", stats.cpu_percent),
                );
                let _ = draw_text(
                    rt,
                    fmt,
                    brush,
                    8.0,
                    234.0,
                    &format!("RAM  {}", mem_txt(stats.ram_used_mb, stats.ram_total_mb)),
                );
                let _ = draw_text(rt, fmt, brush, 8.0, 252.0, "GPU:");
                let _ = draw_text(rt, fmt, brush, 8.0, 270.0, &format!("{gpu_txt}"));
                let _ = draw_text(
                    rt,
                    fmt,
                    brush,
                    8.0,
                    288.0,
                    &format!(
                        "VRAM {}",
                        mem_txt(stats.gpu_vram_used_mb, stats.gpu_vram_total_mb)
                    ),
                );
                let _ = draw_text(rt, fmt, brush, 8.0, 306.0, &hz_game);
                let _ = draw_text(rt, fmt, brush, 8.0, 324.0, &display_hz());
            }
            let _ = overlay.end_draw();
        }
        std::thread::sleep(std::time::Duration::from_millis(8));
    }
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
