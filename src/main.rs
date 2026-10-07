mod config;
mod hotkey;
mod hw;
mod timing;
mod ui;
mod win;

use config::Config;
use hw::HwPoller;
use timing::PresentTimer;
use ui::{draw_graph, draw_text, TextBrush};
use win::overlay::Overlay;
use windows::core::Result;
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, PeekMessageW, TranslateMessage, MSG, PM_REMOVE, WM_HOTKEY, WM_QUIT,
};

fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();
    let cfg = Config::load();
    tracing::info!("config at {:?}: {:?}", Config::path(), cfg);
    let overlay = Overlay::new("minihud", cfg.x, cfg.y, 420, 120)?;
    tracing::info!("overlay hwnd: {:?}", overlay.hwnd);
    overlay.set_click_through(cfg.click_through);
    match hotkey::register(overlay.hwnd) {
        Ok(()) => tracing::info!("hotkeys: F7 toggle, F8 click-through"),
        Err(e) => tracing::warn!("hotkey register failed: {e}"),
    }
    let mut timer = PresentTimer::new(180);
    let mut hw = HwPoller::new(cfg.update_hw_ms);
    let mut visible = true;
    let mut click_through = cfg.click_through;
    let mut msg = MSG::default();
    let mut frames: u64 = 0;
    // Create brushes once outside the loop (cheaper, less flicker).
    let white_brush = overlay
        .rt()
        .and_then(|rt| TextBrush::new(rt, 1.0, 1.0, 1.0, 1.0).ok());
    if white_brush.is_none() {
        tracing::warn!("no render target; overlay will be empty");
    }
    loop {
        // Non-blocking pump: GetMessageW would block and freeze rendering.
        while unsafe { PeekMessageW(&mut msg, None, 0, 0, PM_REMOVE) }.as_bool() {
            match msg.message {
                WM_QUIT => {
                    hotkey::unregister(overlay.hwnd);
                    return Ok(());
                }
                WM_HOTKEY => {
                    let id = msg.wParam.0 as i32;
                    if id == hotkey::HOTKEY_ID_TOGGLE {
                        visible = !visible;
                        tracing::info!("visible={visible} (F7)");
                    }
                    if id == hotkey::HOTKEY_ID_CLICKTHROUGH {
                        click_through = !click_through;
                        overlay.set_click_through(click_through);
                        tracing::info!("click_through={click_through} (F8)");
                    }
                }
                _ => unsafe {
                    let _ = TranslateMessage(&msg);
                    DispatchMessageW(&msg);
                },
            }
        }
        let _ = timer.tick();
        let _ = hw.update();
        frames += 1;
        if frames % 300 == 1 {
            let s = timer.stats();
            tracing::info!("frame {frames}: fps={:.0} avg_ms={:.2}", s.fps, s.avg_ms);
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
                let s = timer.stats();
                let line = format!(
                    "FPS: {:3.0} | ms: {:5.2} | min: {:5.2} | max: {:5.2}",
                    s.fps, s.avg_ms, s.min_ms, s.max_ms
                );
                let r1 = draw_text(rt, fmt, brush, 8.0, 6.0, &line);
                if frames % 300 == 1 {
                    tracing::info!("draw_text result: {r1:?}");
                }
                if cfg.show_frametime_graph {
                    let r2 = draw_graph(rt, brush, timer.samples_ms(), 8.0, 28.0, 380.0, 60.0);
                    if frames % 300 == 1 {
                        tracing::info!("draw_graph result: {r2:?}");
                    }
                }
            }
            let _ = overlay.end_draw();
        }
        std::thread::sleep(std::time::Duration::from_millis(8));
    }
}
