use windows::core::{Result, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM};
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
use windows::Win32::UI::Input::KeyboardAndMouse::ReleaseCapture;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, GetWindowLongPtrW, GetWindowRect, LoadCursorW,
    PostQuitMessage, RegisterClassW, SendMessageW, SetWindowLongPtrW, SetWindowPos, ShowWindow,
    CS_HREDRAW, CS_VREDRAW, GWL_EXSTYLE, HTCAPTION, HWND_TOPMOST, IDC_ARROW, SWP_FRAMECHANGED,
    SWP_NOACTIVATE, SWP_NOMOVE, SWP_NOSIZE, SWP_NOZORDER, SW_HIDE, SW_SHOW, WM_DESTROY,
    WM_EXITSIZEMOVE, WM_LBUTTONDOWN, WM_NCLBUTTONDOWN, WNDCLASSW, WS_EX_TOOLWINDOW, WS_EX_TOPMOST,
    WS_EX_TRANSPARENT, WS_POPUP, WS_VISIBLE,
};

pub struct Overlay {
    pub hwnd: HWND,
    pub factory: ID2D1Factory1,
    pub dwrite: IDWriteFactory,
    pub rt: Option<ID2D1HwndRenderTarget>,
    pub text_fmt: Option<IDWriteTextFormat>,
    interactive: std::sync::atomic::AtomicBool,
    // Remembered for device-loss recovery (driver update/TDR): the
    // render target + all brushes/fonts die with the device and must be
    // recreated at the same size.
    w: i32,
    h: i32,
    font_size: f32,
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
        // Drag anywhere with the left button. Only reachable when
        // click-through is OFF (F8); with it on, the OS never delivers
        // mouse messages to us in the first place.
        WM_LBUTTONDOWN => {
            let _ = ReleaseCapture();
            SendMessageW(
                hwnd,
                WM_NCLBUTTONDOWN,
                Some(WPARAM(HTCAPTION as usize)),
                None,
            )
        }
        // Drag finished: persist the new position next to the exe.
        WM_EXITSIZEMOVE => {
            persist_position(hwnd);
            LRESULT(0)
        }
        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

/// Read the window rect and store x/y in the on-disk config.
fn persist_position(hwnd: HWND) {
    unsafe {
        let mut r = RECT::default();
        if GetWindowRect(hwnd, &mut r).is_ok() {
            let mut cfg = crate::config::Config::load();
            cfg.x = r.left;
            cfg.y = r.top;
            cfg.save();
        }
    }
}

impl Overlay {
    pub fn new(title: &str, x: i32, y: i32, w: i32, h: i32, font_size: f32) -> Result<Self> {
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
                // TOOLWINDOW: hides the overlay from Alt+Tab and the
                // taskbar. No visual effect on a borderless WS_POPUP.
                WS_EX_TOPMOST | WS_EX_TRANSPARENT | WS_EX_TOOLWINDOW,
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
            interactive: std::sync::atomic::AtomicBool::new(false),
            w,
            h,
            font_size,
        };
        ov.create_resources(w, h, font_size)?;
        unsafe {
            let _ = ShowWindow(hwnd, SW_SHOW);
            let _ = SetWindowPos(
                hwnd,
                Some(HWND_TOPMOST),
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE,
            );
        }
        Ok(ov)
    }

    fn create_resources(&mut self, w: i32, h: i32, font_size: f32) -> Result<()> {
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
                font_size.clamp(10.0, 28.0),
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
        use std::sync::atomic::Ordering;
        self.interactive.store(!enable, Ordering::Relaxed);
        unsafe {
            let ex = GetWindowLongPtrW(self.hwnd, GWL_EXSTYLE) as u32;
            let transparent = WS_EX_TRANSPARENT.0 as u32;
            let new_ex = if enable {
                ex | transparent
            } else {
                ex & !transparent
            };
            let _ = SetWindowLongPtrW(self.hwnd, GWL_EXSTYLE, new_ex as isize);
            // Style changes need FRAMECHANGED to apply immediately.
            let _ = SetWindowPos(
                self.hwnd,
                None,
                0,
                0,
                0,
                0,
                SWP_NOMOVE | SWP_NOSIZE | SWP_NOZORDER | SWP_NOACTIVATE | SWP_FRAMECHANGED,
            );
            let applied = GetWindowLongPtrW(self.hwnd, GWL_EXSTYLE) as u32;
            tracing::info!(
                "click-through {}: exstyle {:#x} -> {:#x} (now {:#x})",
                if enable { "on" } else { "off" },
                ex,
                new_ex,
                applied,
            );
        }
    }

    pub fn set_visible(&self, visible: bool) {
        unsafe {
            let _ = ShowWindow(self.hwnd, if visible { SW_SHOW } else { SW_HIDE });
            if visible {
                let _ = SetWindowPos(
                    self.hwnd,
                    Some(HWND_TOPMOST),
                    0,
                    0,
                    0,
                    0,
                    SWP_NOMOVE | SWP_NOSIZE | SWP_NOACTIVATE,
                );
            }
        }
    }

    pub fn begin_draw(&self) {
        use std::sync::atomic::Ordering;
        // Visible feedback: lighter background while interactive (F8 off),
        // so the F8 state is observable before drag support lands.
        let bg = if self.interactive.load(Ordering::Relaxed) {
            0.10
        } else {
            0.02
        };
        if let Some(rt) = &self.rt {
            unsafe {
                rt.BeginDraw();
                rt.Clear(Some(
                    &windows::Win32::Graphics::Direct2D::Common::D2D1_COLOR_F {
                        r: bg,
                        g: bg,
                        b: bg,
                        a: 1.0,
                    },
                ));
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

    /// Drop all device-dependent resources after EndDraw reports the
    /// device lost (driver update, TDR, GPU switch). The window keeps
    /// its last frame until `ensure_target` rebuilds.
    pub fn invalidate(&mut self) {
        self.rt = None;
        self.text_fmt = None;
    }

    /// Rebuild the render target if missing. Returns the underlying
    /// error when the device is still gone; callers retry on a timer,
    /// not every frame, to avoid log spam.
    pub fn ensure_target(&mut self) -> Result<()> {
        if self.rt.is_none() {
            self.create_resources(self.w, self.h, self.font_size)?;
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
