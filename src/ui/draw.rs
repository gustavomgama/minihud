use windows::core::Result;
use windows::Win32::Graphics::Direct2D::Common::{D2D1_COLOR_F, D2D1_POINT_2F, D2D1_RECT_F};
use windows::Win32::Graphics::Direct2D::{
    ID2D1HwndRenderTarget, ID2D1SolidColorBrush, D2D1_BRUSH_PROPERTIES, D2D1_DRAW_TEXT_OPTIONS_NONE,
};
use windows::Win32::Graphics::DirectWrite::{IDWriteTextFormat, DWRITE_TEXT_ALIGNMENT_LEADING};

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
    let rect = D2D1_RECT_F {
        left: x,
        top: y,
        right: x + 800.0,
        bottom: y + 40.0,
    };
    unsafe {
        rt.DrawTextW(
            &utf16,
            fmt,
            &rect,
            brush,
            D2D1_DRAW_TEXT_OPTIONS_NONE,
            DWRITE_TEXT_ALIGNMENT_LEADING,
        );
    }
    Ok(())
}

pub fn draw_graph(
    rt: &ID2D1HwndRenderTarget,
    brush: &ID2D1SolidColorBrush,
    samples: &[f32],
    x: f32,
    y: f32,
    w: f32,
    h: f32,
) -> Result<()> {
    if samples.is_empty() {
        return Ok(());
    }
    let n = samples.len() as f32;
    let mut maxv = samples[0];
    for &v in samples {
        if v > maxv {
            maxv = v;
        }
    }
    if maxv < 1.0 {
        maxv = 1.0;
    }
    let step = w / (n.max(1.0) - 1.0);
    let mut px = x;
    let mut py = y + h - (samples[0] / maxv) * h;
    for (i, &v) in samples.iter().enumerate().skip(1) {
        let nx = x + (i as f32) * step;
        let ny = y + h - (v / maxv) * h;
        unsafe {
            rt.DrawLine(
                D2D1_POINT_2F { x: px, y: py },
                D2D1_POINT_2F { x: nx, y: ny },
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
