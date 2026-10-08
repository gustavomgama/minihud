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
    let win_h = if cfg.show_frametime_graph { 126 } else { 72 };
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
                .map(|(n, _, c)| format!("{n}:{c}"))
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
            if let (Some(rt), Some(fmt)) = (overlay.rt(), overlay.fmt()) {
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
                // Sections: APP = tracked game, SYS = machine, GPU = adapter.
                // No HUD-own stats on screen by design; the graph shows the
                // GAME's frame intervals, never the overlay's refresh.
                let gpu_txt = match stats.gpu_percent {
                    Some(g) => format!("{g:3.0}%"),
                    None => "--".to_string(),
                };
                let app = apps.top(self_pid, args.process.as_ref());
                let line1 = match &app {
                    Some(a) => format!(
                        "APP  {} {:3.0} FPS | {:5.2} ms avg",
                        a.name, a.fps, a.avg_ms
                    ),
                    None => apps.status_text(args.process.as_ref()),
                };
                let line2 = format!(
                    "SYS  CPU {:3.0}% | RAM {}",
                    stats.cpu_percent,
                    mem_txt(stats.ram_used_mb, stats.ram_total_mb),
                );
                let line3 = format!(
                    "GPU  {gpu_txt} | VRAM {}",
                    mem_txt(stats.gpu_vram_used_mb, stats.gpu_vram_total_mb),
                );
                let _ = draw_text(rt, fmt, brush, 8.0, 4.0, &line1);
                let _ = draw_text(rt, fmt, brush, 8.0, 22.0, &line2);
                let _ = draw_text(rt, fmt, brush, 8.0, 40.0, &line3);
                if cfg.show_frametime_graph {
                    if let Some(a) = &app {
                        let _ = draw_graph(rt, brush, &a.recent_ms, 8.0, 62.0, 460.0, 52.0);
                    }
                }
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
