//! The egui/eframe overlay window.
//!
//! Thin glue: all the logic that can be tested lives in [`super::model`],
//! [`super::theme`], [`super::config`] and [`super::layout`]. This module only
//! paints them.
//!
//! The window is transparent, undecorated, always-on-top and click-through, so
//! it reads as an OSD over a game rather than a window.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use eframe::egui;

use super::config::{Config, Density};
use super::layout;
use super::model::{self, Metric};
use super::motion;
use super::spark;
use super::theme::{self, Rgb};
use crate::hw::HwStats;

/// Seconds a changed number/bar takes to roll to its new value. Short enough to
/// read as a settle, not an animation.
const ROLL_SECS: f32 = 0.28;

/// The live data the overlay draws. Producers update it through the shared
/// handle handed to [`run`].
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub stats: HwStats,
    pub fps: Option<f64>,
    pub one_percent_low: Option<f64>,
    /// Recent fps samples (newest last), for the Full-density sparkline.
    pub fps_history: Vec<f32>,
    /// The followed process name, empty when nothing is followed.
    pub target: String,
}

/// Launch the overlay. Blocks until the window is closed. `quit` is set by an
/// external signal (the quit hotkey) to close the window.
pub fn run(cfg: Config, data: Arc<Mutex<Snapshot>>, quit: Arc<AtomicBool>) -> eframe::Result<()> {
    let size = layout::content_size(cfg.density, cfg.scale);
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_decorations(false)
            .with_transparent(true)
            .with_always_on_top()
            .with_mouse_passthrough(true)
            .with_taskbar(false)
            .with_inner_size(size),
        renderer: eframe::Renderer::Glow,
        ..Default::default()
    };
    eframe::run_native(
        "minihud",
        options,
        Box::new(move |_cc| {
            Ok(Box::new(Overlay {
                cfg,
                data,
                placed: false,
                last_h: -1.0,
                last_sig: None,
                last_change: Instant::now(),
                quit,
            }))
        }),
    )
}

struct Overlay {
    cfg: Config,
    data: Arc<Mutex<Snapshot>>,
    /// Whether the window has been moved to its anchored corner yet (done once,
    /// as soon as the monitor size is known).
    placed: bool,
    /// The content height the window was last fitted to, so it is resized only
    /// when the measured content actually changes.
    last_h: f32,
    /// The signature of the last drawn content, to detect change.
    last_sig: Option<u64>,
    /// When the content last changed (drives the idle fade).
    last_change: Instant,
    /// Set by the quit hotkey; when true the window closes.
    quit: Arc<AtomicBool>,
}

impl eframe::App for Overlay {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        // Transparent, so only the panel's own fill shows through.
        egui::Rgba::TRANSPARENT.to_array()
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let scale = self.cfg.scale;

        if self.quit.load(Ordering::SeqCst) {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
        }

        let snap = self.data.lock().map(|g| g.clone()).unwrap_or_default();
        let rows = model::rows_for(self.cfg.density, &snap.stats);

        // Idle fade: full while values move, dimmed once they settle.
        let sig = motion::signature(&rows, snap.fps, snap.one_percent_low);
        if self.last_sig != Some(sig) {
            self.last_sig = Some(sig);
            self.last_change = Instant::now();
        }
        let idle_for = self.last_change.elapsed().as_secs_f32();
        let base = self.cfg.opacity.clamp(0.0, 1.0);
        let faded = motion::idle_opacity(base, self.cfg.fade_idle_secs, idle_for);
        let factor = if base > 0.0 { faded / base } else { 1.0 };

        let temp = snap.stats.gpu_temp_c.or(snap.stats.cpu_temp_c);
        let accent = self
            .cfg
            .accent_color
            .as_deref()
            .and_then(theme::parse_hex)
            .map(to_color)
            .unwrap_or_else(|| to_color(theme::resolve_accent(self.cfg.accent, temp)));
        let alpha = (faded * 255.0).round() as u8;
        let bg = egui::Color32::from_rgba_unmultiplied(10, 12, 16, alpha);
        let hairline = egui::Stroke::new(
            1.0,
            egui::Color32::from_rgba_unmultiplied(255, 255, 255, 26),
        );

        let panel = egui::Frame::NONE
            .fill(bg)
            .stroke(hairline)
            .inner_margin(egui::Margin::same((14.0 * scale) as i8))
            .corner_radius(egui::CornerRadius::same((12.0 * scale) as u8))
            .show(ui, |ui| {
                ui.set_opacity(factor);
                hero(ui, &snap, accent, scale);
                ui.add_space(10.0 * scale);
                for m in &rows {
                    row(ui, m, scale);
                }
                if self.cfg.density == Density::Full {
                    ui.add_space(6.0 * scale);
                    sparkline(ui, &snap.fps_history, accent, scale);
                }
            });

        // A signature accent rail on the panel's left edge, inset to clear the
        // rounded corners.
        let corner = 12.0 * scale;
        let rail_w = (3.0 * scale).max(2.0);
        let rect = panel.response.rect;
        let rail = egui::Rect::from_min_size(
            rect.left_top() + egui::vec2(0.0, corner),
            egui::vec2(rail_w, (rect.height() - 2.0 * corner).max(rail_w)),
        );
        ui.painter()
            .rect_filled(rail, egui::CornerRadius::same(rail_w as u8), accent);

        // Fit the window to the measured panel: `content_size` is only an
        // initial guess, so a bottom/right anchor sits flush instead of being
        // offset by any leftover height.
        let used = panel.response.rect.size();
        if (used.y - self.last_h).abs() > 0.5 {
            self.last_h = used.y;
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(used));
        }

        // Anchor once, as soon as the work area is known.
        if !self.placed {
            let work = work_area().or_else(|| {
                ctx.input(|i| i.viewport().monitor_size)
                    .map(|m| [0.0, 0.0, m.x, m.y])
            });
            if let Some(work) = work {
                let pos = layout::anchor_pos(
                    self.cfg.anchor,
                    work,
                    [used.x, used.y],
                    self.cfg.margin_px as f32,
                );
                ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::pos2(
                    pos[0], pos[1],
                )));
                tracing::info!(
                    "overlay: anchored at ({:.0}, {:.0}) in work area {:?}",
                    pos[0],
                    pos[1],
                    work
                );
                self.placed = true;
            }
        }

        // Repaint faster while the fade is still animating.
        let animating = factor < 0.999;
        ctx.request_repaint_after(Duration::from_millis(if animating { 50 } else { 250 }));
    }
}

/// The single heroic number: FPS, with its unit and 1% low. The number rolls to
/// its new value instead of jumping.
fn hero(ui: &mut egui::Ui, snap: &Snapshot, accent: egui::Color32, scale: f32) {
    let text = match snap.fps {
        Some(target) => {
            let shown = ui.ctx().animate_value_with_time(
                egui::Id::new("roll.fps"),
                target as f32,
                ROLL_SECS,
            );
            format!("{shown:.0}")
        }
        None => "--".to_string(),
    };
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new(text)
                .size(40.0 * scale)
                .strong()
                .monospace()
                .color(accent),
        );
        ui.vertical(|ui| {
            ui.add_space(4.0 * scale);
            ui.label(egui::RichText::new("FPS").size(12.0 * scale).weak());
            if let Some(low) = snap.one_percent_low {
                let shown = ui.ctx().animate_value_with_time(
                    egui::Id::new("roll.low"),
                    low as f32,
                    ROLL_SECS,
                );
                ui.label(
                    egui::RichText::new(format!("1% low {shown:.0}"))
                        .size(10.0 * scale)
                        .weak(),
                );
            }
        });
        // The followed process, right-aligned — so a set-and-forget user can
        // confirm the overlay is tracking the right game.
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            if !snap.target.is_empty() {
                ui.label(egui::RichText::new(&snap.target).size(10.0 * scale).weak());
            }
        });
    });
}

/// One labeled metric, with a load bar when the metric is a proportion.
fn row(ui: &mut egui::Ui, m: &Metric, scale: f32) {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(m.label).size(11.0 * scale).weak());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let mut value = egui::RichText::new(&m.value).size(13.0 * scale).monospace();
            if let Some(tint) = m.tint {
                value = value.color(to_color(tint));
            }
            ui.label(value);
        });
    });
    if let Some(fraction) = m.fraction {
        bar(ui, m.label, fraction, scale);
    }
}

/// A thin load bar: a dim track with a toned fill. The fill rolls to its new
/// width (and tone) instead of snapping.
fn bar(ui: &mut egui::Ui, key: &str, fraction: f32, scale: f32) {
    let target = fraction.clamp(0.0, 1.0);
    let shown =
        ui.ctx()
            .animate_value_with_time(egui::Id::new(("roll.bar", key)), target, ROLL_SECS);
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(width, (4.0 * scale).max(2.0)),
        egui::Sense::hover(),
    );
    let painter = ui.painter();
    let radius = egui::CornerRadius::same((2.0 * scale) as u8);
    painter.rect_filled(rect, radius, egui::Color32::from_gray(55));
    let mut fill = rect;
    fill.set_width(rect.width() * shown);
    painter.rect_filled(fill, radius, to_color(theme::load_color(shown)));
}

/// A compact fps history line (Full density). Nothing is drawn until there are
/// at least two samples.
fn sparkline(ui: &mut egui::Ui, history: &[f32], accent: egui::Color32, scale: f32) {
    if history.len() < 2 {
        return;
    }
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(ui.available_width(), 22.0 * scale),
        egui::Sense::hover(),
    );
    let max = history.iter().copied().fold(0.0_f32, f32::max).max(1.0);
    let norm = spark::normalize(history, max);
    let n = norm.len();
    let points: Vec<egui::Pos2> = norm
        .iter()
        .enumerate()
        .map(|(i, &v)| {
            let x = rect.left() + rect.width() * (i as f32 / (n - 1) as f32);
            let y = rect.bottom() - rect.height() * v;
            egui::pos2(x, y)
        })
        .collect();
    let painter = ui.painter();
    painter.add(egui::Shape::line(
        points,
        egui::Stroke::new((1.5 * scale).max(1.0), accent),
    ));
}

/// The primary monitor's work area `[x, y, w, h]` in points, with the taskbar
/// excluded, so a bottom/right anchor clears it. `None` if the query fails.
fn work_area() -> Option<[f32; 4]> {
    use windows::Win32::Foundation::RECT;
    use windows::Win32::UI::WindowsAndMessaging::{
        SystemParametersInfoW, SPI_GETWORKAREA, SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS,
    };
    let mut rect = RECT::default();
    // SAFETY: SPI_GETWORKAREA writes a RECT into the pointer we pass; `rect` is
    // a valid, writable RECT for the duration of the call.
    unsafe {
        SystemParametersInfoW(
            SPI_GETWORKAREA,
            0,
            Some(&mut rect as *mut RECT as *mut core::ffi::c_void),
            SYSTEM_PARAMETERS_INFO_UPDATE_FLAGS(0),
        )
    }
    .ok()?;
    Some([
        rect.left as f32,
        rect.top as f32,
        (rect.right - rect.left) as f32,
        (rect.bottom - rect.top) as f32,
    ])
}

fn to_color(c: Rgb) -> egui::Color32 {
    egui::Color32::from_rgb(c[0], c[1], c[2])
}
