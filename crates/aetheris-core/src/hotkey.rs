//! Overlay hotkey watcher.
//!
//! Registers a configurable system-wide hotkey and forwards each press as `()`
//! on a channel. A dedicated thread owns a message-only window and a Win32
//! message pump; `RegisterHotKey` binds the combination to that window and each
//! `WM_HOTKEY` dispatch sends a unit on the channel. Binding the hotkey to a
//! window (rather than to the thread with `hwnd = None`) lets `stop()`
//! `UnregisterHotKey` from any thread.
//!
//! The thread and window are separate from [`crate::foreground::ForegroundWatcher`]'s
//! pump, so hotkey handling stays independent of the focus watcher. The service
//! starts at most one watcher.

use std::cell::RefCell;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::{self, JoinHandle};

use windows::Win32::Foundation::{HINSTANCE, HWND, LPARAM, LRESULT, WPARAM};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::GetCurrentThreadId;
use windows::Win32::UI::Input::KeyboardAndMouse::{
    HOT_KEY_MODIFIERS, MOD_ALT, MOD_CONTROL, MOD_SHIFT, MOD_WIN, RegisterHotKey, UnregisterHotKey,
    VK_F1,
};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, CW_USEDEFAULT, DefWindowProcW, DestroyWindow, DispatchMessageW, GetMessageW,
    HWND_MESSAGE, MSG, PM_NOREMOVE, PeekMessageW, PostThreadMessageW, RegisterClassW,
    UnregisterClassW, WINDOW_EX_STYLE, WNDCLASSW, WM_HOTKEY, WM_NCDESTROY, WM_QUIT, WS_POPUP,
};
use windows::core::PCWSTR;

/// The hotkey registration id (any non-zero value; the service registers one).
const HOTKEY_ID: i32 = 1;
/// Window class name for the message-only window. Per-process unique; guarded by
/// [`STARTED`] so a second `HotkeyWatcher::start` reports a clear error instead
/// of a confusing `RegisterClassW` failure.
const HOTKEY_WND_CLASS: &str = "aetheris_hotkey_wnd";

/// Guards against registering the window class twice (process-global).
static STARTED: AtomicBool = AtomicBool::new(false);

// Sender the window proc forwards `WM_HOTKEY` to. The message-only window is
// created and pumped on the watcher thread, so the proc only ever runs there; a
// `thread_local` is therefore the right scope (no cross-thread access).
thread_local! {
    static HOTKEY_TX: RefCell<Option<Sender<()>>> = const { RefCell::new(None) };
}

/// Parse a hotkey spec like `"ctrl+alt+o"` into `(modifiers, virtual-key)`.
///
/// The string is `+`-separated tokens: zero or more of `ctrl` / `alt` /
/// `shift` / `win` (case-insensitive), then a final key token which is either a
/// single character (uppercased to its virtual-key code) or an F-key
/// `f1`..`f24`. `None` is returned for empty/garbage input (unknown modifier,
/// multi-character non-F key, `f0`/`f25+`, dangling `+`, etc.).
pub fn parse_hotkey(s: &str) -> Option<(u32, u32)> {
    let mut tokens: Vec<&str> = s.split('+').map(str::trim).collect();
    if tokens.is_empty() || tokens.iter().any(|t| t.is_empty()) {
        return None;
    }
    let key = tokens.pop()?;
    let mut mods: u32 = 0;
    for m in tokens {
        match m.to_ascii_lowercase().as_str() {
            "ctrl" => mods |= MOD_CONTROL.0,
            "alt" => mods |= MOD_ALT.0,
            "shift" => mods |= MOD_SHIFT.0,
            "win" => mods |= MOD_WIN.0,
            _ => return None,
        }
    }
    let vk = parse_key(key)?;
    Some((mods, vk))
}

/// Parse the key token: an F-key (`f1`..`f24`) or a single character.
fn parse_key(s: &str) -> Option<u32> {
    let lower = s.to_ascii_lowercase();
    if lower.len() > 1 && lower.starts_with('f') {
        let n = lower[1..].parse::<u32>().ok()?;
        if (1..=24).contains(&n) {
            return Some(VK_F1.0 as u32 - 1 + n);
        }
        return None;
    }
    let mut chars = s.chars();
    let (c, rest) = (chars.next()?, chars.next());
    if rest.is_some() {
        return None;
    }
    Some(c.to_ascii_uppercase() as u32)
}

/// Window proc for the message-only window: forwards `WM_HOTKEY` as `()`.
unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wparam: WPARAM, lparam: LPARAM) -> LRESULT {
    if msg == WM_HOTKEY {
        if let Some(tx) = HOTKEY_TX.with(|t| t.borrow().clone()) {
            let _ = tx.send(());
        }
        return LRESULT(0);
    }
    if msg == WM_NCDESTROY {
        HOTKEY_TX.with(|t| *t.borrow_mut() = None);
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

/// A running hotkey watcher: `recv()` yields one `()` per hotkey press.
pub struct HotkeyWatcher {
    rx: Receiver<()>,
    handle: Option<JoinHandle<()>>,
    /// OS thread id of the message pump, so `stop()` can post `WM_QUIT` to it.
    pump_thread_id: u32,
    /// Window handle the hotkey is bound to, so `stop()` can `UnregisterHotKey`
    /// from any thread (stored as an `isize`; `HWND` is not `Send`).
    hwnd: isize,
}

impl HotkeyWatcher {
    /// Register the hotkey and start its message pump on a dedicated thread.
    ///
    /// `start` only succeeds when the window class + message-only window +
    /// `RegisterHotKey` all succeed on the pump thread; failures surface here
    /// as `Err` so the service can log and keep running without the hotkey.
    pub fn start((mods, vk): (u32, u32)) -> Result<Self, String> {
        if STARTED.swap(true, Ordering::SeqCst) {
            return Err("HotkeyWatcher::start called twice".to_string());
        }
        let result = Self::start_inner(mods, vk);
        if result.is_err() {
            STARTED.store(false, Ordering::SeqCst);
        }
        result
    }

    fn start_inner(mods: u32, vk: u32) -> Result<Self, String> {
        let (tx, rx) = channel::<()>();
        // The pump thread publishes its OS thread id first (so `stop()` can
        // target it), then the registration outcome (so `start()` returns
        // `Err` on a failed register rather than silently never firing).
        let (tid_tx, tid_rx) = channel::<u32>();
        let (ready_tx, ready_rx) = channel::<Result<isize, String>>();

        let handle = thread::spawn(move || {
            // Windows creates a thread's message queue lazily on the first
            // GetMessage/PeekMessage. `stop()` posts WM_QUIT to this thread id
            // via PostThreadMessageW, which fails with ERROR_INVALID_THREAD_ID
            // if the queue doesn't exist yet, leaving `join()` to hang. Force
            // the queue into existence BEFORE publishing the id.
            let mut force_queue = std::mem::MaybeUninit::<MSG>::uninit();
            let _ = unsafe { PeekMessageW(force_queue.as_mut_ptr(), None, 0, 0, PM_NOREMOVE) };
            let _ = tid_tx.send(unsafe { GetCurrentThreadId() });

            // The class name Vec must outlive the window: RegisterClassW copies
            // the class data, but UnregisterClassW needs the name again at
            // teardown, so it lives for the whole closure.
            let class: Vec<u16> = HOTKEY_WND_CLASS
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            let class_pw = PCWSTR(class.as_ptr());
            let hinst = match unsafe { GetModuleHandleW(None) } {
                Ok(h) => HINSTANCE(h.0),
                Err(e) => {
                    let _ = ready_tx.send(Err(format!("GetModuleHandleW failed: {e}")));
                    return;
                }
            };

            HOTKEY_TX.with(|t| *t.borrow_mut() = Some(tx));
            let wc = WNDCLASSW {
                lpfnWndProc: Some(wnd_proc),
                hInstance: hinst,
                lpszClassName: class_pw,
                ..Default::default()
            };
            if unsafe { RegisterClassW(&wc) } == 0 {
                HOTKEY_TX.with(|t| *t.borrow_mut() = None);
                let _ = ready_tx.send(Err("RegisterClassW failed".to_string()));
                return;
            }

            let hwnd = match unsafe {
                CreateWindowExW(
                    WINDOW_EX_STYLE(0),
                    class_pw,
                    PCWSTR::null(),
                    WS_POPUP,
                    CW_USEDEFAULT,
                    CW_USEDEFAULT,
                    CW_USEDEFAULT,
                    CW_USEDEFAULT,
                    Some(HWND_MESSAGE), // message-only window
                    None,
                    Some(hinst),
                    None,
                )
            } {
                Ok(h) => h,
                Err(e) => {
                    HOTKEY_TX.with(|t| *t.borrow_mut() = None);
                    let _ = unsafe { UnregisterClassW(class_pw, Some(hinst)) };
                    let _ = ready_tx.send(Err(format!("CreateWindowExW failed: {e}")));
                    return;
                }
            };

            if unsafe { RegisterHotKey(Some(hwnd), HOTKEY_ID, HOT_KEY_MODIFIERS(mods), vk) }
                .is_err()
            {
                HOTKEY_TX.with(|t| *t.borrow_mut() = None);
                let _ = unsafe { DestroyWindow(hwnd) };
                let _ = unsafe { UnregisterClassW(class_pw, Some(hinst)) };
                let _ = ready_tx.send(Err("RegisterHotKey failed".to_string()));
                return;
            }

            let _ = ready_tx.send(Ok(hwnd.0 as isize));
            let mut msg = MSG::default();
            loop {
                let r = unsafe { GetMessageW(&mut msg, None, 0, 0) };
                if r.0 == 0 {
                    break; // WM_QUIT
                }
                unsafe { DispatchMessageW(&msg) };
            }
            // Tear down the message-only window and its class on pump exit.
            let _ = unsafe { DestroyWindow(hwnd) };
            let _ = unsafe { UnregisterClassW(class_pw, Some(hinst)) };
        });

        let pump_thread_id = tid_rx
            .recv()
            .map_err(|_| "hotkey pump thread exited before registering its id".to_string())?;
        let hwnd = ready_rx
            .recv()
            .map_err(|_| "hotkey pump thread exited during setup".to_string())??;

        Ok(Self {
            rx,
            handle: Some(handle),
            pump_thread_id,
            hwnd,
        })
    }

    /// Block until the next hotkey press, yielding `()` per press.
    pub fn recv(&self) -> Option<()> {
        self.rx.recv().ok()
    }

    /// Unregister the hotkey, ask the pump thread to exit, and join it.
    pub fn stop(mut self) {
        // The hotkey is bound to the window (not the registering thread), so
        // UnregisterHotKey is valid from any thread.
        unsafe {
            let _ = UnregisterHotKey(Some(HWND(self.hwnd as *mut core::ffi::c_void)), HOTKEY_ID);
            let _ = PostThreadMessageW(self.pump_thread_id, WM_QUIT, WPARAM(0), LPARAM(0));
        }
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use windows::Win32::UI::Input::KeyboardAndMouse::{
        MOD_ALT, MOD_CONTROL, MOD_SHIFT, MOD_WIN, VK_F9,
    };

    use super::parse_hotkey;

    #[test]
    fn ctrl_alt_o_maps_to_mod_control_alt_and_uppercase_vk() {
        let (m, vk) = parse_hotkey("ctrl+alt+o").unwrap();
        assert_eq!(m, MOD_CONTROL.0 | MOD_ALT.0);
        assert_eq!(vk, b'O' as u32);
    }

    #[test]
    fn f9_maps_to_vk_f9_with_no_modifiers() {
        let (m, vk) = parse_hotkey("f9").unwrap();
        assert_eq!(m, 0);
        assert_eq!(vk, VK_F9.0 as u32);
    }

    #[test]
    fn all_modifiers_and_f24() {
        let (m, vk) = parse_hotkey("ctrl+alt+shift+win+f24").unwrap();
        assert_eq!(m, MOD_CONTROL.0 | MOD_ALT.0 | MOD_SHIFT.0 | MOD_WIN.0);
        assert_eq!(vk, 135u32); // VK_F24 = 0x87
    }

    #[test]
    fn garbage_returns_none() {
        assert!(parse_hotkey("").is_none());
        assert!(parse_hotkey("++").is_none());
        assert!(parse_hotkey("ctrl+").is_none());
        assert!(parse_hotkey("ctrl+bogus").is_none());
        assert!(parse_hotkey("f25").is_none());
        assert!(parse_hotkey("ctrl+alt+f25").is_none());
        // The key token must be last; a modifier-looking token mid-string is garbage.
        assert!(parse_hotkey("shift+ctrl+o+alt").is_none());
    }
}
