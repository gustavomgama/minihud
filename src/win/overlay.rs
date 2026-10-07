use windows::core::{Result, PCWSTR};
use windows::Win32::Foundation::{COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::D2D_SIZE_U;
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, ID2D1Factory1, ID2D1HwndRenderTarget, D2D1_FACTORY_TYPE_SINGLE_THREADED,
    D2D1_HWND_RENDER_TARGET_PROPERTIES, D2D1_PRESENT_OPTIONS_NONE, D2D1_RENDER_TARGET_PROPERTIES,
};
use windows::Win32::Graphics::DirectWrite::{
    DWriteCreateFactory, IDWriteFactory, IDWriteTextFormat, DWRITE_FACTORY_TYPE_SHARED,
    DWRITE_FONT_STRETCH_NORMAL, DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT_NORMAL,
    DWRITE_PARAGRAPH_ALIGNMENT_NEAR, DWRITE_TEXT_ALIGNMENT_LEADING,
};
use windows::Win32::Graphics::Gdi::HBRUSH;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, LoadCursorW, PostQuitMessage, RegisterClassW,
    SetLayeredWindowAttributes, SetWindowLongPtrW, ShowWindow, CS_HREDRAW, CS_VREDRAW, GWL_EXSTYLE,
    IDC_ARROW, LWA_ALPHA, SW_SHOW, WM_DESTROY, WNDCLASSW, WS_EX_LAYERED, WS_EX_TOPMOST,
    WS_EX_TRANSPARENT, WS_POPUP, WS_VISIBLE,
};

pub struct Overlay {
    pub hwnd: HWND,
    pub factory: ID2D1Factory1,
    pub dwrite: IDWriteFactory,
    pub rt: Option<ID2D1HwndRenderTarget>,
    pub text_fmt: Option<IDWriteTextFormat>,
}

fn wstr(s: &str) -> Vec<u16> {
    let mut v: Vec<u16> = s.encode_utf16().collect();
    v.push(0);
    v
}

unsafe extern "system" fn wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    match msg {
        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

impl Overlay {
    pub fn new(title: &str, x: i32, y: i32, w: i32, h: i32) -> Result<Self> {
        let hinstance = unsafe { GetModuleHandleW(None)? };
        let class_name = wstr("minihud_class");
        let wc = WNDCLASSW {
            lpfnWndProc: Some(wnd_proc),
            hInstance: HINSTANCE(hinstance.0),
            lpszClassName: PCWSTR(class_name.as_ptr()),
            hCursor: unsafe { LoadCursorW(None, IDC_ARROW)? },
            hbrBackground: HBRUSH(std::ptr::null_mut()),
            style: CS_HREDRAW | CS_VREDRAW,
            ..Default::default()
        };
        unsafe { RegisterClassW(&wc) };
        let hwnd = unsafe {
            CreateWindowExW(
                WS_EX_TOPMOST | WS_EX_LAYERED | WS_EX_TRANSPARENT,
                PCWSTR(class_name.as_ptr()),
                PCWSTR(wstr(title).as_ptr()),
                WS_POPUP | WS_VISIBLE,
                x,
                y,
                w,
                h,
                None,
                None,
                Some(HINSTANCE(hinstance.0)),
                None,
            )?
        };
        let factory: ID2D1Factory1 =
            unsafe { D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)? };
        let dwrite: IDWriteFactory = unsafe { DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)? };
        let mut ov = Self {
            hwnd,
            factory,
            dwrite,
            rt: None,
            text_fmt: None,
        };
        ov.create_resources(w, h)?;
        unsafe { SetLayeredWindowAttributes(hwnd, COLORREF(0), 240, LWA_ALPHA) };
        Ok(ov)
    }

    fn create_resources(&mut self, w: i32, h: i32) -> Result<()> {
        let rt_props = D2D1_RENDER_TARGET_PROPERTIES::default();
        let hwnd_props = D2D1_HWND_RENDER_TARGET_PROPERTIES {
            hwnd: self.hwnd,
            pixelSize: D2D_SIZE_U {
                width: w as u32,
                height: h as u32,
            },
            presentOptions: D2D1_PRESENT_OPTIONS_NONE,
        };
        let rt: ID2D1HwndRenderTarget = unsafe {
            self.factory
                .CreateHwndRenderTarget(&rt_props, &hwnd_props)?
        };
        let fmt = unsafe {
            self.dwrite.CreateTextFormat(
                PCWSTR(wstr("Consolas").as_ptr()),
                None,
                DWRITE_FONT_WEIGHT_NORMAL,
                DWRITE_FONT_STYLE_NORMAL,
                DWRITE_FONT_STRETCH_NORMAL,
                14.0,
                PCWSTR(wstr("en-us").as_ptr()),
            )?
        };
        unsafe {
            fmt.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_LEADING)?;
            fmt.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_NEAR)?;
        }
        self.rt = Some(rt);
        self.text_fmt = Some(fmt);
        Ok(())
    }

    pub fn set_click_through(&self, enable: bool) {
        unsafe {
            let base = WS_EX_TOPMOST | WS_EX_LAYERED;
            let ex = if enable {
                base | WS_EX_TRANSPARENT
            } else {
                base
            };
            let _ = SetWindowLongPtrW(self.hwnd, GWL_EXSTYLE, ex.0 as isize);
        }
    }

    pub fn show(&self) {
        unsafe {
            ShowWindow(self.hwnd, SW_SHOW);
        }
    }

    pub fn begin_draw(&self) {
        if let Some(rt) = &self.rt {
            unsafe {
                rt.BeginDraw();
            }
        }
    }

    pub fn end_draw(&self) -> Result<()> {
        if let Some(rt) = &self.rt {
            unsafe {
                rt.EndDraw(None, None)?;
            }
        }
        Ok(())
    }

    pub fn rt(&self) -> Option<&ID2D1HwndRenderTarget> {
        self.rt.as_ref()
    }
    pub fn fmt(&self) -> Option<&IDWriteTextFormat> {
        self.text_fmt.as_ref()
    }
}
