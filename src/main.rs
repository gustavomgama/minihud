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
use windows::Win32::UI::WindowsAndMessaging::{GetMessageW, MSG, WM_HOTKEY, WM_QUIT};

fn main() -> Result<()> {
    tracing_subscriber::fmt().with_env_filter("info").init();
    let cfg = Config::load();
    let overlay = Overlay::new("minihud", cfg.x, cfg.y, 420, 120)?;
    overlay.set_click_through(cfg.click_through);
    hotkey::register(overlay.hwnd)?;
    let mut timer = PresentTimer::new(180);
    let mut hw = HwPoller::new(cfg.update_hw_ms);
    let mut visible = true;
    let mut click_through = cfg.click_through;
    let mut msg = MSG::default();
    loop {
        while unsafe { GetMessageW(&mut msg, None, 0, 0) }.as_bool() {
            match msg.message {
                WM_QUIT => {
                    hotkey::unregister(overlay.hwnd);
                    return Ok(());
                }
                WM_HOTKEY => {
                    let id = msg.wParam.0 as i32;
                    if id == hotkey::HOTKEY_ID_TOGGLE {
                        visible = !visible;
                    }
                    if id == hotkey::HOTKEY_ID_CLICKTHROUGH {
                        click_through = !click_through;
                        overlay.set_click_through(click_through);
                    }
                }
                _ => {}
            }
        }
        let _ = timer.tick();
        let _ = hw.update();
        if visible {
            overlay.begin_draw();
            if let (Some(rt), Some(fmt)) = (overlay.rt(), overlay.fmt()) {
                if let Ok(brush) = TextBrush::new(rt, 1.0, 1.0, 1.0, 0.9) {
                    let s = timer.stats();
                    let line = format!(
                        "FPS: {:3.0} | ms: {:5.2} | min: {:5.2} | max: {:5.2}",
                        s.fps, s.avg_ms, s.min_ms, s.max_ms
                    );
                    let _ = draw_text(rt, fmt, &brush.brush, 8.0, 6.0, &line);
                    if cfg.show_frametime_graph {
                        let _ = draw_graph(
                            rt,
                            &brush.brush,
                            timer.samples_ms(),
                            8.0,
                            28.0,
                            380.0,
                            60.0,
                        );
                    }
                }
            }
            let _ = overlay.end_draw();
        }
        std::thread::sleep(std::time::Duration::from_millis(8));
    }
}
