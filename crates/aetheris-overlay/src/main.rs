//! aetheris-overlay: zero-overhead DirectComposition telemetry panel.
//!
//! A single-threaded, no-injection overlay that will show live aetheris
//! telemetry (Task 2). For v2-C Task 1 it initialises the full graphics
//! pipeline and renders a placeholder telemetry line into a small top-left
//! panel.
//!
//! Pipeline (in `run`):
//!
//! 1. `CreateDXGIFactory2` → `D3D11CreateDevice` (hardware, `BGRA_SUPPORT` for
//!    D2D interop) → `IDXGIFactory2::CreateSwapChainForComposition` with an
//!    `DXGI_ALPHA_MODE_PREMULTIPLIED` flip-model swapchain (800x140).
//! 2. `DCompositionCreateDevice` (on the device's `IDXGIDevice`) →
//!    `IDCompositionDevice::CreateVisual` → `SetContent(swapchain)` →
//!    `CreateTargetForHwnd` → `SetRoot` → `Commit`. The visual supplies all
//!    pixels, so the window itself is a hidden top-most click-through popup
//!    (`WS_EX_TOPMOST|WS_EX_NOACTIVATE|WS_EX_TRANSPARENT|WS_EX_LAYERED`, NULL
//!    background brush).
//! 3. D2D + DWrite render the placeholder text into the swapchain back buffer,
//!    then `Present`. For Task 1 we render once at startup and run a plain
//!    `GetMessageW` loop; Task 2 re-invokes `render_telemetry` per 1 Hz update.
//!
//! Rendering path chosen for windows 0.62.2: the D2D device context attaches
//! to the flip-model back buffer through `ID2D1DeviceContext::
//! CreateBitmapFromDxgiSurface` (`IDXGISurface2` interop) with that bitmap set
//! as the context target. `ID2D1DCRenderTarget` was considered and rejected:
//! it binds a GDI `HDC` to a render target and cannot attach to a flip-model
//! DXGI back buffer (it targets `IDXGISurface` render targets built with
//! `CreateDxgiSurfaceRenderTarget`, which does not compose through the D2D
//! device). The draw primitives (`BeginDraw`/`EndDraw`/`Clear`/`DrawText`)
//! live on the `ID2D1RenderTarget` parent interface, which the pinned windows
//! crate does not re-expose on `ID2D1DeviceContext`; the parent view is
//! obtained via the generated `From` interface-hierarchy conversion (a
//! zero-cost transmute of the same COM pointer).

use windows::core::{w, Interface, PCWSTR, Result};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, S_FALSE, WPARAM};
use windows::Win32::Graphics::Direct2D::Common::{D2D1_COLOR_F, D2D_RECT_F};
use windows::Win32::Graphics::Direct2D::{
    D2D1CreateFactory, D2D1_DEVICE_CONTEXT_OPTIONS_NONE, D2D1_DRAW_TEXT_OPTIONS_NONE,
    D2D1_FACTORY_TYPE_SINGLE_THREADED, ID2D1Bitmap1, ID2D1Device, ID2D1DeviceContext,
    ID2D1Factory1, ID2D1RenderTarget, ID2D1SolidColorBrush,
};
use windows::Win32::Graphics::Direct3D::D3D_DRIVER_TYPE_HARDWARE;
use windows::Win32::Graphics::Direct3D11::{
    D3D11CreateDevice, D3D11_CREATE_DEVICE_BGRA_SUPPORT, D3D11_SDK_VERSION, ID3D11Device,
};
use windows::Win32::Graphics::DirectComposition::{
    DCompositionCreateDevice, IDCompositionDevice, IDCompositionTarget, IDCompositionVisual,
};
use windows::Win32::Graphics::DirectWrite::{
    DWriteCreateFactory, DWRITE_FACTORY_TYPE_SHARED, DWRITE_FONT_STRETCH_NORMAL,
    DWRITE_FONT_STYLE_NORMAL, DWRITE_FONT_WEIGHT_NORMAL, DWRITE_MEASURING_MODE_NATURAL,
    DWRITE_PARAGRAPH_ALIGNMENT_NEAR, DWRITE_TEXT_ALIGNMENT_LEADING, IDWriteFactory, IDWriteTextFormat,
};
use windows::Win32::Graphics::Dxgi::Common::{
    DXGI_ALPHA_MODE_PREMULTIPLIED, DXGI_FORMAT_B8G8R8A8_UNORM, DXGI_SAMPLE_DESC,
};
use windows::Win32::Graphics::Dxgi::{
    CreateDXGIFactory2, DXGI_CREATE_FACTORY_FLAGS, DXGI_PRESENT, DXGI_SCALING_STRETCH,
    DXGI_SWAP_CHAIN_DESC1, DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL, DXGI_USAGE_RENDER_TARGET_OUTPUT,
    IDXGIDevice, IDXGIFactory2, IDXGISurface2, IDXGISwapChain1,
};
use windows::Win32::System::Com::{CoInitializeEx, COINIT_APARTMENTTHREADED};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Input::KeyboardAndMouse::VK_ESCAPE;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW, PostQuitMessage,
    RegisterClassW, TranslateMessage, MSG, WM_CLOSE, WM_DESTROY, WM_KEYDOWN, WM_NCHITTEST, WNDCLASSW,
    WS_EX_LAYERED, WS_EX_NOACTIVATE, WS_EX_TOPMOST, WS_EX_TRANSPARENT, WS_POPUP, WS_VISIBLE,
    HTTRANSPARENT,
};

use aetheris_core::ipc::DEFAULT_PIPE;

/// Panel size in pixels, pinned to the top-left corner of the screen.
const PANEL_W: u32 = 800;
const PANEL_H: u32 = 140;

/// Window class / title shared by the overlay popup.
const CLASS_NAME: windows::core::PCWSTR = w!("aetheris_overlay");

// ---------------------------------------------------------------------------
// Window procedure
// ---------------------------------------------------------------------------

unsafe extern "system" fn wndproc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    match msg {
        // Click-through: report HTTRANSPARENT so mouse input falls through to
        // the windows beneath the overlay. This is what makes the overlay
        // truly transparent to input on top of WS_EX_TRANSPARENT|WS_EX_LAYERED.
        WM_NCHITTEST => LRESULT(HTTRANSPARENT as isize),

        // ESC exits the overlay cleanly (mostly useful when injected via
        // PostMessage; a WS_EX_NOACTIVATE window never takes keyboard focus,
        // so a physical ESC reaches no one — the primary exit is WM_DESTROY).
        WM_KEYDOWN => {
            if (wparam.0 & 0xffff) as u16 == VK_ESCAPE.0 {
                let _ = DestroyWindow(hwnd);
                LRESULT(0)
            } else {
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
        }

        WM_CLOSE => {
            let _ = DestroyWindow(hwnd);
            LRESULT(0)
        }

        WM_DESTROY => {
            PostQuitMessage(0);
            LRESULT(0)
        }

        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }
}

// ---------------------------------------------------------------------------
// Rendering
// ---------------------------------------------------------------------------

/// Draw `text` into the swapchain back buffer and present it.
///
/// Re-acquires the flip-model back buffer on every call (flip-model buffers
/// must be re-obtained after each `Present`), creates a fresh `ID2D1Bitmap1`
/// target on it, clears to transparent, draws the telemetry text with DWrite,
/// and presents. Task 2 will call this once per 1 Hz telemetry update.
unsafe fn render_telemetry(
    dctx: &ID2D1DeviceContext,
    rt: &ID2D1RenderTarget,
    swapchain: &IDXGISwapChain1,
    text_format: &IDWriteTextFormat,
    text_brush: &ID2D1SolidColorBrush,
    text: &str,
) -> Result<()> {
    // Re-acquire the flip-model back buffer on every call (flip-model buffers
    // must be re-obtained after each Present).
    let surface: IDXGISurface2 = swapchain.GetBuffer(0)?;

    // Attach a D2D bitmap target to the surface. The properties argument is
    // deliberately `None`: the windows 0.62.2 binding of
    // `CreateBitmapFromDxgiSurface` rejects an explicit `D2D1_BITMAP_PROPERTIES1`
    // with E_INVALIDARG (documented in the task report), while the null path
    // lets D2D inherit the surface's own pixel format (B8G8R8A8), premultiplied
    // alpha mode, 96 DPI and render-target binding — exactly what the
    // premultiplied composition swapchain needs for per-pixel transparency.
    let target: ID2D1Bitmap1 = dctx.CreateBitmapFromDxgiSurface(&surface, None)?;
    dctx.SetTarget(&target);

    rt.BeginDraw();
    // Fully transparent background: per-pixel alpha from the premultiplied
    // swapchain lets DComp show the desktop beneath the text.
    rt.Clear(Some(&D2D1_COLOR_F {
        r: 0.0,
        g: 0.0,
        b: 0.0,
        a: 0.0,
    }));
    let wide: Vec<u16> = text.encode_utf16().collect();
    let layout = D2D_RECT_F {
        left: 10.0,
        top: 8.0,
        right: PANEL_W as f32 - 10.0,
        bottom: PANEL_H as f32 - 8.0,
    };
    rt.DrawText(
        &wide,
        text_format,
        &layout,
        text_brush,
        D2D1_DRAW_TEXT_OPTIONS_NONE,
        DWRITE_MEASURING_MODE_NATURAL,
    );
    rt.EndDraw(None, None)?;

    // Detach the D2D target before Present so the flip-model buffer is not
    // held while the compositor reads it.
    dctx.SetTarget(None);

    swapchain.Present(0, DXGI_PRESENT(0)).ok()?;
    Ok(())
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

fn main() {
    if let Err(e) = run() {
        eprintln!("aetheris-overlay: {e}");
        std::process::exit(1);
    }
}

fn run() -> Result<()> {
    // COM must be initialized before creating a shared DWrite factory
    // (DWRITE_FACTORY_TYPE_SHARED), D2D or DComp — all assume COM and would
    // fail with CO_E_NOTINITIALIZED otherwise. This is a single-threaded UI
    // thread, so apartment-threaded is the correct model (NOT multi-threaded,
    // which is incompatible with a shared DWrite factory). S_FALSE means COM
    // was already initialized on this thread with the same model — benign,
    // keep going; anything else that failed is fatal.
    let com = unsafe { CoInitializeEx(None, COINIT_APARTMENTTHREADED) };
    if com == S_FALSE {
        eprintln!("aetheris-overlay: COM already initialized on this thread (S_FALSE)");
    } else if com.is_err() {
        return Err(com.into());
    }

    // Parse `--pipe <name>`; default to the service's well-known pipe. Task 2
    // uses it for `client_call`; Task 1 only displays it in the placeholder.
    let mut pipe = DEFAULT_PIPE.to_string();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--pipe" if i + 1 < args.len() => {
                pipe = args[i + 1].clone();
                i += 2;
            }
            "--pipe" => {
                eprintln!("aetheris-overlay: --pipe requires a value");
                std::process::exit(2);
            }
            other => {
                eprintln!("aetheris-overlay: unknown argument: {other}");
                std::process::exit(2);
            }
        }
    }

    let placeholder = format!(
        "aetheris-overlay\npipe: {pipe}\nplaceholder telemetry - mode: -, boosted: -"
    );

    unsafe {
        let hinstance: HINSTANCE = GetModuleHandleW(None)?.into();

        // Hidden top-most click-through popup. A NULL background brush means
        // the OS paints nothing; every pixel comes from the DComp visual.
        let wc = WNDCLASSW {
            style: Default::default(),
            lpfnWndProc: Some(wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinstance,
            hIcon: Default::default(),
            hCursor: Default::default(),
            hbrBackground: Default::default(),
            lpszMenuName: PCWSTR::null(),
            lpszClassName: CLASS_NAME,
        };
        if RegisterClassW(&wc) == 0 {
            return Err(windows::core::Error::from_thread());
        }

        let hwnd = CreateWindowExW(
            WS_EX_TOPMOST | WS_EX_NOACTIVATE | WS_EX_TRANSPARENT | WS_EX_LAYERED,
            CLASS_NAME,
            w!("aetheris-overlay"),
            WS_POPUP | WS_VISIBLE,
            0,
            0,
            PANEL_W as i32,
            PANEL_H as i32,
            None,
            None,
            Some(hinstance),
            None,
        )?;

        // --- DXGI + D3D11 device ---------------------------------------------
        let dxgi_factory: IDXGIFactory2 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))?;

        let mut device: Option<ID3D11Device> = None;
        D3D11CreateDevice(
            None, // default adapter
            D3D_DRIVER_TYPE_HARDWARE,
            Default::default(),
            D3D11_CREATE_DEVICE_BGRA_SUPPORT, // required for D2D interop
            None,
            D3D11_SDK_VERSION,
            Some(&mut device),
            None,
            None,
        )?;
        let device = device.expect("D3D11CreateDevice returned a null device");

        // Composition swapchain: premultiplied alpha so the panel is per-pixel
        // transparent where no text is drawn.
        let swapchain: IDXGISwapChain1 = dxgi_factory.CreateSwapChainForComposition(
            &device,
            &DXGI_SWAP_CHAIN_DESC1 {
                Width: PANEL_W,
                Height: PANEL_H,
                Format: DXGI_FORMAT_B8G8R8A8_UNORM,
                Stereo: false.into(),
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: 2,
                Scaling: DXGI_SCALING_STRETCH,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_SEQUENTIAL,
                AlphaMode: DXGI_ALPHA_MODE_PREMULTIPLIED,
                Flags: 0,
            },
            None,
        )?;

        // --- DirectComposition visual over the window ------------------------
        let dxgi_device: IDXGIDevice = device.cast()?;
        let dcomp: IDCompositionDevice = DCompositionCreateDevice(&dxgi_device)?;
        let visual: IDCompositionVisual = dcomp.CreateVisual()?;
        visual.SetContent(&swapchain)?;
        let target: IDCompositionTarget = dcomp.CreateTargetForHwnd(hwnd, true)?;
        target.SetRoot(&visual)?;
        dcomp.Commit()?;

        // --- D2D + DWrite ----------------------------------------------------
        // ID2D1Factory1, not ID2D1Factory: CreateDevice (the D2D device from
        // the DXGI device) only exists on the factory v1+ interface.
        let d2d: ID2D1Factory1 = D2D1CreateFactory(D2D1_FACTORY_TYPE_SINGLE_THREADED, None)?;
        let d2d_device: ID2D1Device = d2d.CreateDevice(&dxgi_device)?;
        let dctx: ID2D1DeviceContext =
            d2d_device.CreateDeviceContext(D2D1_DEVICE_CONTEXT_OPTIONS_NONE)?;
        // Zero-cost From conversion (interface hierarchy) to the parent
        // ID2D1RenderTarget view that owns BeginDraw/EndDraw/Clear/DrawText.
        let rt: ID2D1RenderTarget = dctx.clone().into();

        let dwrite: IDWriteFactory = DWriteCreateFactory(DWRITE_FACTORY_TYPE_SHARED)?;
        let text_format: IDWriteTextFormat = dwrite.CreateTextFormat(
            w!("Consolas"),
            None,
            DWRITE_FONT_WEIGHT_NORMAL,
            DWRITE_FONT_STYLE_NORMAL,
            DWRITE_FONT_STRETCH_NORMAL,
            18.0,
            w!("en-us"),
        )?;
        text_format.SetTextAlignment(DWRITE_TEXT_ALIGNMENT_LEADING)?;
        text_format.SetParagraphAlignment(DWRITE_PARAGRAPH_ALIGNMENT_NEAR)?;

        // White text brush, created once alongside the text format and reused
        // across renders (Task 2 re-invokes render_telemetry per 1 Hz update).
        let text_brush: ID2D1SolidColorBrush = rt.CreateSolidColorBrush(
            &D2D1_COLOR_F {
                r: 1.0,
                g: 1.0,
                b: 1.0,
                a: 1.0,
            },
            None,
        )?;

        render_telemetry(&dctx, &rt, &swapchain, &text_format, &text_brush, &placeholder)?;

        // --- Message loop ----------------------------------------------------
        // Render-on-demand: Task 1 draws once at startup; the loop only pumps
        // messages (ESC / WM_CLOSE / WM_DESTROY). Task 2 calls
        // `render_telemetry` from a 1 Hz timer instead of a busy render loop.
        let mut msg = MSG::default();
        loop {
            let r = GetMessageW(&mut msg, None, 0, 0);
            if r.0 == 0 {
                break; // WM_QUIT
            }
            if r.0 == -1 {
                return Err(windows::core::Error::from_thread());
            }
            _ = TranslateMessage(&msg);
            _ = DispatchMessageW(&msg);
        }
    }

    Ok(())
}
