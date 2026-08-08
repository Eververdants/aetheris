//! aetheris-ui: programmatic Win32 dialog shell.
//!
//! Registers a window class, creates a main window titled "aetheris", shows
//! the IPC pipe name in a status static control, runs the message loop, and
//! exits cleanly (code 0) when the window is closed. No `.rc`, no GUI
//! framework. Real panels land in a later task.

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::Graphics::Gdi::{GetSysColorBrush, COLOR_BTNFACE};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DefWindowProcW, DispatchMessageW, GetMessageW, LoadCursorW, LoadIconW,
    PostQuitMessage, RegisterClassW, ShowWindow, TranslateMessage, CW_USEDEFAULT, IDC_ARROW,
    IDI_APPLICATION, MSG, SW_SHOW, WINDOW_EX_STYLE, WNDCLASSW, WS_CHILD, WS_CLIPSIBLINGS,
    WS_OVERLAPPEDWINDOW, WS_VISIBLE, WM_DESTROY,
};

/// Window-procedure signature: `unsafe extern "system" fn(HWND, u32, WPARAM, LPARAM) -> LRESULT`.
unsafe extern "system" fn wndproc(
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

fn main() {
    if let Err(e) = run() {
        eprintln!("aetheris-ui: {e}");
        std::process::exit(1);
    }
}

fn run() -> windows::core::Result<()> {
    // Parse `--pipe <name>`; default to the service's well-known pipe.
    let mut pipe = aetheris_core::ipc::DEFAULT_PIPE.to_string();
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--pipe" if i + 1 < args.len() => {
                pipe = args[i + 1].clone();
                i += 2;
            }
            "--pipe" => {
                eprintln!("aetheris-ui: --pipe requires a value");
                std::process::exit(2);
            }
            other => {
                eprintln!("aetheris-ui: unknown argument: {other}");
                std::process::exit(2);
            }
        }
    }

    // Wide (UTF-16) text for the status static; kept alive for the CreateWindowExW call below,
    // which copies the string synchronously.
    let status: Vec<u16> = format!("Pipe: {pipe}")
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    unsafe {
        let hinstance: HINSTANCE = GetModuleHandleW(None)?.into();

        let wc = WNDCLASSW {
            style: Default::default(),
            lpfnWndProc: Some(wndproc),
            cbClsExtra: 0,
            cbWndExtra: 0,
            hInstance: hinstance,
            hIcon: LoadIconW(None, IDI_APPLICATION)?,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            hbrBackground: GetSysColorBrush(COLOR_BTNFACE),
            lpszMenuName: PCWSTR::null(),
            lpszClassName: w!("aetheris_main"),
        };

        if RegisterClassW(&wc) == 0 {
            return Err(windows::core::Error::from_thread());
        }

        let hwnd = CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            w!("aetheris_main"),
            w!("aetheris"),
            WS_OVERLAPPEDWINDOW | WS_CLIPSIBLINGS,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            480,
            320,
            None,
            None,
            Some(hinstance),
            None,
        )?;

        // Status static child control showing the pipe name.
        CreateWindowExW(
            WINDOW_EX_STYLE::default(),
            w!("Static"),
            PCWSTR(status.as_ptr()),
            WS_CHILD | WS_VISIBLE,
            12,
            12,
            456,
            28,
            Some(hwnd),
            None,
            Some(hinstance),
            None,
        )?;

        let _ = ShowWindow(hwnd, SW_SHOW);
    }

    // Standard message loop; returns when WM_QUIT arrives (window closed).
    let mut msg = MSG::default();
    loop {
        let r = unsafe { GetMessageW(&mut msg, None, 0, 0) };
        if r.0 == 0 {
            break; // WM_QUIT
        }
        if r.0 == -1 {
            return Err(windows::core::Error::from_thread());
        }
        unsafe {
            _ = TranslateMessage(&msg);
            _ = DispatchMessageW(&msg);
        }
    }
    Ok(())
}
