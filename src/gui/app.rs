//! The egui/eframe overlay window.
//!
//! Thin glue: all the logic that can be tested lives in [`super::model`],
//! [`super::theme`], [`super::config`] and [`super::layout`]. This module only
//! paints them.
//!
//! The window is transparent, undecorated, always-on-top and click-through, so
//! it reads as an OSD over a game rather than a window.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use eframe::egui;

use super::config::Config;
use super::layout;
use super::model::{self, Metric};
use super::theme::{self, Rgb};
use crate::hw::HwStats;

/// The live data the overlay draws. Producers update it through the shared
/// handle handed to [`run`].
#[derive(Clone, Debug, Default)]
pub struct Snapshot {
    pub stats: HwStats,
    pub fps: Option<f64>,
    pub one_percent_low: Option<f64>,
}

/// Launch the overlay. Blocks until the window is closed.
pub fn run(cfg: Config, data: Arc<Mutex<Snapshot>>) -> eframe::Result<()> {
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
}

impl eframe::App for Overlay {
    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        // Transparent, so only the panel's own fill shows through.
        egui::Rgba::TRANSPARENT.to_array()
    }

    fn ui(&mut self, ui: &mut egui::Ui, _frame: &mut eframe::Frame) {
        let ctx = ui.ctx().clone();
        let scale = self.cfg.scale;

        let snap = self.data.lock().map(|g| g.clone()).unwrap_or_default();
        let temp = snap.stats.gpu_temp_c.or(snap.stats.cpu_temp_c);
        let accent = to_color(theme::resolve_accent(self.cfg.accent, temp));
        let alpha = (self.cfg.opacity.clamp(0.0, 1.0) * 255.0).round() as u8;
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
                hero(ui, &snap, accent, scale);
                ui.add_space(10.0 * scale);
                for m in model::rows_for(self.cfg.density, &snap.stats) {
                    row(ui, &m, scale);
                }
            });

        // Fit the window to the measured panel: `content_size` is only an
        // initial guess, so a bottom/right anchor sits flush instead of being
        // offset by any leftover height.
        let used = panel.response.rect.size();
        if (used.y - self.last_h).abs() > 0.5 {
            self.last_h = used.y;
            ctx.send_viewport_cmd(egui::ViewportCommand::InnerSize(used));
        }

        // Anchor once, as soon as the monitor size is known.
        if !self.placed {
            if let Some(mon) = ctx.input(|i| i.viewport().monitor_size) {
                let pos = layout::anchor_pos(
                    self.cfg.anchor,
                    [mon.x, mon.y],
                    [used.x, used.y],
                    self.cfg.margin_px as f32,
                );
                ctx.send_viewport_cmd(egui::ViewportCommand::OuterPosition(egui::pos2(
                    pos[0], pos[1],
                )));
                self.placed = true;
            }
        }

        ctx.request_repaint_after(Duration::from_millis(250));
    }
}

/// The single heroic number: FPS, with its unit and 1% low.
fn hero(ui: &mut egui::Ui, snap: &Snapshot, accent: egui::Color32, scale: f32) {
    let fps = snap
        .fps
        .map(|f| format!("{f:.0}"))
        .unwrap_or_else(|| "--".to_string());
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new(fps)
                .size(40.0 * scale)
                .strong()
                .monospace()
                .color(accent),
        );
        ui.label(egui::RichText::new("FPS").size(12.0 * scale).weak());
        if let Some(low) = snap.one_percent_low {
            ui.label(
                egui::RichText::new(format!("1% {low:.0}"))
                    .size(11.0 * scale)
                    .weak(),
            );
        }
    });
}

/// One labeled metric, with a load bar when the metric is a proportion.
fn row(ui: &mut egui::Ui, m: &Metric, scale: f32) {
    ui.horizontal(|ui| {
        ui.label(egui::RichText::new(m.label).size(11.0 * scale).weak());
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(egui::RichText::new(&m.value).size(13.0 * scale).monospace());
        });
    });
    if let Some(fraction) = m.fraction {
        bar(ui, fraction, scale);
    }
}

/// A thin load bar: a dim track with a toned fill.
fn bar(ui: &mut egui::Ui, fraction: f32, scale: f32) {
    let fraction = fraction.clamp(0.0, 1.0);
    let width = ui.available_width();
    let (rect, _) = ui.allocate_exact_size(
        egui::vec2(width, (4.0 * scale).max(2.0)),
        egui::Sense::hover(),
    );
    let painter = ui.painter();
    let radius = egui::CornerRadius::same((2.0 * scale) as u8);
    painter.rect_filled(rect, radius, egui::Color32::from_gray(55));
    let mut fill = rect;
    fill.set_width(rect.width() * fraction);
    painter.rect_filled(fill, radius, to_color(theme::load_color(fraction)));
}

fn to_color(c: Rgb) -> egui::Color32 {
    egui::Color32::from_rgb(c[0], c[1], c[2])
}
