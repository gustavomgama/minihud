//! hook-test: ground-truth presenter for hook validation.
//!
//! Creates a hidden D3D11 window, presents exactly N frames with vsync,
//! prints `presented=N`. The spike gate: hook-host's reported count must
//! match within 1% (loss counter explains any gap).

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HINSTANCE, HMODULE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_DRIVER_TYPE_WARP, D3D_FEATURE_LEVEL_11_0};
use windows::Win32::Graphics::Direct3D11::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::*;

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
    DefWindowProcW(hwnd, msg, wparam, lparam)
}

fn main() {
    let n: u32 = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(600);
    unsafe {
        if let Err(e) = run(n) {
            eprintln!("hook-test failed: {e:?}");
            std::process::exit(1);
        }
    }
}

unsafe fn run(n: u32) -> windows::core::Result<()> {
    let hinstance = GetModuleHandleW(None)?;
    let class = wstr("hook-test");
    let wc = WNDCLASSW {
        lpfnWndProc: Some(wnd_proc),
        hInstance: hinstance.into(),
        lpszClassName: PCWSTR(class.as_ptr()),
        ..Default::default()
    };
    RegisterClassW(&wc);
    let hwnd = CreateWindowExW(
        Default::default(),
        PCWSTR(class.as_ptr()),
        PCWSTR(wstr("hook-test").as_ptr()),
        WS_POPUP,
        0,
        0,
        64,
        64,
        None,
        None,
        Some(HINSTANCE(hinstance.0)),
        None,
    )?;
    let levels = [D3D_FEATURE_LEVEL_11_0];
    let desc = DXGI_SWAP_CHAIN_DESC {
        BufferDesc: DXGI_MODE_DESC {
            Width: 64,
            Height: 64,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            ..Default::default()
        },
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
        BufferCount: 2,
        OutputWindow: hwnd,
        Windowed: true.into(),
        SwapEffect: DXGI_SWAP_EFFECT_DISCARD,
        Flags: 0,
        ..Default::default()
    };
    let mut swapchain: Option<IDXGISwapChain> = None;
    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;
    if D3D11CreateDeviceAndSwapChain(
        None,
        D3D_DRIVER_TYPE_HARDWARE,
        HMODULE(std::ptr::null_mut()),
        D3D11_CREATE_DEVICE_FLAG(0),
        Some(&levels),
        D3D11_SDK_VERSION,
        Some(&desc),
        Some(&mut swapchain as *mut _),
        Some(&mut device as *mut _),
        None,
        Some(&mut context as *mut _),
    )
    .is_err()
    {
        D3D11CreateDeviceAndSwapChain(
            None,
            D3D_DRIVER_TYPE_WARP,
            HMODULE(std::ptr::null_mut()),
            D3D11_CREATE_DEVICE_FLAG(0),
            Some(&levels),
            D3D11_SDK_VERSION,
            Some(&desc),
            Some(&mut swapchain as *mut _),
            Some(&mut device as *mut _),
            None,
            Some(&mut context as *mut _),
        )?;
    }
    eprintln!("stage: device+swapchain ok");
    let sc = swapchain.unwrap();
    eprintln!("stage: swapchain ok");
    for _ in 0..n {
        sc.Present(1, DXGI_PRESENT(0)).ok()?;
    }
    println!("presented={n}");
    Ok(())
}
