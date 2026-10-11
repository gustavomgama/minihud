//! `hook-test` — a tiny graphics presenter used to validate the hooks.
//!
//! It prints its `pid` and then presents `--frames` frames through one of
//! D3D11 (default), D3D9, or OpenGL, so an injected recorder can be observed
//! against a real present path. It is a test target, not an OSD: it draws
//! nothing but the swap.

use core::ffi::c_void;
use std::path::PathBuf;
use std::time::Duration;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{GetDC, ReleaseDC};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, RegisterClassW, ShowWindow, CW_USEDEFAULT,
    SW_SHOW, WNDCLASSW, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
};

const CLASS_NAME: PCWSTR = w!("MinihudHookTest");

/// RTSS opt-out marker: RTSS (RivaTuner Statistics Server) skips hooking a
/// process that exports this symbol. RTSS's present hook conflicts with a
/// custom present-wrapping Vulkan layer, so the *validation target* opts out to
/// isolate the layer test. See `build.rs` (the `/EXPORT` linker arg).
#[no_mangle]
pub static RTSSHooksCompatibility: u32 = 0;

/// Default frame count: large enough that an injected recorder has time to
/// attach and observe presents.
const DEFAULT_FRAMES: u32 = 2000;

// The Vulkan loader entry point, imported **statically** from `vulkan-1.dll`.
//
// A natively-linked game imports `vkGetInstanceProcAddr` in its own import
// table and resolves every other Vulkan entry point by name through it. The
// recorder IAT-hooks that import, so every lookup the app makes (including
// `vkQueuePresentKHR`) lands on the recorder. `raw-dylib` emits the import
// without needing a `vulkan-1.lib`.
#[link(name = "vulkan-1", kind = "raw-dylib")]
extern "system" {
    fn vkGetInstanceProcAddr(
        instance: ash::vk::Instance,
        p_name: *const core::ffi::c_char,
    ) -> ash::vk::PFN_vkVoidFunction;
}

// `opengl32!wglSwapBuffers` — the WGL present export. The `windows` crate binds
// `SwapBuffers` (gdi32) and `wglSwapLayerBuffers` (opengl32) but not
// `wglSwapBuffers`, so it is declared here as a `raw-dylib` import (the same
// mechanism every `windows` binding uses). `build.rs` adds
// `/DELAYLOAD:opengl32.dll`, so this import lands in the delay-load directory
// (data directory 13) and its IAT slot is the one `__delayLoadHelper2`
// overwrites on the first call — the self-heal arm this presenter validates.
#[link(name = "opengl32.dll", kind = "raw-dylib", modifiers = "+verbatim")]
extern "system" {
    fn wglSwapBuffers(hdc: windows::Win32::Graphics::Gdi::HDC) -> i32;
}

// ANGLE (`libEGL.dll` + `libGLESv2.dll`) — Steam's CEF ships a usable pair. The
// `eglSwapBuffers` export is declared `raw-dylib` and **delay-loaded**
// (`build.rs` adds `/DELAYLOAD:libEGL.dll`), so `hook-test` still starts on a
// machine without ANGLE; selecting `--api angle` then fails cleanly instead of
// failing to load the whole target. The recorder IAT-hooks the delay-load slot
// for `libEGL.dll!eglSwapBuffers`, so presenting through it exercises the
// `egl_swap_buffers` detour.
#[link(name = "libEGL.dll", kind = "raw-dylib", modifiers = "+verbatim")]
extern "system" {
    fn eglGetDisplay(native_display: *mut c_void) -> *mut c_void;
    fn eglInitialize(dpy: *mut c_void, major: *mut i32, minor: *mut i32) -> u32;
    fn eglChooseConfig(
        dpy: *mut c_void,
        attribs: *const i32,
        configs: *mut *mut c_void,
        config_size: i32,
        num_config: *mut i32,
    ) -> u32;
    fn eglCreateWindowSurface(
        dpy: *mut c_void,
        config: *mut c_void,
        win: *mut c_void,
        attribs: *const i32,
    ) -> *mut c_void;
    fn eglCreateContext(
        dpy: *mut c_void,
        config: *mut c_void,
        share: *mut c_void,
        attribs: *const i32,
    ) -> *mut c_void;
    fn eglMakeCurrent(
        dpy: *mut c_void,
        draw: *mut c_void,
        read: *mut c_void,
        ctx: *mut c_void,
    ) -> u32;
    fn eglSwapBuffers(dpy: *mut c_void, surface: *mut c_void) -> u32;
    fn eglGetError() -> i32;
}

/// `WGL_SWAP_MAIN_PLANE` — the plane bit `wglSwapLayerBuffers` swaps the main
/// plane for. The `windows` crate does not define it.
const WGL_SWAP_MAIN_PLANE: u32 = 0x0000_0001;

// EGL constants (egl.h) — only the subset this presenter uses.
const EGL_DEFAULT_DISPLAY: *mut c_void = std::ptr::null_mut();
const EGL_NO_CONTEXT: *mut c_void = std::ptr::null_mut();
const EGL_NONE: i32 = 0x3038;
const EGL_SURFACE_TYPE: i32 = 0x3033;
const EGL_WINDOW_BIT: i32 = 0x0004;
const EGL_RENDERABLE_TYPE: i32 = 0x3040;
const EGL_OPENGL_ES2_BIT: i32 = 0x0004;
const EGL_RED_SIZE: i32 = 0x3024;
const EGL_GREEN_SIZE: i32 = 0x3025;
const EGL_BLUE_SIZE: i32 = 0x3026;
const EGL_CONTEXT_CLIENT_VERSION: i32 = 0x3098;

/// Parsed command line: which presenter to run and how many frames to present.
struct Config {
    api: String,
    frames: u32,
    /// Resolve Vulkan through `GetProcAddress` (`ash::Entry::load`) instead of a
    /// static import of the loader's proc-addr. This is the fully-dynamic case
    /// the in-process IAT hook cannot see and the implicit layer must capture.
    dynamic: bool,
    /// Enter exclusive fullscreen (`SetFullscreenState(true)`) so the recorder is
    /// tested against a target that **owns the display** (the case where a dummy
    /// device/swapchain bootstrap is refused). Supported by `--api d3d11`.
    fullscreen: bool,
}

/// Parse `--api <name>` and `--frames <n>`; a missing or unparsable option
/// falls back to the defaults. `--dynamic` (or `--api vulkan-dynamic`) selects
/// the dynamic loader path.
fn parse_cli(args: &[String]) -> Config {
    let dynamic = args.iter().any(|a| a == "--dynamic");
    let mut api = flag(args, "--api").unwrap_or_else(|| "d3d11".to_string());
    let mut dynamic = dynamic;
    if api == "vulkan-dynamic" {
        api = "vulkan".to_string();
        dynamic = true;
    }
    let frames = flag(args, "--frames")
        .and_then(|v| v.parse().ok())
        .unwrap_or(DEFAULT_FRAMES);
    let fullscreen = args.iter().any(|a| a == "--fullscreen");
    Config {
        api,
        frames,
        dynamic,
        fullscreen,
    }
}

/// The presenter selected by `--api`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Presenter {
    D3d11,
    D3d9,
    /// D3D9 with an `IDirect3DDevice9Ex` device (`PresentEx`/`EndScene`/`ResetEx`).
    D3d9Ex,
    D3d12,
    Vulkan,
    OpenGl,
    /// OpenGL that calls the **delay-loaded** `opengl32!wglSwapBuffers` export
    /// every frame (vs `OpenGl`, which calls `gdi32!SwapBuffers`).
    OpenGlDelay,
    /// D3D11 device + a swapchain created through the **base**
    /// `IDXGIFactory::CreateSwapChain` (slot 10), plus static
    /// `CreateDXGIFactory`/`CreateDXGIFactory1` imports — the arms the other
    /// presenters never reach.
    D3d11Factory,
    /// OpenGL that presents through `opengl32!wglSwapLayerBuffers` (vs `OpenGl`,
    /// which calls `gdi32!SwapBuffers`).
    OpenGlLayer,
    /// ANGLE (`libEGL`): presents via `eglSwapBuffers`.
    Angle,
    /// DirectComposition: a **composition** swapchain
    /// (`IDXGIFactory2::CreateSwapChainForComposition`) rendered through a
    /// `DirectComposition` visual/target tree on an HWND. Reaches the
    /// `create_swap_chain_for_composition` detour, which no other presenter does.
    Dcomp,
}

/// Map an `--api` name to its presenter, or `None` for an unknown name. Pure,
/// so the CLI's supported set is pinned by a test.
fn presenter_for(api: &str) -> Option<Presenter> {
    Some(match api {
        "d3d11" => Presenter::D3d11,
        "d3d11-factory" => Presenter::D3d11Factory,
        "d3d9" => Presenter::D3d9,
        "d3d9ex" => Presenter::D3d9Ex,
        "d3d12" => Presenter::D3d12,
        "vulkan" | "vk" => Presenter::Vulkan,
        "opengl" | "gl" => Presenter::OpenGl,
        "opengl-delay" => Presenter::OpenGlDelay,
        "opengl-layer" => Presenter::OpenGlLayer,
        "angle" | "egl" => Presenter::Angle,
        "dcomp" | "directcomposition" => Presenter::Dcomp,
        _ => return None,
    })
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let Config {
        api,
        frames,
        dynamic,
        fullscreen,
    } = parse_cli(&args);

    println!(
        "pid={} api={api} frames={frames} dynamic={dynamic}",
        std::process::id()
    );

    let Some(presenter) = presenter_for(&api) else {
        eprintln!(
            "hook-test: unknown --api {api:?} (d3d11|d3d11-factory|d3d9|d3d9ex|d3d12|vulkan|vulkan-dynamic|opengl|opengl-delay|opengl-layer|angle|egl|dcomp)"
        );
        std::process::exit(1);
    };
    let result = match presenter {
        Presenter::D3d11 => run_d3d11(frames, fullscreen),
        Presenter::D3d11Factory => run_d3d11_factory(frames),
        Presenter::D3d9 => run_d3d9(frames, false),
        Presenter::D3d9Ex => run_d3d9(frames, true),
        Presenter::D3d12 => run_d3d12(frames),
        Presenter::Vulkan => run_vulkan(frames, dynamic),
        Presenter::OpenGl => run_opengl(frames),
        Presenter::OpenGlDelay => run_opengl_delay(frames),
        Presenter::OpenGlLayer => run_opengl_layer(frames),
        Presenter::Angle => run_angle(frames),
        Presenter::Dcomp => run_dcomp(frames),
    };
    if let Err(e) = result {
        eprintln!("hook-test: {e}");
        std::process::exit(1);
    }
    println!("hook-test: presented {frames} frames via {api}");
}

/// `--flag value` lookup.
fn flag(args: &[String], name: &str) -> Option<String> {
    args.iter()
        .position(|a| a == name)
        .and_then(|i| args.get(i + 1))
        .cloned()
}

/// One Vulkan queue family, reduced to the two capabilities we need.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
struct QueueFamily {
    graphics: bool,
    present: bool,
}

/// Index of the first queue family that can both render and present.
fn pick_queue_family(families: &[QueueFamily]) -> Option<u32> {
    families
        .iter()
        .position(|f| f.graphics && f.present)
        .map(|i| i as u32)
}

/// Default window procedure: delegate to `DefWindowProcW`.
unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, w: WPARAM, l: LPARAM) -> LRESULT {
    // SAFETY: forwarding the window's own message unchanged.
    unsafe { DefWindowProcW(hwnd, msg, w, l) }
}

/// Register a class and create a visible window; returns its handle.
fn create_window() -> Result<HWND, String> {
    // SAFETY: `GetModuleHandleW(None)` returns this module's HINSTANCE.
    let instance = unsafe { GetModuleHandleW(None) }.map_err(|e| e.to_string())?;
    let class = WNDCLASSW {
        lpfnWndProc: Some(wndproc),
        hInstance: HINSTANCE(instance.0),
        lpszClassName: CLASS_NAME,
        ..Default::default()
    };
    // SAFETY: `class` is a fully initialized WNDCLASSW.
    let atom = unsafe { RegisterClassW(&class) };
    if atom == 0 {
        return Err("RegisterClassW failed".into());
    }
    // SAFETY: the class is registered; the parameters describe a normal window.
    let hwnd = unsafe {
        CreateWindowExW(
            Default::default(),
            CLASS_NAME,
            w!("minihud hook-test"),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            320,
            240,
            None,
            None,
            Some(HINSTANCE(instance.0)),
            None,
        )
    }
    .map_err(|e| format!("CreateWindowExW: {e}"))?;
    // SAFETY: `hwnd` is a live window.
    unsafe {
        let _ = ShowWindow(hwnd, SW_SHOW);
    }
    Ok(hwnd)
}

/// Sleep between presents so the recorder sees a realistic cadence.
fn pace() {
    std::thread::sleep(Duration::from_millis(8));
}

fn destroy(hwnd: HWND) {
    // SAFETY: `hwnd` was created by `create_window` and is destroyed once.
    unsafe {
        let _ = DestroyWindow(hwnd);
    }
}

/// Present `frames` frames through a D3D11 swapchain.
fn run_d3d11(frames: u32, fullscreen: bool) -> Result<(), String> {
    use windows::Win32::Foundation::{HMODULE, TRUE};
    use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
    use windows::Win32::Graphics::Direct3D11::{
        D3D11CreateDeviceAndSwapChain, ID3D11Device, ID3D11DeviceContext, D3D11_CREATE_DEVICE_FLAG,
    };
    use windows::Win32::Graphics::Dxgi::Common::{
        DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_FORMAT_UNKNOWN, DXGI_MODE_DESC,
        DXGI_MODE_SCALING_UNSPECIFIED, DXGI_MODE_SCANLINE_ORDER_UNSPECIFIED, DXGI_RATIONAL,
        DXGI_SAMPLE_DESC,
    };
    use windows::Win32::Graphics::Dxgi::{
        IDXGIAdapter, IDXGIOutput, IDXGISwapChain, DXGI_PRESENT, DXGI_SWAP_CHAIN_DESC,
        DXGI_SWAP_CHAIN_FLAG, DXGI_SWAP_EFFECT_DISCARD, DXGI_USAGE_RENDER_TARGET_OUTPUT,
    };

    let hwnd = create_window()?;
    let desc = DXGI_SWAP_CHAIN_DESC {
        BufferDesc: DXGI_MODE_DESC {
            Width: 320,
            Height: 240,
            RefreshRate: DXGI_RATIONAL {
                Numerator: 0,
                Denominator: 0,
            },
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            ScanlineOrdering: DXGI_MODE_SCANLINE_ORDER_UNSPECIFIED,
            Scaling: DXGI_MODE_SCALING_UNSPECIFIED,
        },
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
        BufferCount: 2,
        OutputWindow: hwnd,
        Windowed: TRUE,
        SwapEffect: DXGI_SWAP_EFFECT_DISCARD,
        Flags: 0,
    };
    let levels = [D3D_FEATURE_LEVEL_11_0];
    let mut swapchain: Option<IDXGISwapChain> = None;
    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;
    // SAFETY: all pointers are valid out-pointers; the desc is initialized.
    unsafe {
        D3D11CreateDeviceAndSwapChain(
            None::<&IDXGIAdapter>,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_FLAG(0),
            Some(&levels),
            7,
            Some(&desc),
            Some(&mut swapchain),
            Some(&mut device),
            None,
            Some(&mut context),
        )
    }
    .map_err(|e| format!("D3D11CreateDeviceAndSwapChain: {e}"))?;
    let swapchain = swapchain.ok_or("no swapchain returned")?;
    if fullscreen {
        // Enter exclusive fullscreen so the recorder is tested against a target
        // that owns the display (where a dummy device bootstrap is refused).
        let _ = unsafe { swapchain.SetFullscreenState(true, None::<&IDXGIOutput>) };
    }
    // Exercise the resize/fullscreen detours once, halfway through. This target
    // holds no back-buffer reference, so `ResizeBuffers` needs no release first.
    let resize_at = frames / 2;
    for i in 0..frames {
        if i == resize_at {
            // SAFETY: `swapchain` is live; keep the buffer count, resize the
            // back buffers, and leave fullscreen (a no-op windowed swapchain).
            let _ = unsafe {
                swapchain.ResizeBuffers(0, 480, 360, DXGI_FORMAT_UNKNOWN, DXGI_SWAP_CHAIN_FLAG(0))
            };
            if !fullscreen {
                let _ = unsafe { swapchain.SetFullscreenState(false, None::<&IDXGIOutput>) };
            }
        }
        // SAFETY: `swapchain` is live; sync=1, no present flags.
        let hr = unsafe { swapchain.Present(1, DXGI_PRESENT(0)) };
        if hr.is_err() {
            return Err(format!("Present failed: {hr:?}"));
        }
        pace();
    }
    // Leave fullscreen before tearing down (the swapchain requires it).
    let _ = unsafe { swapchain.SetFullscreenState(false, None::<&IDXGIOutput>) };
    drop((device, context));
    destroy(hwnd);
    Ok(())
}

/// Present `frames` frames through a D3D11 device whose swapchain is created by
/// the **base** `IDXGIFactory::CreateSwapChain` (vtable slot 10) rather than
/// `D3D11CreateDeviceAndSwapChain`/`CreateSwapChainForHwnd`.
///
/// The factory is created through the **statically imported**
/// `dxgi.dll!CreateDXGIFactory` (and `CreateDXGIFactory1`), so the recorder's
/// IAT hooks for those two exports fire and patch the returned factory's shared
/// vtable. The base `CreateSwapChain` call then reaches the `create_swap_chain`
/// detour — the arm the other presenters never touch.
fn run_d3d11_factory(frames: u32) -> Result<(), String> {
    use windows::Win32::Foundation::{HMODULE, TRUE};
    use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
    use windows::Win32::Graphics::Direct3D11::{
        D3D11CreateDevice, ID3D11Device, D3D11_CREATE_DEVICE_FLAG,
    };
    use windows::Win32::Graphics::Dxgi::Common::{
        DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_MODE_DESC, DXGI_MODE_SCALING_UNSPECIFIED,
        DXGI_MODE_SCANLINE_ORDER_UNSPECIFIED, DXGI_RATIONAL, DXGI_SAMPLE_DESC,
    };
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory, CreateDXGIFactory1, IDXGIAdapter, IDXGIFactory, IDXGIFactory1,
        IDXGISwapChain, DXGI_PRESENT, DXGI_SWAP_CHAIN_DESC, DXGI_SWAP_EFFECT_DISCARD,
        DXGI_USAGE_RENDER_TARGET_OUTPUT,
    };

    let hwnd = create_window()?;

    let levels = [D3D_FEATURE_LEVEL_11_0];
    let mut device: Option<ID3D11Device> = None;
    // SAFETY: no adapter, hardware driver, default software module, one feature
    // level; all out-pointers are valid.
    unsafe {
        D3D11CreateDevice(
            None::<&IDXGIAdapter>,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_FLAG(0),
            Some(&levels),
            7,
            Some(&mut device),
            None,
            None,
        )
    }
    .map_err(|e| format!("D3D11CreateDevice: {e}"))?;
    let device = device.ok_or("D3D11CreateDevice returned no device")?;

    // Base `CreateDXGIFactory` -> the recorder's `create_dxgi_factory` detour.
    // SAFETY: the GUID is IDXGIFactory's.
    let factory: IDXGIFactory =
        unsafe { CreateDXGIFactory() }.map_err(|e| format!("CreateDXGIFactory: {e}"))?;
    // `CreateDXGIFactory1` -> the recorder's `create_dxgi_factory1` detour.
    // SAFETY: the GUID is IDXGIFactory1's.
    let factory1: IDXGIFactory1 =
        unsafe { CreateDXGIFactory1() }.map_err(|e| format!("CreateDXGIFactory1: {e}"))?;
    let _ = &factory1;

    let desc = DXGI_SWAP_CHAIN_DESC {
        BufferDesc: DXGI_MODE_DESC {
            Width: 320,
            Height: 240,
            RefreshRate: DXGI_RATIONAL {
                Numerator: 0,
                Denominator: 0,
            },
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            ScanlineOrdering: DXGI_MODE_SCANLINE_ORDER_UNSPECIFIED,
            Scaling: DXGI_MODE_SCALING_UNSPECIFIED,
        },
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
        BufferCount: 2,
        OutputWindow: hwnd,
        Windowed: TRUE,
        SwapEffect: DXGI_SWAP_EFFECT_DISCARD,
        Flags: 0,
    };
    let mut swapchain: Option<IDXGISwapChain> = None;
    // SAFETY: `device` is the swapchain's pDevice and `desc` is initialized.
    let hr = unsafe { factory.CreateSwapChain(&device, &desc, &mut swapchain) };
    if hr.is_err() {
        return Err(format!("CreateSwapChain: {hr:?}"));
    }
    let swapchain = swapchain.ok_or("no swapchain returned")?;

    for _ in 0..frames {
        // SAFETY: `swapchain` is live; sync=1, no present flags.
        let hr = unsafe { swapchain.Present(1, DXGI_PRESENT(0)) };
        if hr.is_err() {
            return Err(format!("Present failed: {hr:?}"));
        }
        pace();
    }
    drop((swapchain, device));
    destroy(hwnd);
    Ok(())
}

/// Present `frames` frames through a D3D9 device.
///
/// With `ex`, an `IDirect3DDevice9Ex` is created (`Direct3DCreate9Ex` →
/// `CreateDeviceEx`) and presented via `PresentEx`/`ResetEx`; otherwise a plain
/// `IDirect3DDevice9` presents via `Present`. Both call `EndScene` before the
/// present, exercising the `d3d9_endscene` detour.
fn run_d3d9(frames: u32, ex: bool) -> Result<(), String> {
    use windows::Win32::Foundation::{FALSE, TRUE};
    use windows::Win32::Graphics::Direct3D9::{
        Direct3DCreate9, IDirect3DDevice9, D3DADAPTER_DEFAULT, D3DCREATE_SOFTWARE_VERTEXPROCESSING,
        D3DDEVTYPE_HAL, D3DFMT_UNKNOWN, D3DFMT_X8R8G8B8, D3DMULTISAMPLE_NONE,
        D3DPRESENT_INTERVAL_IMMEDIATE, D3DPRESENT_PARAMETERS, D3DSWAPEFFECT_DISCARD,
        D3D_SDK_VERSION,
    };

    let hwnd = create_window()?;
    let mut params = D3DPRESENT_PARAMETERS {
        BackBufferWidth: 320,
        BackBufferHeight: 240,
        BackBufferFormat: D3DFMT_X8R8G8B8,
        BackBufferCount: 2,
        MultiSampleType: D3DMULTISAMPLE_NONE,
        MultiSampleQuality: 0,
        SwapEffect: D3DSWAPEFFECT_DISCARD,
        hDeviceWindow: hwnd,
        Windowed: TRUE,
        EnableAutoDepthStencil: FALSE,
        AutoDepthStencilFormat: D3DFMT_UNKNOWN,
        Flags: 0,
        FullScreen_RefreshRateInHz: 0,
        PresentationInterval: D3DPRESENT_INTERVAL_IMMEDIATE as u32,
    };

    if ex {
        use windows::Win32::Graphics::Direct3D9::{Direct3DCreate9Ex, IDirect3DDevice9Ex};
        // SAFETY: `Direct3DCreate9Ex` takes only the SDK version.
        let d3d = unsafe { Direct3DCreate9Ex(D3D_SDK_VERSION) }
            .map_err(|e| format!("Direct3DCreate9Ex: {e}"))?;
        let mut device: Option<IDirect3DDevice9Ex> = None;
        // SAFETY: `params`/`device` are valid; the window is live; no
        // fullscreen display mode (windowed).
        unsafe {
            d3d.CreateDeviceEx(
                D3DADAPTER_DEFAULT,
                D3DDEVTYPE_HAL,
                hwnd,
                D3DCREATE_SOFTWARE_VERTEXPROCESSING as u32,
                &mut params,
                std::ptr::null_mut(),
                &mut device,
            )
        }
        .map_err(|e| format!("CreateDeviceEx: {e}"))?;
        let device = device.ok_or("no Ex device returned")?;
        // Reset once so the `d3d9_resetex` detour is exercised (windowed).
        // Best-effort: the detour records regardless of the call's outcome.
        let _ = unsafe { device.ResetEx(&mut params, std::ptr::null_mut()) };
        for _ in 0..frames {
            // SAFETY: `device` is live; a benign Begin/EndScene pair.
            let _ = unsafe { device.BeginScene() };
            // SAFETY: `device` is live; the `d3d9_endscene` detour fires here.
            let _ = unsafe { device.EndScene() };
            // SAFETY: `device` is live; null source/dest/dirty rects, no flags.
            unsafe {
                device.PresentEx(
                    std::ptr::null(),
                    std::ptr::null(),
                    hwnd,
                    std::ptr::null(),
                    0,
                )
            }
            .map_err(|e| format!("PresentEx: {e}"))?;
            pace();
        }
        drop(device);
        destroy(hwnd);
        return Ok(());
    }

    // SAFETY: `Direct3DCreate9` takes only the SDK version.
    let d3d = unsafe { Direct3DCreate9(D3D_SDK_VERSION) }.ok_or("Direct3DCreate9 failed")?;
    let mut device: Option<IDirect3DDevice9> = None;
    // SAFETY: `params`/`device` are valid; the window is live.
    unsafe {
        d3d.CreateDevice(
            D3DADAPTER_DEFAULT,
            D3DDEVTYPE_HAL,
            hwnd,
            D3DCREATE_SOFTWARE_VERTEXPROCESSING as u32,
            &mut params,
            &mut device,
        )
    }
    .map_err(|e| format!("CreateDevice: {e}"))?;
    let device = device.ok_or("no device returned")?;
    for _ in 0..frames {
        // SAFETY: `device` is live; a benign Begin/EndScene pair before present.
        let _ = unsafe { device.BeginScene() };
        // SAFETY: `device` is live; the `d3d9_endscene` detour fires here.
        let _ = unsafe { device.EndScene() };
        // SAFETY: `device` is live; null source/dest/dirty rects.
        unsafe { device.Present(std::ptr::null(), std::ptr::null(), hwnd, std::ptr::null()) }
            .map_err(|e| format!("Present: {e}"))?;
        pace();
    }
    drop(device);
    destroy(hwnd);
    Ok(())
}

/// A D3D12 resource-transition barrier (present <-> render target).
fn d3d12_transition(
    resource: &windows::Win32::Graphics::Direct3D12::ID3D12Resource,
    before: windows::Win32::Graphics::Direct3D12::D3D12_RESOURCE_STATES,
    after: windows::Win32::Graphics::Direct3D12::D3D12_RESOURCE_STATES,
) -> windows::Win32::Graphics::Direct3D12::D3D12_RESOURCE_BARRIER {
    use std::mem::ManuallyDrop;
    use windows::Win32::Graphics::Direct3D12::{
        D3D12_RESOURCE_BARRIER, D3D12_RESOURCE_BARRIER_0, D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
        D3D12_RESOURCE_BARRIER_FLAG_NONE, D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
        D3D12_RESOURCE_TRANSITION_BARRIER,
    };
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            Transition: ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                pResource: ManuallyDrop::new(Some(resource.clone())),
                Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                StateBefore: before,
                StateAfter: after,
            }),
        },
    }
}

/// Create an `IDXGIFactory2` through the **statically imported**
/// `dxgi.dll!CreateDXGIFactory2`.
///
/// The static import matters: the injected recorder IAT-hooks the app's own
/// `dxgi.dll!CreateDXGIFactory2`, so the returned factory's vtable is patched
/// and its `CreateSwapChainForHwnd` detour then patches the D3D12 command queue
/// captured from the swapchain's `pDevice` — without this import the queue is
/// never hooked and `d3d12_executecommandlists` never fires.
fn dxgi_factory2() -> Result<windows::Win32::Graphics::Dxgi::IDXGIFactory2, String> {
    use windows::Win32::Graphics::Dxgi::{
        CreateDXGIFactory2, IDXGIFactory2, DXGI_CREATE_FACTORY_FLAGS,
    };
    // SAFETY: no factory flags; the IID is IDXGIFactory2's.
    unsafe { CreateDXGIFactory2::<IDXGIFactory2>(DXGI_CREATE_FACTORY_FLAGS(0)) }
        .map_err(|e| format!("CreateDXGIFactory2: {e}"))
}

/// Present `frames` frames through a D3D12 device, `DIRECT` command queue, and
/// `FLIP_DISCARD` swapchain (clearing the RTV and calling `Present`).
fn run_d3d12(frames: u32) -> Result<(), String> {
    use windows::core::Interface;
    use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_11_0;
    use windows::Win32::Graphics::Direct3D12::{
        D3D12CreateDevice, ID3D12CommandAllocator, ID3D12CommandList, ID3D12CommandQueue,
        ID3D12DescriptorHeap, ID3D12Device, ID3D12Fence, ID3D12GraphicsCommandList,
        ID3D12PipelineState, ID3D12Resource, D3D12_COMMAND_LIST_TYPE_DIRECT,
        D3D12_COMMAND_QUEUE_DESC, D3D12_COMMAND_QUEUE_FLAG_NONE, D3D12_CPU_DESCRIPTOR_HANDLE,
        D3D12_DESCRIPTOR_HEAP_DESC, D3D12_DESCRIPTOR_HEAP_FLAG_NONE,
        D3D12_DESCRIPTOR_HEAP_TYPE_RTV, D3D12_FENCE_FLAG_NONE, D3D12_RESOURCE_STATE_PRESENT,
        D3D12_RESOURCE_STATE_RENDER_TARGET,
    };
    use windows::Win32::Graphics::Dxgi::Common::{
        DXGI_ALPHA_MODE_UNSPECIFIED, DXGI_FORMAT_R8G8B8A8_UNORM, DXGI_FORMAT_UNKNOWN,
        DXGI_SAMPLE_DESC,
    };
    use windows::Win32::Graphics::Dxgi::{
        IDXGIOutput, IDXGISwapChain, IDXGISwapChain1, IDXGISwapChain3, DXGI_PRESENT,
        DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_CHAIN_FLAG,
        DXGI_SWAP_EFFECT_FLIP_DISCARD, DXGI_USAGE_RENDER_TARGET_OUTPUT,
    };

    let hwnd = create_window()?;

    let mut device: Option<ID3D12Device> = None;
    // SAFETY: the default adapter is used; `device` is a valid out-pointer.
    unsafe {
        D3D12CreateDevice(
            None::<&windows::core::IUnknown>,
            D3D_FEATURE_LEVEL_11_0,
            &mut device,
        )
    }
    .map_err(|e| format!("D3D12CreateDevice: {e}"))?;
    let device = device.ok_or("D3D12CreateDevice returned no device")?;

    let queue_desc = D3D12_COMMAND_QUEUE_DESC {
        Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
        Priority: 0,
        Flags: D3D12_COMMAND_QUEUE_FLAG_NONE,
        NodeMask: 0,
    };
    // SAFETY: `queue_desc` is initialized; the returned queue is live.
    let queue: ID3D12CommandQueue = unsafe { device.CreateCommandQueue(&queue_desc) }
        .map_err(|e| format!("CreateCommandQueue: {e}"))?;

    // The factory is resolved at runtime rather than imported: if it were a
    // static import, an injected recorder would patch this call's (shared)
    // vtable-creation slot, and then re-patching the already-patched swapchain
    // vtable on a late creation would store the detour as its own "original"
    // and recurse. Resolving it here keeps the recorder's bootstrap swapchain
    // patch as the single patch, so `Present` is captured without recursion.
    let factory = dxgi_factory2()?;
    let sc_desc = DXGI_SWAP_CHAIN_DESC1 {
        Width: 320,
        Height: 240,
        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
        Stereo: Default::default(),
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
        BufferCount: 2,
        Scaling: DXGI_SCALING_STRETCH,
        SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
        AlphaMode: DXGI_ALPHA_MODE_UNSPECIFIED,
        Flags: 0,
    };
    // SAFETY: `queue` is the swapchain's presentation device and `sc_desc` a
    // fully initialized description.
    let swapchain: IDXGISwapChain1 = unsafe {
        factory.CreateSwapChainForHwnd(&queue, hwnd, &sc_desc, None, None::<&IDXGIOutput>)
    }
    .map_err(|e| format!("CreateSwapChainForHwnd: {e}"))?;

    let heap_desc = D3D12_DESCRIPTOR_HEAP_DESC {
        Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV,
        NumDescriptors: 2,
        Flags: D3D12_DESCRIPTOR_HEAP_FLAG_NONE,
        NodeMask: 0,
    };
    // SAFETY: `heap_desc` is initialized.
    let rtv_heap: ID3D12DescriptorHeap = unsafe { device.CreateDescriptorHeap(&heap_desc) }
        .map_err(|e| format!("CreateDescriptorHeap: {e}"))?;
    let rtv_size =
        unsafe { device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_RTV) };
    let rtv_start = unsafe { rtv_heap.GetCPUDescriptorHandleForHeapStart() };

    let mut buffers: Vec<ID3D12Resource> = Vec::with_capacity(2);
    let mut rtv_handles = [D3D12_CPU_DESCRIPTOR_HANDLE::default(); 2];
    for i in 0..2u32 {
        // SAFETY: `i` is within the swapchain's buffer count.
        let buffer: ID3D12Resource =
            unsafe { swapchain.GetBuffer(i) }.map_err(|e| format!("GetBuffer({i}): {e}"))?;
        let mut handle = rtv_start;
        handle.ptr += i as usize * rtv_size as usize;
        // SAFETY: `handle` is an RTV heap slot; the desc is defaulted.
        unsafe { device.CreateRenderTargetView(&buffer, None, handle) };
        rtv_handles[i as usize] = handle;
        buffers.push(buffer);
    }

    // SAFETY: the allocator/list are fresh and the list is closed before use.
    let allocator: ID3D12CommandAllocator =
        unsafe { device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT) }
            .map_err(|e| format!("CreateCommandAllocator: {e}"))?;
    let list: ID3D12GraphicsCommandList = unsafe {
        device.CreateCommandList(
            0,
            D3D12_COMMAND_LIST_TYPE_DIRECT,
            &allocator,
            None::<&ID3D12PipelineState>,
        )
    }
    .map_err(|e| format!("CreateCommandList: {e}"))?;
    unsafe { list.Close() }.map_err(|e| format!("Close: {e}"))?;

    // SAFETY: initial value 0; the fence is live.
    let fence: ID3D12Fence = unsafe { device.CreateFence(0, D3D12_FENCE_FLAG_NONE) }
        .map_err(|e| format!("CreateFence: {e}"))?;

    let present: IDXGISwapChain = swapchain
        .cast()
        .map_err(|e| format!("swapchain cast: {e}"))?;
    let back_buffers: IDXGISwapChain3 = swapchain
        .cast()
        .map_err(|e| format!("IDXGISwapChain3 cast: {e}"))?;

    let mut fence_value = 0u64;
    let resize_at = frames / 2;
    for i in 0..frames {
        // Wait for the previous frame so the allocator/list are free to reuse.
        if fence_value > 0 {
            // SAFETY: the fence is live; polling its completed value is safe.
            while unsafe { fence.GetCompletedValue() } < fence_value {
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        // The list starts closed after creation, so it is reset every frame.
        // SAFETY: the previous frame has completed (or this is the first).
        unsafe { allocator.Reset() }.map_err(|e| format!("Reset allocator: {e}"))?;
        unsafe { list.Reset(&allocator, None::<&ID3D12PipelineState>) }
            .map_err(|e| format!("Reset list: {e}"))?;

        // Exercise the resize/fullscreen detours once, halfway through. The
        // list was just reset, so it no longer references the old back buffers;
        // dropping the buffers vector releases the last references, which
        // `ResizeBuffers` requires before it will retarget the buffers.
        if i == resize_at {
            buffers.clear();
            // SAFETY: the previous frame has completed and no references to the
            // back buffers remain; keep the buffer count, resize, then leave
            // fullscreen (windowed).
            unsafe {
                present.ResizeBuffers(2, 480, 360, DXGI_FORMAT_UNKNOWN, DXGI_SWAP_CHAIN_FLAG(0))
            }
            .map_err(|e| format!("ResizeBuffers({:#010x}): {e}", e.code().0 as u32))?;
            for b in 0..2u32 {
                // SAFETY: `b` is within the swapchain's buffer count.
                let buffer: ID3D12Resource = unsafe { swapchain.GetBuffer(b) }
                    .map_err(|e| format!("GetBuffer({b}): {e}"))?;
                let mut handle = rtv_start;
                handle.ptr += b as usize * rtv_size as usize;
                // SAFETY: `handle` is an RTV heap slot; the desc is defaulted.
                unsafe { device.CreateRenderTargetView(&buffer, None, handle) };
                rtv_handles[b as usize] = handle;
                buffers.push(buffer);
            }
            let _ = unsafe { present.SetFullscreenState(false, None::<&IDXGIOutput>) };
        }

        // SAFETY: current back buffer index is within `buffers`.
        let index = unsafe { back_buffers.GetCurrentBackBufferIndex() } as usize;
        let buffer = &buffers[index];
        // The Windows binding stores the barrier's resource in a `ManuallyDrop`
        // union member, so it never auto-drops. Keep each barrier in a named
        // local and release the cloned reference after the barrier is recorded;
        // otherwise one back-buffer reference is leaked per transition (two per
        // frame) and `ResizeBuffers` refuses to run while any back buffer is
        // still referenced.
        let render_target = [d3d12_transition(
            buffer,
            D3D12_RESOURCE_STATE_PRESENT,
            D3D12_RESOURCE_STATE_RENDER_TARGET,
        )];
        let to_present = [d3d12_transition(
            buffer,
            D3D12_RESOURCE_STATE_RENDER_TARGET,
            D3D12_RESOURCE_STATE_PRESENT,
        )];
        // SAFETY: the list is open and reset for this frame; records a clear
        // into the current back buffer.
        unsafe {
            list.ResourceBarrier(&render_target);
            list.ClearRenderTargetView(rtv_handles[index], &[0.02, 0.05, 0.10, 1.0], None);
            list.ResourceBarrier(&to_present);
            list.Close().map_err(|e| format!("Close: {e}"))?;
            // Release the references the barrier clones hold. The union field
            // cannot be `&mut`-borrowed (a union field never auto-derefs), so the
            // ManuallyDrop is read out by value and then dropped exactly once.
            let rt = std::ptr::read(&render_target[0].Anonymous.Transition.pResource);
            drop(std::mem::ManuallyDrop::into_inner(rt));
            let tp = std::ptr::read(&to_present[0].Anonymous.Transition.pResource);
            drop(std::mem::ManuallyDrop::into_inner(tp));
        }

        let command_lists: [Option<ID3D12CommandList>; 1] =
            [Some(list.cast().map_err(|e| format!("list cast: {e}"))?)];
        // SAFETY: the command list is closed and recorded on `queue`.
        unsafe { queue.ExecuteCommandLists(&command_lists) };

        fence_value += 1;
        // SAFETY: `fence` is live; signals once the queue has run the list.
        unsafe { queue.Signal(&fence, fence_value) }.map_err(|e| format!("Signal fence: {e}"))?;
        // SAFETY: sync interval 1, no present flags.
        unsafe { present.Present(1, DXGI_PRESENT(0)) }
            .ok()
            .map_err(|e| format!("Present: {e}"))?;
        pace();
    }

    drop((swapchain, queue, device));
    destroy(hwnd);
    Ok(())
}

/// Present `frames` frames through an OpenGL/WGL double-buffered context.
fn run_opengl(frames: u32) -> Result<(), String> {
    use windows::Win32::Graphics::OpenGL::{
        wglCreateContext, wglDeleteContext, wglMakeCurrent, ChoosePixelFormat, SetPixelFormat,
        SwapBuffers, PFD_DOUBLEBUFFER, PFD_DRAW_TO_WINDOW, PFD_SUPPORT_OPENGL, PFD_TYPE_RGBA,
        PIXELFORMATDESCRIPTOR,
    };

    let hwnd = create_window()?;
    // SAFETY: `hwnd` is a live window.
    let hdc = unsafe { GetDC(Some(hwnd)) };
    let pfd = PIXELFORMATDESCRIPTOR {
        nSize: std::mem::size_of::<PIXELFORMATDESCRIPTOR>() as u16,
        nVersion: 1,
        dwFlags: PFD_DRAW_TO_WINDOW | PFD_SUPPORT_OPENGL | PFD_DOUBLEBUFFER,
        iPixelType: PFD_TYPE_RGBA,
        cColorBits: 32,
        ..Default::default()
    };
    // SAFETY: `hdc` is a live DC; `pfd` is initialized.
    let format = unsafe { ChoosePixelFormat(hdc, &pfd) };
    if format == 0 {
        return Err("ChoosePixelFormat failed".into());
    }
    // SAFETY: `format` came from ChoosePixelFormat for this DC.
    unsafe { SetPixelFormat(hdc, format, &pfd) }.map_err(|e| format!("SetPixelFormat: {e}"))?;
    // SAFETY: `hdc` is a live DC with a pixel format set.
    let context = unsafe { wglCreateContext(hdc) }.map_err(|e| format!("wglCreateContext: {e}"))?;
    // SAFETY: `context` is a live GL context for `hdc`.
    unsafe { wglMakeCurrent(hdc, context) }.map_err(|e| format!("wglMakeCurrent: {e}"))?;

    for _ in 0..frames {
        // SAFETY: a current GL context and its DC.
        unsafe { SwapBuffers(hdc) }.map_err(|e| format!("SwapBuffers: {e}"))?;
        pace();
    }

    // SAFETY: tearing down the context we created.
    unsafe {
        let _ = wglMakeCurrent(hdc, Default::default());
        let _ = wglDeleteContext(context);
        ReleaseDC(Some(hwnd), hdc);
    }
    destroy(hwnd);
    Ok(())
}

/// Present `frames` frames through an OpenGL/WGL double-buffered context by
/// calling the **delay-loaded** `opengl32!wglSwapBuffers` export directly.
///
/// This is the validation target for the delay-load *swap* self-heal: unlike
/// the `d3d9` presenter (whose delay-loaded `Direct3DCreate9` is called once),
/// this calls a delay-loaded swap export **every frame**. The first call lets
/// `__delayLoadHelper2` overwrite the recorder's patched IAT slot, so capture
/// can only resume once the 500 ms rescan re-patches it — the loss window this
/// mode measures.
fn run_opengl_delay(frames: u32) -> Result<(), String> {
    use windows::Win32::Graphics::OpenGL::{
        wglCreateContext, wglDeleteContext, wglMakeCurrent, ChoosePixelFormat, SetPixelFormat,
        PFD_DOUBLEBUFFER, PFD_DRAW_TO_WINDOW, PFD_SUPPORT_OPENGL, PFD_TYPE_RGBA,
        PIXELFORMATDESCRIPTOR,
    };

    let hwnd = create_window()?;
    // SAFETY: `hwnd` is a live window.
    let hdc = unsafe { GetDC(Some(hwnd)) };
    let pfd = PIXELFORMATDESCRIPTOR {
        nSize: std::mem::size_of::<PIXELFORMATDESCRIPTOR>() as u16,
        nVersion: 1,
        dwFlags: PFD_DRAW_TO_WINDOW | PFD_SUPPORT_OPENGL | PFD_DOUBLEBUFFER,
        iPixelType: PFD_TYPE_RGBA,
        cColorBits: 32,
        ..Default::default()
    };
    // SAFETY: `hdc` is a live DC; `pfd` is initialized.
    let format = unsafe { ChoosePixelFormat(hdc, &pfd) };
    if format == 0 {
        return Err("ChoosePixelFormat failed".into());
    }
    // SAFETY: `format` came from ChoosePixelFormat for this DC.
    unsafe { SetPixelFormat(hdc, format, &pfd) }.map_err(|e| format!("SetPixelFormat: {e}"))?;
    // SAFETY: `hdc` is a live DC with a pixel format set. This call resolves the
    // delay-loaded `opengl32.dll`.
    let context = unsafe { wglCreateContext(hdc) }.map_err(|e| format!("wglCreateContext: {e}"))?;
    // SAFETY: `context` is a live GL context for `hdc`.
    unsafe { wglMakeCurrent(hdc, context) }.map_err(|e| format!("wglMakeCurrent: {e}"))?;

    for _ in 0..frames {
        // SAFETY: a current GL context and its DC. Calls the delay-loaded
        // `opengl32!wglSwapBuffers` export directly (not `gdi32!SwapBuffers`),
        // so the recorder's delay-IAT hook for it self-heals.
        let ok = unsafe { wglSwapBuffers(hdc) };
        if ok == 0 {
            return Err("wglSwapBuffers failed".into());
        }
        pace();
    }

    // SAFETY: tearing down the context we created.
    unsafe {
        let _ = wglMakeCurrent(hdc, Default::default());
        let _ = wglDeleteContext(context);
        ReleaseDC(Some(hwnd), hdc);
    }
    destroy(hwnd);
    Ok(())
}

/// Present `frames` frames through an OpenGL/WGL context by calling
/// `opengl32!wglSwapLayerBuffers` (the layered-swap export) rather than
/// `gdi32!SwapBuffers`.
///
/// The recorder IAT-hooks `opengl32.dll!wglSwapLayerBuffers`; reaching it needs
/// a target that calls this exact export, which is what selects
/// `wgl_swap_layer_buffers`.
fn run_opengl_layer(frames: u32) -> Result<(), String> {
    use windows::Win32::Graphics::OpenGL::{
        wglCreateContext, wglDeleteContext, wglMakeCurrent, wglSwapLayerBuffers, ChoosePixelFormat,
        SetPixelFormat, PFD_DOUBLEBUFFER, PFD_DRAW_TO_WINDOW, PFD_SUPPORT_OPENGL, PFD_TYPE_RGBA,
        PIXELFORMATDESCRIPTOR,
    };

    let hwnd = create_window()?;
    // SAFETY: `hwnd` is a live window.
    let hdc = unsafe { GetDC(Some(hwnd)) };
    let pfd = PIXELFORMATDESCRIPTOR {
        nSize: std::mem::size_of::<PIXELFORMATDESCRIPTOR>() as u16,
        nVersion: 1,
        dwFlags: PFD_DRAW_TO_WINDOW | PFD_SUPPORT_OPENGL | PFD_DOUBLEBUFFER,
        iPixelType: PFD_TYPE_RGBA,
        cColorBits: 32,
        ..Default::default()
    };
    // SAFETY: `hdc` is a live DC; `pfd` is initialized.
    let format = unsafe { ChoosePixelFormat(hdc, &pfd) };
    if format == 0 {
        return Err("ChoosePixelFormat failed".into());
    }
    // SAFETY: `format` came from ChoosePixelFormat for this DC.
    unsafe { SetPixelFormat(hdc, format, &pfd) }.map_err(|e| format!("SetPixelFormat: {e}"))?;
    // SAFETY: `hdc` is a live DC with a pixel format set.
    let context = unsafe { wglCreateContext(hdc) }.map_err(|e| format!("wglCreateContext: {e}"))?;
    // SAFETY: `context` is a live GL context for `hdc`.
    unsafe { wglMakeCurrent(hdc, context) }.map_err(|e| format!("wglMakeCurrent: {e}"))?;

    for _ in 0..frames {
        // SAFETY: a current GL context and its DC; swap the main plane.
        unsafe { wglSwapLayerBuffers(hdc, WGL_SWAP_MAIN_PLANE) }
            .map_err(|e| format!("wglSwapLayerBuffers: {e}"))?;
        pace();
    }

    // SAFETY: tearing down the context we created.
    unsafe {
        let _ = wglMakeCurrent(hdc, Default::default());
        let _ = wglDeleteContext(context);
        ReleaseDC(Some(hwnd), hdc);
    }
    destroy(hwnd);
    Ok(())
}

/// Find a directory holding a usable ANGLE pair (`libEGL.dll` +
/// `libGLESv2.dll`): the `MINIHUD_ANGLE_DIR` override first, then the given
/// candidate directories. Pure over the filesystem, so it is tested directly.
fn angle_dir_at(env: Option<PathBuf>, candidates: &[PathBuf]) -> Option<PathBuf> {
    let mut all: Vec<PathBuf> = Vec::new();
    if let Some(dir) = env {
        all.push(dir);
    }
    all.extend(candidates.iter().cloned());
    all.into_iter()
        .find(|d| d.join("libEGL.dll").is_file() && d.join("libGLESv2.dll").is_file())
}

/// Locate ANGLE on this machine. The validation target's default candidates are
/// Steam's bundled CEF (which ships an ANGLE pair); `MINIHUD_ANGLE_DIR`
/// overrides. `None` when no usable pair exists.
fn angle_dir() -> Option<PathBuf> {
    let env = std::env::var_os("MINIHUD_ANGLE_DIR").map(PathBuf::from);
    let candidates = [
        r"C:\Program Files (x86)\Steam\bin\cef\cef.win64",
        r"C:\Program Files (x86)\Steam\bin\cef\cef.win7x64",
    ]
    .map(PathBuf::from);
    angle_dir_at(env, &candidates)
}

/// Present `frames` frames through ANGLE's EGL (`libEGL.dll`), calling
/// `eglSwapBuffers` every frame.
///
/// This is the validation target for the `egl_swap_buffers` detour: the
/// recorder IAT-hooks the delay-loaded `libEGL.dll!eglSwapBuffers` slot. If no
/// ANGLE pair is found the mode reports it and exits non-zero (documented skip),
/// rather than failing to load the whole target.
fn run_angle(frames: u32) -> Result<(), String> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::System::LibraryLoader::SetDllDirectoryW;

    let dir = angle_dir().ok_or_else(|| {
        "no ANGLE (libEGL.dll + libGLESv2.dll) found; set MINIHUD_ANGLE_DIR".to_string()
    })?;
    // The delay-load helper resolves `libEGL.dll` with LoadLibrary, whose search
    // path includes the directories added by SetDllDirectoryW. Point it at the
    // ANGLE directory so both DLLs resolve.
    let dir_w: Vec<u16> = dir
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    // SAFETY: `dir_w` is a NUL-terminated wide string.
    unsafe { SetDllDirectoryW(PCWSTR(dir_w.as_ptr())) }
        .map_err(|e| format!("SetDllDirectoryW({}): {e}", dir.display()))?;

    let hwnd = create_window()?;
    // SAFETY: EGL calls below use the display/config/surface/context they
    // created; the HWND is live.
    let (dpy, surface, context) = unsafe {
        let dpy = eglGetDisplay(EGL_DEFAULT_DISPLAY);
        if dpy.is_null() {
            return Err(format!(
                "eglGetDisplay failed (eglGetError={:#x})",
                eglGetError()
            ));
        }
        let mut major = 0i32;
        let mut minor = 0i32;
        if eglInitialize(dpy, &mut major, &mut minor) == 0 {
            return Err(format!(
                "eglInitialize failed (eglGetError={:#x})",
                eglGetError()
            ));
        }
        let config_attribs = [
            EGL_SURFACE_TYPE,
            EGL_WINDOW_BIT,
            EGL_RENDERABLE_TYPE,
            EGL_OPENGL_ES2_BIT,
            EGL_RED_SIZE,
            8,
            EGL_GREEN_SIZE,
            8,
            EGL_BLUE_SIZE,
            8,
            EGL_NONE,
        ];
        let mut config: *mut c_void = std::ptr::null_mut();
        let mut count = 0i32;
        if eglChooseConfig(dpy, config_attribs.as_ptr(), &mut config, 1, &mut count) == 0
            || count < 1
            || config.is_null()
        {
            return Err(format!(
                "eglChooseConfig failed (eglGetError={:#x})",
                eglGetError()
            ));
        }
        let surface = eglCreateWindowSurface(dpy, config, hwnd.0, std::ptr::null());
        if surface.is_null() {
            return Err(format!(
                "eglCreateWindowSurface failed (eglGetError={:#x})",
                eglGetError()
            ));
        }
        let context_attribs = [EGL_CONTEXT_CLIENT_VERSION, 2, EGL_NONE];
        let context = eglCreateContext(dpy, config, EGL_NO_CONTEXT, context_attribs.as_ptr());
        if context.is_null() {
            return Err(format!(
                "eglCreateContext failed (eglGetError={:#x})",
                eglGetError()
            ));
        }
        if eglMakeCurrent(dpy, surface, surface, context) == 0 {
            return Err(format!(
                "eglMakeCurrent failed (eglGetError={:#x})",
                eglGetError()
            ));
        }
        (dpy, surface, context)
    };
    let _ = context;

    for _ in 0..frames {
        // SAFETY: `dpy`/`surface` are the live EGL objects created above.
        if unsafe { eglSwapBuffers(dpy, surface) } == 0 {
            return Err(format!(
                "eglSwapBuffers failed (eglGetError={:#x})",
                unsafe { eglGetError() }
            ));
        }
        pace();
    }

    destroy(hwnd);
    Ok(())
}

/// Present `frames` frames through a **DirectComposition** tree: a D3D11 device
/// whose swapchain is created by `IDXGIFactory2::CreateSwapChainForComposition`
/// (a windowless composition swapchain), bound to an HWND through a
/// `IDCompositionTarget` + `IDCompositionVisual`.
///
/// This is the validation target for the `create_swap_chain_for_composition`
/// detour: the recorder patches the shared `IDXGIFactory2` vtable slot 24 when
/// the statically-imported `dxgi.dll!CreateDXGIFactory2` is called (its
/// `create_dxgi_factory2` detour), and this is the only presenter that then
/// calls that exact composition slot. The composition detour's success path
/// patches the returned swapchain's `Present` slot, so `dxgi.present` fires too.
fn run_dcomp(frames: u32) -> Result<(), String> {
    use windows::core::Interface;
    use windows::Win32::Foundation::HMODULE;
    use windows::Win32::Graphics::Direct3D::{D3D_DRIVER_TYPE_HARDWARE, D3D_FEATURE_LEVEL_11_0};
    use windows::Win32::Graphics::Direct3D11::{
        D3D11CreateDevice, ID3D11Device, ID3D11DeviceContext, ID3D11RenderTargetView,
        ID3D11Texture2D, D3D11_CREATE_DEVICE_FLAG,
    };
    use windows::Win32::Graphics::DirectComposition::{
        DCompositionCreateDevice, IDCompositionDevice,
    };
    use windows::Win32::Graphics::Dxgi::Common::{
        DXGI_ALPHA_MODE_PREMULTIPLIED, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC,
    };
    use windows::Win32::Graphics::Dxgi::{
        IDXGIAdapter, IDXGIDevice, IDXGIOutput, IDXGISwapChain1, DXGI_PRESENT,
        DXGI_SCALING_STRETCH, DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
        DXGI_USAGE_RENDER_TARGET_OUTPUT,
    };

    let hwnd = create_window()?;

    let levels = [D3D_FEATURE_LEVEL_11_0];
    let mut device: Option<ID3D11Device> = None;
    let mut context: Option<ID3D11DeviceContext> = None;
    // SAFETY: no adapter, hardware driver, default software module, one feature
    // level; the out-pointers are valid.
    unsafe {
        D3D11CreateDevice(
            None::<&IDXGIAdapter>,
            D3D_DRIVER_TYPE_HARDWARE,
            HMODULE::default(),
            D3D11_CREATE_DEVICE_FLAG(0),
            Some(&levels),
            7,
            Some(&mut device),
            None,
            Some(&mut context),
        )
    }
    .map_err(|e| format!("D3D11CreateDevice: {e}"))?;
    let device = device.ok_or("D3D11CreateDevice returned no device")?;
    let context = context.ok_or("no device context returned")?;

    // The composition device is created from the D3D11 device's DXGI device.
    // SAFETY: `device` is a live ID3D11Device; it answers for IDXGIDevice.
    let dxgi_device: IDXGIDevice = device.cast().map_err(|e| format!("IDXGIDevice: {e}"))?;
    // SAFETY: `dxgi_device` is a live IDXGIDevice.
    let dcomp: IDCompositionDevice = unsafe { DCompositionCreateDevice(&dxgi_device) }
        .map_err(|e| format!("DCompositionCreateDevice: {e}"))?;

    // The factory is created through the statically-imported
    // `dxgi.dll!CreateDXGIFactory2`, so the recorder's `create_dxgi_factory2`
    // detour fires and patches the factory's shared vtable — including the
    // composition slot 24 this presenter then calls.
    let factory = dxgi_factory2()?;

    let desc = DXGI_SWAP_CHAIN_DESC1 {
        Width: 320,
        Height: 240,
        Format: DXGI_FORMAT_B8G8R8A8_UNORM,
        Stereo: Default::default(),
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
        BufferCount: 2,
        Scaling: DXGI_SCALING_STRETCH,
        SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
        AlphaMode: DXGI_ALPHA_MODE_PREMULTIPLIED,
        Flags: 0,
    };
    // SAFETY: `device` is the swapchain's pDevice (a D3D11 device) and `desc` is
    // a fully initialized composition swapchain description.
    let swapchain: IDXGISwapChain1 =
        unsafe { factory.CreateSwapChainForComposition(&device, &desc, None::<&IDXGIOutput>) }
            .map_err(|e| format!("CreateSwapChainForComposition: {e}"))?;

    // Bind the swapchain to the window through a DirectComposition visual tree.
    // SAFETY: `dcomp` is a live IDCompositionDevice; the swapchain is valid
    // IDXGIResource content; `hwnd` is a live top-level window.
    let target = unsafe { dcomp.CreateTargetForHwnd(hwnd, true) }
        .map_err(|e| format!("CreateTargetForHwnd: {e}"))?;
    let visual = unsafe { dcomp.CreateVisual() }.map_err(|e| format!("CreateVisual: {e}"))?;
    unsafe { visual.SetContent(&swapchain) }.map_err(|e| format!("SetContent: {e}"))?;
    unsafe { target.SetRoot(&visual) }.map_err(|e| format!("SetRoot: {e}"))?;
    unsafe { dcomp.Commit() }.map_err(|e| format!("Commit: {e}"))?;

    // A render-target view so the back buffer can be cleared each frame.
    let mut rtv: Option<ID3D11RenderTargetView> = None;
    // SAFETY: buffer 0 exists (BufferCount 2); `rtv` is a valid out-pointer.
    let buffer: ID3D11Texture2D =
        unsafe { swapchain.GetBuffer(0) }.map_err(|e| format!("GetBuffer: {e}"))?;
    unsafe { device.CreateRenderTargetView(&buffer, None, Some(&mut rtv)) }
        .map_err(|e| format!("CreateRenderTargetView: {e}"))?;
    let rtv = rtv.ok_or("no render target view")?;

    for _ in 0..frames {
        // SAFETY: `context`/`rtv` are live; clears the current back buffer.
        unsafe { context.ClearRenderTargetView(&rtv, &[0.02, 0.05, 0.10, 1.0]) };
        // SAFETY: `swapchain` is live; sync=1, no present flags.
        let hr = unsafe { swapchain.Present(1, DXGI_PRESENT(0)) };
        if hr.is_err() {
            return Err(format!("Present failed: {hr:?}"));
        }
        pace();
    }

    drop((
        rtv,
        buffer,
        visual,
        target,
        dcomp,
        swapchain,
        context,
        device,
        dxgi_device,
    ));
    destroy(hwnd);
    Ok(())
}

/// Present `frames` frames through a Vulkan swapchain (`ash`).
///
/// With `dynamic`, Vulkan is resolved through `GetProcAddress`
/// (`ash::Entry::load`, i.e. `libloading`), mirroring an app that never
/// statically imports the loader's proc-addr — the case the in-process IAT hook
/// cannot see and the implicit Vulkan layer must capture.
fn run_vulkan(frames: u32, dynamic: bool) -> Result<(), String> {
    use ash::khr::{surface, swapchain, win32_surface};
    use ash::vk;
    use std::ffi::{c_char, CString};

    let hwnd = create_window()?;

    let entry = if dynamic {
        // SAFETY: `Entry::load` loads `vulkan-1.dll` and resolves the loader's
        // global entry points; it is valid for the lifetime of `entry`.
        unsafe { ash::Entry::load() }.map_err(|e| format!("ash Entry::load: {e}"))?
    } else {
        // Resolve Vulkan through the *statically imported* loader entry point,
        // mirroring a natively-linked game, instead of `ash::Entry::load()`
        // (which resolves everything via `GetProcAddress` and is invisible to
        // an IAT hook). The recorder's `vulkan-1.dll!vkGetInstanceProcAddr`
        // detour then sees every subsequent lookup and can hand back its
        // `vkQueuePresentKHR` detour.
        let static_fn = ash::StaticFn {
            get_instance_proc_addr: vkGetInstanceProcAddr,
        };
        // SAFETY: `vkGetInstanceProcAddr` is the loader's entry point, imported
        // from vulkan-1.dll, and stays valid for the lifetime of `entry`.
        unsafe { ash::Entry::from_static_fn(static_fn) }
    };

    let name = CString::new("minihud hook-test").map_err(|e| e.to_string())?;
    let app_info = vk::ApplicationInfo::default()
        .application_name(&name)
        .application_version(vk::make_api_version(0, 1, 0, 0))
        .engine_name(&name)
        .engine_version(vk::make_api_version(0, 1, 0, 0))
        .api_version(vk::make_api_version(0, 1, 0, 0));
    let instance_exts: Vec<*const c_char> = [surface::NAME, win32_surface::NAME]
        .iter()
        .map(|n| n.as_ptr())
        .collect();
    let instance_info = vk::InstanceCreateInfo::default()
        .application_info(&app_info)
        .enabled_extension_names(&instance_exts);
    // SAFETY: `instance_info` is fully initialized; no allocator.
    let instance = unsafe { entry.create_instance(&instance_info, None) }
        .map_err(|e| format!("vkCreateInstance: {e}"))?;

    // SAFETY: this module's HINSTANCE; the window lives in it.
    let hinstance = unsafe { GetModuleHandleW(None) }
        .map_err(|e| e.to_string())?
        .0 as isize;
    let win32 = win32_surface::Instance::new(&entry, &instance);
    let surface_info = vk::Win32SurfaceCreateInfoKHR::default()
        .hwnd(hwnd.0 as isize)
        .hinstance(hinstance);
    // SAFETY: `hwnd` is live and `surface_info` is initialized.
    let surface = unsafe { win32.create_win32_surface(&surface_info, None) }
        .map_err(|e| format!("vkCreateWin32SurfaceKHR: {e}"))?;

    let surface_loader = surface::Instance::new(&entry, &instance);
    // SAFETY: `instance` is live.
    let devices = unsafe { instance.enumerate_physical_devices() }
        .map_err(|e| format!("vkEnumeratePhysicalDevices: {e}"))?;
    let mut chosen: Option<(vk::PhysicalDevice, u32)> = None;
    for physical in devices {
        // SAFETY: `physical` came from this instance.
        let props = unsafe { instance.get_physical_device_queue_family_properties(physical) };
        let families: Vec<QueueFamily> = props
            .iter()
            .enumerate()
            .map(|(i, p)| QueueFamily {
                graphics: p.queue_flags.contains(vk::QueueFlags::GRAPHICS),
                // SAFETY: same physical device, valid family index.
                present: unsafe {
                    win32.get_physical_device_win32_presentation_support(physical, i as u32)
                },
            })
            .collect();
        if let Some(index) = pick_queue_family(&families) {
            chosen = Some((physical, index));
            break;
        }
    }
    let (physical, queue_family) =
        chosen.ok_or("no queue family supports graphics + win32 presentation")?;

    let priorities = [1.0f32];
    let queue_info = vk::DeviceQueueCreateInfo::default()
        .queue_family_index(queue_family)
        .queue_priorities(&priorities);
    let device_exts: Vec<*const c_char> = [swapchain::NAME].iter().map(|n| n.as_ptr()).collect();
    let device_info = vk::DeviceCreateInfo::default()
        .queue_create_infos(std::slice::from_ref(&queue_info))
        .enabled_extension_names(&device_exts);
    // SAFETY: `physical` supports `queue_family`; the info is initialized.
    let device = unsafe { instance.create_device(physical, &device_info, None) }
        .map_err(|e| format!("vkCreateDevice: {e}"))?;
    // SAFETY: `queue_family` was chosen for this device and has index 0.
    let queue = unsafe { device.get_device_queue(queue_family, 0) };

    let swapchain_loader = swapchain::Device::new(&instance, &device);
    // SAFETY: `surface` was created for `physical`; queries write into locals.
    let caps =
        unsafe { surface_loader.get_physical_device_surface_capabilities(physical, surface) }
            .map_err(|e| format!("surface capabilities: {e}"))?;
    let formats = unsafe { surface_loader.get_physical_device_surface_formats(physical, surface) }
        .map_err(|e| format!("surface formats: {e}"))?;
    let present_modes =
        unsafe { surface_loader.get_physical_device_surface_present_modes(physical, surface) }
            .map_err(|e| format!("surface present modes: {e}"))?;

    let format = formats
        .iter()
        .copied()
        .find(|f| f.format == vk::Format::B8G8R8A8_SRGB)
        .or_else(|| formats.first().copied())
        .ok_or("the surface reports no formats")?;
    let present_mode = present_modes
        .iter()
        .copied()
        .find(|m| *m == vk::PresentModeKHR::FIFO)
        .unwrap_or(vk::PresentModeKHR::FIFO);
    let extent = vk::Extent2D {
        width: 320,
        height: 240,
    };
    let create_info = vk::SwapchainCreateInfoKHR::default()
        .surface(surface)
        .min_image_count(caps.min_image_count + 1)
        .image_format(format.format)
        .image_color_space(format.color_space)
        .image_extent(extent)
        .image_array_layers(1)
        .image_usage(vk::ImageUsageFlags::COLOR_ATTACHMENT)
        .image_sharing_mode(vk::SharingMode::EXCLUSIVE)
        .pre_transform(caps.current_transform)
        .composite_alpha(vk::CompositeAlphaFlagsKHR::OPAQUE)
        .present_mode(present_mode)
        .clipped(true)
        .old_swapchain(vk::SwapchainKHR::null());
    // SAFETY: `create_info` is fully initialized and supported by the device.
    let swapchain = unsafe { swapchain_loader.create_swapchain(&create_info, None) }
        .map_err(|e| format!("vkCreateSwapchainKHR: {e}"))?;
    let images = unsafe { swapchain_loader.get_swapchain_images(swapchain) }
        .map_err(|e| format!("vkGetSwapchainImagesKHR: {e}"))?;

    let mut views = Vec::with_capacity(images.len());
    for image in &images {
        let range = vk::ImageSubresourceRange::default()
            .aspect_mask(vk::ImageAspectFlags::COLOR)
            .level_count(1)
            .layer_count(1);
        let view_info = vk::ImageViewCreateInfo::default()
            .image(*image)
            .view_type(vk::ImageViewType::TYPE_2D)
            .format(format.format)
            .subresource_range(range);
        // SAFETY: `view_info` is fully initialized for this device.
        let view = unsafe { device.create_image_view(&view_info, None) }
            .map_err(|e| format!("vkCreateImageView: {e}"))?;
        views.push(view);
    }

    let attachment = vk::AttachmentDescription::default()
        .format(format.format)
        .samples(vk::SampleCountFlags::TYPE_1)
        .load_op(vk::AttachmentLoadOp::CLEAR)
        .store_op(vk::AttachmentStoreOp::STORE)
        .initial_layout(vk::ImageLayout::UNDEFINED)
        .final_layout(vk::ImageLayout::PRESENT_SRC_KHR);
    let color_ref = [vk::AttachmentReference::default()
        .attachment(0)
        .layout(vk::ImageLayout::COLOR_ATTACHMENT_OPTIMAL)];
    let subpass = vk::SubpassDescription::default()
        .pipeline_bind_point(vk::PipelineBindPoint::GRAPHICS)
        .color_attachments(&color_ref);
    let dependencies = [vk::SubpassDependency::default()
        .src_subpass(vk::SUBPASS_EXTERNAL)
        .dst_subpass(0)
        .src_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
        .dst_stage_mask(vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT)
        .dst_access_mask(vk::AccessFlags::COLOR_ATTACHMENT_WRITE)];
    let render_pass_info = vk::RenderPassCreateInfo::default()
        .attachments(std::slice::from_ref(&attachment))
        .subpasses(std::slice::from_ref(&subpass))
        .dependencies(&dependencies);
    // SAFETY: `render_pass_info` is fully initialized.
    let render_pass = unsafe { device.create_render_pass(&render_pass_info, None) }
        .map_err(|e| format!("vkCreateRenderPass: {e}"))?;

    let mut framebuffers = Vec::with_capacity(views.len());
    for view in &views {
        let fb_info = vk::FramebufferCreateInfo::default()
            .render_pass(render_pass)
            .attachments(std::slice::from_ref(view))
            .width(extent.width)
            .height(extent.height)
            .layers(1);
        // SAFETY: `fb_info` is fully initialized.
        let fb = unsafe { device.create_framebuffer(&fb_info, None) }
            .map_err(|e| format!("vkCreateFramebuffer: {e}"))?;
        framebuffers.push(fb);
    }

    let pool_info = vk::CommandPoolCreateInfo::default().queue_family_index(queue_family);
    // SAFETY: `pool_info` is initialized for the graphics queue family.
    let pool = unsafe { device.create_command_pool(&pool_info, None) }
        .map_err(|e| format!("vkCreateCommandPool: {e}"))?;
    let alloc_info = vk::CommandBufferAllocateInfo::default()
        .command_pool(pool)
        .level(vk::CommandBufferLevel::PRIMARY)
        .command_buffer_count(1);
    // SAFETY: `alloc_info` is initialized.
    let mut command_buffers = unsafe { device.allocate_command_buffers(&alloc_info) }
        .map_err(|e| format!("vkAllocateCommandBuffers: {e}"))?;
    let command_buffer = command_buffers.pop().ok_or("no command buffer allocated")?;

    let semaphore_info = vk::SemaphoreCreateInfo::default();
    // SAFETY: `semaphore_info` is initialized.
    let image_available = unsafe { device.create_semaphore(&semaphore_info, None) }
        .map_err(|e| format!("vkCreateSemaphore: {e}"))?;
    let render_finished = unsafe { device.create_semaphore(&semaphore_info, None) }
        .map_err(|e| format!("vkCreateSemaphore: {e}"))?;

    let clear_values = [vk::ClearValue {
        color: vk::ClearColorValue {
            float32: [0.02, 0.05, 0.10, 1.0],
        },
    }];
    let render_area = vk::Rect2D {
        offset: vk::Offset2D { x: 0, y: 0 },
        extent,
    };

    for _ in 0..frames {
        // SAFETY: `swapchain` is live and a semaphore is signalled on acquire.
        let (index, _suboptimal) = unsafe {
            swapchain_loader.acquire_next_image(
                swapchain,
                u64::MAX,
                image_available,
                vk::Fence::null(),
            )
        }
        .map_err(|e| format!("vkAcquireNextImageKHR: {e}"))?;

        // SAFETY: the queue is idle (previous frame's device wait); the
        // command buffer is reset and recorded afresh for this image.
        unsafe {
            device
                .reset_command_buffer(command_buffer, vk::CommandBufferResetFlags::empty())
                .map_err(|e| format!("vkResetCommandBuffer: {e}"))?;
            let begin = vk::CommandBufferBeginInfo::default()
                .flags(vk::CommandBufferUsageFlags::ONE_TIME_SUBMIT);
            device
                .begin_command_buffer(command_buffer, &begin)
                .map_err(|e| format!("vkBeginCommandBuffer: {e}"))?;
            let pass_begin = vk::RenderPassBeginInfo::default()
                .render_pass(render_pass)
                .framebuffer(framebuffers[index as usize])
                .render_area(render_area)
                .clear_values(&clear_values);
            device.cmd_begin_render_pass(command_buffer, &pass_begin, vk::SubpassContents::INLINE);
            device.cmd_end_render_pass(command_buffer);
            device
                .end_command_buffer(command_buffer)
                .map_err(|e| format!("vkEndCommandBuffer: {e}"))?;
        }

        let wait_semaphores = [image_available];
        let wait_stages = [vk::PipelineStageFlags::COLOR_ATTACHMENT_OUTPUT];
        let signal_semaphores = [render_finished];
        let buffers = [command_buffer];
        let submit = vk::SubmitInfo::default()
            .wait_semaphores(&wait_semaphores)
            .wait_dst_stage_mask(&wait_stages)
            .command_buffers(&buffers)
            .signal_semaphores(&signal_semaphores);
        // SAFETY: the command buffer is recorded and the semaphores are live.
        unsafe { device.queue_submit(queue, std::slice::from_ref(&submit), vk::Fence::null()) }
            .map_err(|e| format!("vkQueueSubmit: {e}"))?;

        let swapchains = [swapchain];
        let indices = [index];
        let present_info = vk::PresentInfoKHR::default()
            .wait_semaphores(&signal_semaphores)
            .swapchains(&swapchains)
            .image_indices(&indices);
        // SAFETY: `queue` supports presentation to `swapchain`.
        unsafe { swapchain_loader.queue_present(queue, &present_info) }
            .map_err(|e| format!("vkQueuePresentKHR: {e}"))?;

        // Drain the queue so the single semaphore pair and command buffer are
        // safe to reuse next frame.
        // SAFETY: all queue work, including the present, was just submitted.
        unsafe { device.device_wait_idle() }.map_err(|e| format!("vkDeviceWaitIdle: {e}"))?;
        pace();
    }

    // Tear down; destroying the device frees its child objects.
    // SAFETY: nothing is in flight after the final device wait.
    unsafe {
        swapchain_loader.destroy_swapchain(swapchain, None);
        device.destroy_device(None);
        surface_loader.destroy_surface(surface, None);
        instance.destroy_instance(None);
    }
    destroy(hwnd);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(v: &[&str]) -> Vec<String> {
        v.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn cli_defaults_to_d3d11_and_two_thousand_frames() {
        let c = parse_cli(&[]);
        assert_eq!(c.api, "d3d11");
        assert_eq!(c.frames, 2000);
        assert!(!c.dynamic);
    }

    #[test]
    fn cli_parses_the_api_and_frame_count() {
        let c = parse_cli(&args(&["--api", "vulkan", "--frames", "600"]));
        assert_eq!(c.api, "vulkan");
        assert_eq!(c.frames, 600);
        assert!(!c.dynamic, "static vulkan by default");
    }

    #[test]
    fn cli_selects_the_dynamic_vulkan_loader() {
        // Catches: the dynamic path silently not selected, so the fully-dynamic
        // app exercises the static import the layer is meant to bypass.
        assert!(parse_cli(&args(&["--api", "vulkan", "--dynamic"])).dynamic);
        let c = parse_cli(&args(&["--api", "vulkan-dynamic", "--frames", "300"]));
        assert_eq!(c.api, "vulkan", "vulkan-dynamic is an alias for vulkan");
        assert!(c.dynamic);
        assert_eq!(c.frames, 300);
        assert!(
            !parse_cli(&args(&["--api", "d3d11", "--dynamic"]))
                .api
                .is_empty(),
            "dynamic is orthogonal to the api name"
        );
    }

    #[test]
    fn cli_ignores_a_non_numeric_frame_count() {
        assert_eq!(parse_cli(&args(&["--frames", "lots"])).frames, 2000);
        assert_eq!(parse_cli(&args(&["--api"])).api, "d3d11");
    }

    #[test]
    fn cli_selects_fullscreen() {
        // Catches: `--fullscreen` not parsed, so the exclusive-fullscreen
        // acceptance case silently runs windowed (the case a dummy device
        // bootstrap is refused in would never be exercised).
        assert!(!parse_cli(&args(&["--api", "d3d11"])).fullscreen);
        assert!(parse_cli(&args(&["--api", "d3d11", "--fullscreen"])).fullscreen);
    }

    #[test]
    fn cli_maps_each_api_name_to_a_presenter_including_d3d9ex() {
        // Catches: an `--api` name with no presenter arm (the process errors
        // instead of presenting), and the `d3d9ex` path silently aliasing plain
        // d3d9 — the Ex device is what exercises `PresentEx`/`ResetEx`.
        assert_eq!(presenter_for("d3d11"), Some(Presenter::D3d11));
        assert_eq!(presenter_for("d3d9"), Some(Presenter::D3d9));
        assert_eq!(presenter_for("d3d9ex"), Some(Presenter::D3d9Ex));
        assert_eq!(presenter_for("d3d12"), Some(Presenter::D3d12));
        assert_eq!(presenter_for("vulkan"), Some(Presenter::Vulkan));
        assert_eq!(presenter_for("vk"), Some(Presenter::Vulkan));
        assert_eq!(presenter_for("opengl"), Some(Presenter::OpenGl));
        assert_eq!(presenter_for("gl"), Some(Presenter::OpenGl));
        assert_eq!(presenter_for("bogus"), None);
        assert_eq!(parse_cli(&args(&["--api", "d3d9ex"])).api, "d3d9ex");
    }

    #[test]
    fn cli_selects_the_delay_loaded_wgl_presenter() {
        // Catches: `opengl-delay` silently aliasing the plain `opengl` presenter,
        // which calls `gdi32!SwapBuffers`. The whole point of this mode is to
        // call the *delay-loaded* `opengl32!wglSwapBuffers` swap export, so the
        // target must select a distinct presenter.
        assert_eq!(presenter_for("opengl-delay"), Some(Presenter::OpenGlDelay));
        assert_eq!(
            parse_cli(&args(&["--api", "opengl-delay"])).api,
            "opengl-delay"
        );
        assert_ne!(
            presenter_for("opengl-delay"),
            presenter_for("opengl"),
            "opengl-delay must not alias the gdi32 SwapBuffers presenter"
        );
    }

    #[test]
    fn cli_selects_the_base_factory_layer_and_egl_presenters() {
        // Catches: any of the untested detour-arm presenters silently aliasing
        // another mode. `d3d11-factory` must create the swapchain through the
        // *base* `IDXGIFactory::CreateSwapChain` (not `ForHwnd`); `opengl-layer`
        // must call `wglSwapLayerBuffers` (not `gdi32!SwapBuffers`); `angle`/`egl`
        // must present through `eglSwapBuffers`. Aliasing any of these leaves the
        // corresponding recorder detour at 0 hits.
        assert_eq!(
            presenter_for("d3d11-factory"),
            Some(Presenter::D3d11Factory)
        );
        assert_eq!(presenter_for("opengl-layer"), Some(Presenter::OpenGlLayer));
        assert_eq!(presenter_for("angle"), Some(Presenter::Angle));
        assert_eq!(presenter_for("egl"), Some(Presenter::Angle));
        assert_ne!(
            presenter_for("d3d11-factory"),
            presenter_for("d3d11"),
            "the factory mode must not alias the ForHwnd D3D11 presenter"
        );
        assert_ne!(
            presenter_for("opengl-layer"),
            presenter_for("opengl"),
            "opengl-layer must not alias the gdi32 SwapBuffers presenter"
        );
        assert_eq!(
            parse_cli(&args(&["--api", "d3d11-factory"])).api,
            "d3d11-factory"
        );
        assert_eq!(parse_cli(&args(&["--api", "angle"])).api, "angle");
    }

    #[test]
    fn cli_selects_the_directcomposition_presenter() {
        // Catches: `dcomp` silently aliasing another presenter — the composition
        // swapchain detour then never fires — or being unmapped altogether (the
        // process errors instead of presenting). `dcomp` must select its own
        // variant so `IDXGIFactory2::CreateSwapChainForComposition` is actually
        // called and the recorder's `create_swap_chain_for_composition` arm runs.
        assert_eq!(presenter_for("dcomp"), Some(Presenter::Dcomp));
        assert_eq!(presenter_for("directcomposition"), Some(Presenter::Dcomp));
        assert_ne!(
            presenter_for("dcomp"),
            presenter_for("d3d11"),
            "dcomp must not alias the ForHwnd D3D11 presenter"
        );
        assert_eq!(parse_cli(&args(&["--api", "dcomp"])).api, "dcomp");
    }

    /// Locate a usable ANGLE pair (`libEGL.dll` + `libGLESv2.dll`) on this
    /// machine, or `None`. Pure w.r.t. the filesystem so it can be asserted.
    #[test]
    fn angle_dir_prefers_the_env_override_and_validates_the_pair() {
        // Catches: accepting a directory that has only one of the two DLLs (the
        // present then fails to load), and ignoring the `MINIHUD_ANGLE_DIR`
        // override (a machine whose ANGLE is not under the default candidate).
        let fake = std::env::temp_dir().join("minihud-angle-probe");
        let _ = std::fs::create_dir_all(&fake);
        assert_eq!(
            angle_dir_at(Some(fake.clone()), &[]),
            None,
            "an empty candidate dir has neither DLL"
        );
        std::fs::write(fake.join("libEGL.dll"), b"x").unwrap();
        assert_eq!(
            angle_dir_at(Some(fake.clone()), &[]),
            None,
            "only one of the pair must not qualify"
        );
        std::fs::write(fake.join("libGLESv2.dll"), b"x").unwrap();
        assert_eq!(
            angle_dir_at(Some(fake.clone()), &[]),
            Some(fake.clone()),
            "both DLLs present must qualify"
        );
        let _ = std::fs::remove_dir_all(&fake);
    }

    #[test]
    fn queue_family_selection_requires_graphics_and_present() {
        let fams = [
            QueueFamily {
                graphics: true,
                present: false,
            },
            QueueFamily {
                graphics: false,
                present: true,
            },
            QueueFamily {
                graphics: true,
                present: true,
            },
        ];
        assert_eq!(pick_queue_family(&fams), Some(2));
        assert_eq!(pick_queue_family(&[]), None);
        assert_eq!(
            pick_queue_family(&[QueueFamily {
                graphics: true,
                present: false,
            }]),
            None
        );
    }
}
