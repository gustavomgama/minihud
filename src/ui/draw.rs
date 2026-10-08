use windows::core::Result;
use windows::Win32::Graphics::Direct2D::Common::{D2D1_COLOR_F, D2D_RECT_F};
use windows::Win32::Graphics::Direct2D::{
    ID2D1HwndRenderTarget, ID2D1SolidColorBrush, D2D1_BRUSH_PROPERTIES, D2D1_DRAW_TEXT_OPTIONS_NONE,
};
use windows::Win32::Graphics::DirectWrite::{IDWriteTextFormat, DWRITE_MEASURING_MODE_NATURAL};

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
