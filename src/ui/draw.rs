use windows::core::Result;
use windows::Win32::Graphics::Direct2D::Common::{D2D1_COLOR_F, D2D_RECT_F};
use windows::Win32::Graphics::Direct2D::{
    ID2D1HwndRenderTarget, ID2D1SolidColorBrush, D2D1_BRUSH_PROPERTIES, D2D1_DRAW_TEXT_OPTIONS_NONE,
};
use windows::Win32::Graphics::DirectWrite::{IDWriteTextFormat, DWRITE_MEASURING_MODE_NATURAL};
use windows_numerics::Vector2;

pub struct TextBrush {
    pub brush: ID2D1SolidColorBrush,
}

impl TextBrush {
    pub fn new(rt: &ID2D1HwndRenderTarget, r: f32, g: f32, b: f32, a: f32) -> Result<Self> {
        let color = D2D1_COLOR_F { r, g, b, a };
        let props = D2D1_BRUSH_PROPERTIES {
            opacity: a,
            transform: Default::default(),
        };
        let brush = unsafe { rt.CreateSolidColorBrush(&color, Some(&props))? };
        Ok(Self { brush })
    }
}

pub fn draw_text(
    rt: &ID2D1HwndRenderTarget,
    fmt: &IDWriteTextFormat,
    brush: &ID2D1SolidColorBrush,
    x: f32,
    y: f32,
    text: &str,
) -> Result<()> {
    let utf16: Vec<u16> = text.encode_utf16().collect();
    let rect = D2D_RECT_F {
        left: x,
        top: y,
        right: x + 800.0,
        bottom: y + 40.0,
    };
    unsafe {
        rt.DrawText(
            &utf16,
            fmt,
            &rect,
            brush,
            D2D1_DRAW_TEXT_OPTIONS_NONE,
            DWRITE_MEASURING_MODE_NATURAL,
        );
    }
    Ok(())
}

/// Frametime graph on a FIXED 0–`max_ms` scale. Autoscaling to the data
/// makes steady frame rates look spiky (any jitter fills the height);
/// a fixed ceiling keeps flat rates flat and reserves the top for real
/// spikes. Values above the ceiling clamp.
pub fn draw_graph(
    rt: &ID2D1HwndRenderTarget,
    brush: &ID2D1SolidColorBrush,
    samples: &[f32],
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    max_ms: f32,
) -> Result<()> {
    if samples.len() < 2 || max_ms <= 0.0 {
        return Ok(());
    }
    let n = samples.len() as f32;
    let step = w / (n.max(1.0) - 1.0);
    let py_of = |v: f32| y + h - (v.clamp(0.0, max_ms) / max_ms) * h;
    let mut px = x;
    let mut py = py_of(samples[0]);
    for (i, &v) in samples.iter().enumerate().skip(1) {
        let nx = x + (i as f32) * step;
        let ny = py_of(v);
        unsafe {
            rt.DrawLine(
                Vector2 { X: px, Y: py },
                Vector2 { X: nx, Y: ny },
                brush,
                1.0,
                None,
            );
        }
        px = nx;
        py = ny;
    }
    Ok(())
}
