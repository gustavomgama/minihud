mod config;
mod fps;
mod hw;
mod ui;
mod win;

use config::{ui as config_ui, Config};
use hw::HwPoller;
use ui::{draw_text, TextBrush};
use win::overlay::Overlay;
use windows::core::Result;
use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_F7, VK_F8, VK_SHIFT};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE, WM_QUIT,
};

fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();
    tracing::info!("minihud v{}", env!("CARGO_PKG_VERSION"));
    parse_args(); // --help exits; unknown args are fatal
    let cfg = Config::load();
    tracing::info!("config at {:?}: {:?}", Config::path(), cfg);
    tracing::info!(
        "poll cadence: hw={}ms active/{}ms idle (floor 50ms)",
        cfg.update_hw_ms,
        cfg.idle_hw_ms
    );
    let win_h = 240; // fixed: stable rows, no resize flicker
    let mut overlay = Overlay::new("minihud", cfg.x, cfg.y, 500, win_h, cfg.text_size)?;
    tracing::info!("overlay hwnd: {:?}", overlay.hwnd);
    overlay.set_click_through(cfg.click_through);
    tracing::info!("hotkeys: F7 toggle, F8 click-through, Shift+F7 quit (polled)");
    let mut hw = HwPoller::new(cfg.update_hw_ms, cfg.idle_hw_ms);
    let mut fps_cap = fps::Capture::new();
    let mut visible = true;
    let mut click_through = cfg.click_through;
    let mut msg = MSG::default();
    let mut frames: u64 = 0;
    // Render-on-demand state: redraw only when the pixels would differ.
    // `force_draw` fires on toggles/recovery/startup; otherwise the
    // frame key (row strings) decides.
    let mut last_drawn: Option<Vec<String>> = None;
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
    // values near-white, labels dim slate.
    struct Brushes {
        values: TextBrush,
        labels: TextBrush,
    }
    fn make_brushes(overlay: &Overlay, text_alpha: f32) -> Option<Brushes> {
        let rt = overlay.rt()?;
        Some(Brushes {
            values: TextBrush::new(rt, 1.0, 1.0, 1.0, text_alpha).ok()?,
            labels: TextBrush::new(rt, 0.60, 0.64, 0.72, text_alpha).ok()?,
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
        if let Some(s) = hw.update() {
            let _ = s;
        }
        let stats = hw.cached().clone();
        frames += 1;
        if let Some(fps) = fps_cap.tick() {
            tracing::info!("fps={:.1}", fps);
        }
        // Per-iteration: did this loop present anything? Idle loops
        // sleep longer (hotkeys stay responsive either way).
        let mut drew = false;
        // Poll fallback for hotkeys (edge-triggered).
        let shift_down = unsafe { GetAsyncKeyState(VK_SHIFT.0 as i32) } < 0;
        let f7_now = unsafe { GetAsyncKeyState(VK_F7.0 as i32) } < 0;
        if f7_now && !f7_down {
            // Shift+F7: clean quit. Plain F7 toggles visibility.
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
            if shift_down {
                // Shift+F8: open minimal config UI (writes minihud.toml, non-blocking).
                if let Err(e) = config_ui::edit_config() {
                    tracing::warn!("config ui failed: {e}");
                } else {
                    tracing::info!("config saved (Shift+F8)");
                }
            } else {
                click_through = !click_through;
                overlay.set_click_through(click_through);
                force_draw = true;
                tracing::info!("click_through={click_through} (F8)");
            }
        }
        f8_down = f8_now;
        if frames % 300 == 1 {
            tracing::info!(
                "frame {frames}: cpu={:.0}% ram={} vram={} gpu={:?} cputp={} gpusen=[{}] hwms={} skip={} ready={}",
                stats.cpu_percent,
                mem_txt_opt(stats.ram_used_mb, stats.ram_total_mb),
                mem_txt_opt(stats.gpu_vram_used_mb, stats.gpu_vram_total_mb),
                stats.gpu_percent.unwrap_or(0.0),
                format!(
                    "{}/{}",
                    stats.cpu_temp_c.map(|t| format!("{t:.0}C")).as_deref().unwrap_or("--"),
                    stats.cpu_power_w.map(|p| format!("{p:.0}W")).as_deref().unwrap_or("--"),
                ),
                format!(
                    "{}/{}/{}",
                    stats.gpu_temp_c.map(|t| format!("{t:.0}C")).as_deref().unwrap_or("--"),
                    stats.gpu_power_w.map(|p| format!("{p:.0}W")).as_deref().unwrap_or("--"),
                    match (stats.gpu_core_mhz, stats.gpu_mem_mhz) {
                        (Some(c), Some(m)) => format!("{c}/{m}MHz"),
                        _ => "--".to_string(),
                    },
                ),
                hw.interval_ms(),
                std::mem::replace(&mut skipped, 0),
                hw.ready(),
            );
        }
        if visible {
            // Hardware rows only: every value comes from LibreHardwareMonitor
            // (PDH/NVML/DXGI paths are deleted). "--" until the first
            // bridge sample lands, and wherever firmware yields nothing.
            let ready = hw.ready();
            let show = |v: String| if ready { v } else { "--".to_string() };
            // Values in fixed draw order for the dirty check below:
            // 0 cpu%, 1 cputemp, 2 cpupower, 3 ram, 4 gpu%, 5 gputemp,
            // 6 gpupower, 7 core, 8 mem, 9 vram.
            let vals: Vec<String> = vec![
                show(format!("{:3.0} %", stats.cpu_percent.max(0.0))),
                show(opt_f32(stats.cpu_temp_c, "°C", 0)),
                show(opt_f32(stats.cpu_power_w, "W", 0)),
                show(mem_txt_opt(stats.ram_used_mb, stats.ram_total_mb)),
                show(match stats.gpu_percent {
                    Some(g) => format!("{:3.0} %", g.max(0.0)),
                    None => "--".to_string(),
                }),
                show(opt_f32(stats.gpu_temp_c, "°C", 0)),
                show(opt_f32(stats.gpu_power_w, "W", 0)),
                show(opt_u32(stats.gpu_core_mhz, "MHz")),
                show(opt_u32(stats.gpu_mem_mhz, "MHz")),
                show(mem_txt_opt(stats.gpu_vram_used_mb, stats.gpu_vram_total_mb)),
            ];
            let dirty = force_draw || last_drawn.as_ref() != Some(&vals);
            if dirty {
                if let (Some(rt), Some(fmt), Some(b)) =
                    (overlay.rt(), overlay.fmt(), brushes.as_ref())
                {
                    let bv = &b.values.brush;
                    let bl = &b.labels.brush;
                    // Two-column row: dim label at x=8, bright value.
                    let row = |y: f32, label: &str, value: &str| {
                        let _ = draw_text(rt, fmt, bl, 8.0, y, label);
                        let _ = draw_text(rt, fmt, bv, 104.0, y, value);
                    };
                    overlay.begin_draw();
                    let _ = draw_text(rt, fmt, bl, 8.0, 4.0, "CPU:");
                    row(22.0, "load", &vals[0]);
                    row(40.0, "temp", &vals[1]);
                    row(58.0, "power", &vals[2]);
                    row(76.0, "RAM", &vals[3]);
                    let _ = draw_text(rt, fmt, bl, 8.0, 98.0, "GPU:");
                    row(116.0, "load", &vals[4]);
                    row(134.0, "temp", &vals[5]);
                    row(152.0, "power", &vals[6]);
                    row(170.0, "core", &vals[7]);
                    row(188.0, "mem", &vals[8]);
                    row(206.0, "VRAM", &vals[9]);
                    // EndDraw fails when the D2D device is lost (driver
                    // update, TDR, GPU switch): drop the dead resources
                    // and rebuild on a 2s timer until the device is back.
                    if let Err(e) = overlay.end_draw() {
                        tracing::warn!("end_draw failed (device lost?): {e:?}; rebuilding");
                        overlay.invalidate();
                        brushes = None;
                        last_recover = Some(std::time::Instant::now());
                    } else {
                        last_drawn = Some(vals);
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
        // Data cadence lives in HwPoller (adaptive, 50ms floor).
        std::thread::sleep(std::time::Duration::from_millis(if drew { 8 } else { 30 }));
    }
    tracing::info!("minihud stopped cleanly");
    Ok(())
}

/// "12.1/31.9 GB" or "512/1024 MB" — GB when the total is 2+ GB.
/// "--" unless both ends are known (LHM hasn't delivered yet).
fn mem_txt_opt(used_mb: Option<u64>, total_mb: Option<u64>) -> String {
    match (used_mb, total_mb) {
        (Some(u), Some(t)) if t > 0 => {
            if t >= 2048 {
                format!("{:.1}/{:.1} GB", u as f64 / 1024.0, t as f64 / 1024.0)
            } else {
                format!("{u}/{t} MB")
            }
        }
        _ => "--".to_string(),
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

/// Minimal CLI: `--help` prints usage. No arg library on purpose.
struct Args {}

fn parse_args() -> Args {
    let mut it = std::env::args().skip(1);
    while let Some(a) = it.next() {
        match a.as_str() {
            "--help" | "-h" => {
                println!("minihud — minimal Windows performance overlay");
                println!();
                println!("Usage: minihud.exe [--help]");
                println!();
                println!("All hardware data comes from LibreHardwareMonitor");
                println!("(see README for the one-time DLL fetch).");
                println!();
                println!("Keys: F7 show/hide, F8 click-through on/off,");
                println!("      Shift+F7 clean quit.");
                std::process::exit(0);
            }
            other => {
                eprintln!("minihud: unknown arg {other:?} (try --help)");
                std::process::exit(2);
            }
        }
    }
    Args {}
}
