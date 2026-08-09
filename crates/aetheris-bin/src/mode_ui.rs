//! Windows GUI mode of the single `aetheris` binary. On entry [`main`] calls
//! `FreeConsole()` so the dialog detaches from any console; with no console,
//! `eprintln!` output is invisible — startup/fatal errors are surfaced via
//! [`report_error`] (message box + `%TEMP%\aetheris-ui.log`).
//!
//! aetheris-ui: status panel + rule editor + save flow.
//!
//! A programmatic Win32 dialog (no `.rc`, no GUI framework) wired to the
//! running aetheris service over the named pipe:
//!
//! * **Status panel** (top): live mode, boosted-process count/names and the
//!   last-reload result, re-pulled on demand by the Refresh button
//!   (`GetState`).
//! * **Rule editor** (middle): three `SysListView32` lists — game processes,
//!   `[[background]]` rules and `[[rule]]` always-rules. Selecting a background
//!   or rule row loads its fields into the shared editor (name, priority combo,
//!   affinity, qos_cpu_quota, suspend/trim checkboxes). Add/Delete/Apply
//!   mutate a local `Config` copy; Apply writes the editor controls back to the
//!   selected row; "Reload cfg" re-fetches `GetConfig` into the editor (and
//!   clears the startup load error once a fetch succeeds).
//! * **Save / Reload / Exit** (bottom): Save validates the local config and
//!   pushes it to the service via `SaveConfig(local)` — refused while the
//!   startup `GetConfig` failed, so the empty stub config can't overwrite the
//!   real config on disk; Reload asks the service to re-read its config file;
//!   Exit closes the window.
//!
//! Every pipe call (the startup `GetConfig`/`GetState`, Refresh, Save, "Reload
//! cfg", Reload) runs on a detached worker thread: the worker calls
//! [`client_call`] and posts the outcome back as a custom `WM_IPC_RESULT`
//! message whose `wparam` is the call id and whose `lparam` is a
//! `*mut Result<Response, String>` the worker allocates and the wndproc
//! frees. The UI thread never blocks on the pipe, so with the service down the
//! dialog still opens instantly and stays responsive (the retry budget in
//! `client_call` is spent off the UI thread).
//!
//! The dialog state (pipe name, working `Config`, list↔row index maps and
//! control handles) lives in a `UiState` box stored on the window via
//! `SetWindowLongPtrW(GWLP_USERDATA)` and freed on `WM_DESTROY`. Programmatic
//! list mutations set a `busy` flag so the reentrant `LVN_ITEMCHANGED`
//! notification (fired synchronously by `LVM_SETITEMSTATE`) is ignored — it
//! would otherwise mint a second `&mut UiState` while the outer frame's `&mut`
//! is live (two simultaneous `&mut` = UB).
//!
//! Config is loaded once with `GetConfig` on startup; Refresh re-pulls status
//! only (it never overwrites the editor's working copy).

use std::ffi::c_void;
use std::sync::Mutex;

use windows::core::{w, PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    COLORREF, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    CreateBitmap, CreateCompatibleBitmap, CreateCompatibleDC, CreateSolidBrush, DeleteDC,
    DeleteObject, FillRect, GetDC, GetSysColorBrush, ReleaseDC, SelectObject, COLOR_BTNFACE,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{
    InitCommonControlsEx, INITCOMMONCONTROLSEX, ICC_LISTVIEW_CLASSES, BST_CHECKED,
    LVM_DELETEALLITEMS, LVM_GETITEMCOUNT, LVM_GETNEXTITEM, LVM_INSERTCOLUMNW, LVM_INSERTITEMW,
    LVM_SETEXTENDEDLISTVIEWSTYLE, LVM_SETITEMSTATE, LVM_SETITEMTEXTW, LVN_ITEMCHANGED, LVCOLUMNW,
    LVCOLUMNW_MASK, LVITEMW, LVCF_SUBITEM, LVCF_TEXT, LVCF_WIDTH, LVIF_TEXT, LVIS_FOCUSED,
    LVIS_SELECTED, LVNI_SELECTED, LVS_EX_FULLROWSELECT, LVS_REPORT, LVS_SINGLESEL, NMHDR,
};
use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows::Win32::UI::Shell::{
    ShellExecuteW, Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE,
    NIM_MODIFY, NOTIFYICONDATAW, NOTIFY_ICON_MESSAGE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, AppendMenuW, BM_GETCHECK, BM_SETCHECK, BS_AUTOCHECKBOX, CB_ADDSTRING,
    CB_GETCURSEL, CB_SETCURSEL, CBS_DROPDOWNLIST, CreateIconIndirect, CreatePopupMenu,
    CreateWindowExW, CW_USEDEFAULT, DefWindowProcW, DestroyIcon, DestroyMenu, DestroyWindow,
    DispatchMessageW, ES_AUTOHSCROLL, GetCursorPos, GetMessageW, GetWindowLongPtrW, GetWindowTextW,
    GWLP_USERDATA, HICON, HMENU, ICONINFO, IDC_ARROW, IDI_APPLICATION, KillTimer, LoadCursorW,
    LoadIconW, MB_ICONERROR, MB_OK, MESSAGEBOX_STYLE, MessageBoxW, MF_SEPARATOR, MF_STRING, MSG,
    PostMessageW, PostQuitMessage, RegisterClassW, SendMessageW, SetForegroundWindow,
    SetTimer, SetWindowLongPtrW, SetWindowTextW, ShowWindow, SIZE_MINIMIZED, SW_HIDE, SW_SHOW,
    TPM_RETURNCMD, TrackPopupMenu, TranslateMessage, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP,
    WM_COMMAND, WM_CREATE, WM_DESTROY, WM_LBUTTONUP, WM_NOTIFY, WM_RBUTTONUP, WM_SIZE, WM_TIMER,
    WNDCLASSW, WS_BORDER, WS_CHILD, WS_CLIPSIBLINGS, WS_OVERLAPPEDWINDOW, WS_TABSTOP, WS_VISIBLE,
    WS_VSCROLL, WS_EX_CLIENTEDGE, WS_EX_STATICEDGE,
};

use aetheris_core::config::{AffinitySpec, AlwaysRule, BackgroundRule, Config, PriorityClass};
use aetheris_core::ipc::{client_call, ProcessInfo, Request, Response, DEFAULT_PIPE};

// ---------------------------------------------------------------------------
// Control identifiers
// ---------------------------------------------------------------------------
const IDC_STATUS_MODE: isize = 100;
const IDC_STATUS_BOOSTED: isize = 101;
const IDC_STATUS_RELOAD: isize = 102;
const IDC_BTN_REFRESH: isize = 103;

const IDC_LIST_GAME: isize = 110;
const IDC_LIST_BG: isize = 111;
const IDC_LIST_RULE: isize = 112;

const IDC_EDIT_NAME: isize = 120;
const IDC_COMBO_PRIORITY: isize = 121;
const IDC_EDIT_AFFINITY: isize = 122;
const IDC_EDIT_QOS: isize = 123;
const IDC_CHK_SUSPEND: isize = 124;
const IDC_CHK_TRIM: isize = 125;

const IDC_BTN_ADD: isize = 130;
const IDC_BTN_RELOAD_CFG: isize = 131;
const IDC_BTN_DELETE: isize = 132;
const IDC_BTN_APPLY: isize = 133;

const IDC_STATUS_RESULT: isize = 140;
const IDC_BTN_SAVE: isize = 141;
const IDC_BTN_RELOAD: isize = 142;
const IDC_BTN_EXIT: isize = 143;

/// Custom message: a worker thread posts a pipe-call outcome here. `wparam`
/// carries the [`IpcCall`] id, `lparam` a `*mut Result<Response, String>`
/// that the worker allocated and the wndproc takes ownership of (and frees).
/// `WM_APP + 1` sits in the application-defined range, clear of any system
/// message.
const WM_IPC_RESULT: u32 = WM_APP + 1;

/// Notification message the tray icon posts to the dialog's wndproc (`lparam`
/// carries the mouse message — `WM_LBUTTONUP` / `WM_RBUTTONUP`). Registered via
/// `NOTIFYICONDATAW.uCallbackMessage`.
const WM_TRAYICON: u32 = WM_APP + 2;

/// Tray icon identifier (`NOTIFYICONDATAW.uID` / `NIM_MODIFY`).
const TRAY_ICON_ID: u32 = 1;

/// Timer that re-pulls service status for the tray icon (`SetTimer` id, fired
/// every [`TRAY_STATUS_INTERVAL_MS`]).
const TRAY_STATUS_TIMER_ID: usize = 2;
const TRAY_STATUS_INTERVAL_MS: u32 = 5000;

/// Tray popup-menu command ids (`AppendMenuW.uidnewitem`). Their values are
/// matched against `TrackPopupMenu(TPM_RETURNCMD)`'s return in the
/// `WM_TRAYICON` handler.
const IDM_START_SERVICE: isize = 1000;
const IDM_STOP_SERVICE: isize = 1001;
const IDM_TOGGLE_OVERLAY: isize = 1002;
const IDM_OPEN_UI: isize = 1003;
const IDM_EXIT: isize = 1004;

/// Identifies which worker-thread IPC call a `WM_IPC_RESULT` message answers.
/// The worker posts it in `wparam`; the wndproc routes on it to the matching
/// state-update method.
#[derive(Clone, Copy, PartialEq, Eq)]
enum IpcCall {
    /// Startup `GetConfig` — loads the working config, clears `init_error`.
    GetConfig,
    /// `GetState` — startup status pull and the Refresh button.
    GetState,
    /// "Reload cfg" — re-fetch `GetConfig` into the editor.
    ReloadCfg,
    /// "Reload" — ask the service to re-read its config file.
    Reload,
    /// "Save" — `SaveConfig`.
    Save,
}

impl IpcCall {
    fn as_wparam(self) -> usize {
        self as usize
    }

    fn from_wparam(w: usize) -> Option<IpcCall> {
        match w {
            0 => Some(IpcCall::GetConfig),
            1 => Some(IpcCall::GetState),
            2 => Some(IpcCall::ReloadCfg),
            3 => Some(IpcCall::Reload),
            4 => Some(IpcCall::Save),
            _ => None,
        }
    }
}

/// Display order of the priority combo: index 0 is "(default)" = `None`, then
/// the six `PriorityClass` variants in their serde snake_case spelling.
const PRIORITIES: &[(PriorityClass, &str)] = &[
    (PriorityClass::Idle, "idle"),
    (PriorityClass::BelowNormal, "below_normal"),
    (PriorityClass::Normal, "normal"),
    (PriorityClass::AboveNormal, "above_normal"),
    (PriorityClass::High, "high"),
    (PriorityClass::Realtime, "realtime"),
];

/// Which of the three rule lists an editor action targets.
#[derive(Debug, Clone, Copy, PartialEq)]
enum ListKind {
    Game,
    Background,
    Rule,
}

/// Per-window dialog state, stashed in `GWLP_USERDATA` as a heap box.
struct UiState {
    pipe: String,
    cfg: Config,
    /// Set when a `GetConfig`-style load fails (startup) and shown until a
    /// fetch succeeds; while set, Save is refused so the stub config can't
    /// overwrite the real one, and `set_result_if_loaded` keeps the error line
    /// visible.
    init_error: Option<String>,
    /// True once a `GetConfig` has succeeded at least once. The window opens
    /// with a `Config::default()` stub and the real config arrives on a worker
    /// thread, so Save is also refused until this is set (the config may not be
    /// loaded yet even though no error has been reported).
    config_loaded: bool,
    /// True while a `SaveConfig` is in flight on a worker thread. Save is
    /// refused while set, so a second click can't spawn a second worker with a
    /// newer config that the one-shot pipe server applies out of order — the
    /// server persists in *connection* order, not click order, so an older
    /// config could otherwise be written last. Cleared when the `Save`
    /// `WM_IPC_RESULT` is dispatched (success or error).
    save_in_flight: bool,
    /// True while a list is being mutated/rebuild from code. The `WM_NOTIFY`
    /// (`LVN_ITEMCHANGED`) handler returns immediately while this is set, so a
    /// selection change triggered by `list_set_sel`/`LVM_SETITEMSTATE` cannot
    /// re-enter `wndproc` and mint a second `&mut UiState` while the outer
    /// frame's `&mut` is live (which would be two simultaneous `&mut` = UB).
    busy: bool,
    mode: String,
    boosted: Vec<ProcessInfo>,
    last_reload: Option<String>,
    last_result: Option<String>,
    game_row_to_idx: Vec<usize>,
    bg_row_to_idx: Vec<usize>,
    rule_row_to_idx: Vec<usize>,
    active: ListKind,
    cur_row: Option<usize>,
    h_status_mode: HWND,
    h_status_boosted: HWND,
    h_status_reload: HWND,
    h_result: HWND,
    h_list_game: HWND,
    h_list_bg: HWND,
    h_list_rule: HWND,
    h_name: HWND,
    h_prio: HWND,
    h_aff: HWND,
    h_qos: HWND,
    h_suspend: HWND,
    h_trim: HWND,
    /// Tray status icons (owned, freed on `WM_DESTROY` via `DestroyIcon`):
    /// green when the service answers `GetState`, gray when it does not.
    h_icon_green: HICON,
    h_icon_gray: HICON,
    /// True while a tray-status `GetState` probe is in flight on a worker
    /// thread. The `WM_TIMER` tick skips while set so a down service (whose
    /// `client_call` spends its retry budget off-thread) cannot pile up stuck
    /// workers faster than they drain.
    status_probe_in_flight: bool,
}

/// RAII guard for `UiState::busy`. Setting it clears the flag on `drop`, so the
/// reentrancy lock is released on *every* exit path of the guarded method,
/// including early returns and panics. It stores a raw pointer rather than a
/// borrow so the enclosing `&mut UiState` methods can keep running while the
/// guard is alive.
struct BusyGuard(*mut UiState);

impl BusyGuard {
    /// Mark `state` busy and return a guard that unmarks it when dropped.
    ///
    /// # Safety
    /// `state` must be the `&mut UiState` a method is currently executing on;
    /// the guard is dropped before that method returns, so the raw pointer the
    /// guard holds stays valid for its whole lifetime.
    unsafe fn acquire(state: &mut UiState) -> BusyGuard {
        state.busy = true;
        BusyGuard(state as *mut UiState)
    }
}

impl Drop for BusyGuard {
    fn drop(&mut self) {
        // SAFETY: the pointee is the `UiState` the enclosing method holds
        // exclusively and is still alive (the guard drops before it returns).
        unsafe { (*self.0).busy = false }
    }
}

/// Startup payload passed as `CreateWindowExW`'s `lpParam`; consumed by
/// `WM_CREATE` (which runs synchronously inside the call). The config is no
/// longer pre-loaded — the dialog opens immediately on a `Config::default()`
/// stub and the real config is fetched on a worker thread (see [`WM_IPC_RESULT`]).
struct InitData {
    pipe: String,
}

// ---------------------------------------------------------------------------
// Async IPC: worker threads post results back as WM_IPC_RESULT
// ---------------------------------------------------------------------------

/// Registry of IPC result boxes (`Box<Result<Response, String>>`) that worker
/// threads have handed to the UI thread but that have not been delivered to the
/// wndproc yet. Pointers are stored as `usize` because raw pointers are
/// `!Send`/`!Sync` and can't live in a `static`. [`WM_IPC_RESULT`] handling
/// untracks and frees each one; [`ipc_teardown`] frees whatever is left when
/// the dialog is destroyed, so closing the window with a call in flight (e.g.
/// with the service down) never leaks a worker allocation.
struct PendingIpc {
    /// Set once the dialog is being torn down. Workers that observe it reclaim
    /// their own box instead of posting to a dead window.
    ///
    /// Sticky for the process lifetime: this is a single-window tool, so the
    /// flag is never reset — after `WM_DESTROY` no further window exists and no
    /// more posts can legitimately be made.
    destroyed: bool,
    boxes: Vec<usize>,
}

static PENDING_IPC: Mutex<PendingIpc> = Mutex::new(PendingIpc {
    destroyed: false,
    boxes: Vec::new(),
});

fn pending_lock() -> std::sync::MutexGuard<'static, PendingIpc> {
    PENDING_IPC.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Hand a worker's `client_call` outcome to the UI thread by posting
/// `WM_IPC_RESULT`, taking ownership of a freshly boxed copy of `result`. The
/// box is registered in [`PENDING_IPC`] *before* the post so [`ipc_teardown`]
/// can reclaim it if the dialog is closed mid-call.
///
/// Returns `false` (and reclaims the box here) if the dialog is already gone,
/// so nothing leaks.
///
/// # Safety
/// `hwnd` must be the dialog's window (a stale handle makes `PostMessageW`
/// fail, which is handled without touching the box twice).
unsafe fn ipc_post(hwnd: HWND, call_id: usize, result: Result<Response, String>) -> bool {
    let raw = Box::into_raw(Box::new(result)) as isize;
    let destroyed = {
        let mut g = pending_lock();
        if g.destroyed {
            true
        } else {
            g.boxes.push(raw as usize);
            false
        }
    };
    if destroyed {
        // The dialog was already torn down; the message can never be
        // dispatched, so reclaim the box here.
        drop(Box::from_raw(raw as *mut Result<Response, String>));
        return false;
    }
    if PostMessageW(Some(hwnd), WM_IPC_RESULT, WPARAM(call_id), LPARAM(raw)).is_err() {
        // The window handle died between registration and the post. Teardown
        // may already have freed the box — reclaim it only if it is still
        // tracked (otherwise it was torn down and must not be touched again).
        let still_tracked = {
            let mut g = pending_lock();
            let n = g.boxes.len();
            g.boxes.retain(|&p| p != raw as usize);
            g.boxes.len() != n
        };
        if still_tracked {
            drop(Box::from_raw(raw as *mut Result<Response, String>));
        }
        return false;
    }
    true
}

/// Remove `raw` (an `lparam` from `WM_IPC_RESULT`) from [`PENDING_IPC`] and
/// return the owned box, or `None` if it was already reclaimed by
/// [`ipc_teardown`] — a stray queued message for a window that is going away.
///
/// # Safety
/// `raw` must be an address handed to the wndproc by [`ipc_post`].
unsafe fn ipc_reclaim(raw: isize) -> Option<Box<Result<Response, String>>> {
    let tracked = {
        let mut g = pending_lock();
        let n = g.boxes.len();
        g.boxes.retain(|&p| p != raw as usize);
        g.boxes.len() != n
    };
    if !tracked {
        return None;
    }
    Some(Box::from_raw(raw as *mut Result<Response, String>))
}

/// Reclaim every not-yet-delivered result box and mark the registry closed.
/// Called from `WM_DESTROY` so closing the dialog with IPC calls in flight
/// never leaks the workers' allocations.
///
/// # Safety
/// Only call from the UI thread once the dialog is being destroyed.
unsafe fn ipc_teardown() {
    let mut g = pending_lock();
    g.destroyed = true;
    for p in g.boxes.drain(..) {
        drop(Box::from_raw(p as *mut Result<Response, String>));
    }
}

/// Run one pipe request on a detached worker thread and post the outcome back
/// to the dialog as [`WM_IPC_RESULT`]. The UI thread never blocks on the pipe,
/// so with the service down `client_call`'s retry budget is spent off-thread
/// and the dialog stays responsive (and closable) the whole time.
///
/// `hwnd` is not `Send` in the pinned windows crate, so the raw handle value is
/// handed across and the (Copy) handle is rebuilt inside the worker.
fn spawn_worker(hwnd: HWND, call_id: usize, pipe: String, req: Request) {
    let raw_hwnd = hwnd.0 as isize;
    std::thread::spawn(move || {
        // A panic inside client_call must not silently drop the result: surface
        // it as an error message instead (mirrors the wndproc's catch_unwind).
        let result =
            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| client_call(&pipe, &req)))
                .unwrap_or_else(|_| Err("internal error: IPC call panicked".to_string()));
        let hwnd = HWND(raw_hwnd as *mut c_void);
        let _ = unsafe { ipc_post(hwnd, call_id, result) };
    });
}

// ---------------------------------------------------------------------------
// Small Win32 helpers
// ---------------------------------------------------------------------------

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Append a line to `%TEMP%\aetheris-ui.log` (best-effort: a failure to open or
/// write is ignored). The UI mode detaches from its console (`FreeConsole()`),
/// so there is no console for `eprintln!` to reach; the log file is the
/// persistent record of errors that fall outside the message-box path.
fn log_err(msg: &str) {
    use std::io::Write;
    let path = std::env::temp_dir().join("aetheris-ui.log");
    if let Ok(mut f) = std::fs::OpenOptions::new().create(true).append(true).open(path) {
        let _ = writeln!(f, "{msg}");
    }
}

/// Surface a fatal error: append it to [`log_err`]'s log file and show it in a
/// message box. With the console detached (`FreeConsole()`) the dialog is the
/// only visible surface, so startup/argument errors must pop a box rather than
/// `eprintln!` to nothing.
fn report_error(msg: &str) {
    log_err(msg);
    unsafe {
        let wide = to_wide(&format!("aetheris-ui\n\n{msg}"));
        let _ = MessageBoxW(
            None,
            PCWSTR(wide.as_ptr()),
            w!("aetheris-ui"),
            MESSAGEBOX_STYLE(MB_OK.0 | MB_ICONERROR.0),
        );
    }
}

/// Get the boxed `UiState` for the window. Safe only after `WM_CREATE`.
unsafe fn state_mut(hwnd: HWND) -> &'static mut UiState {
    let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
    assert!(p != 0, "aetheris-ui: dialog state missing");
    &mut *(p as *mut UiState)
}

unsafe fn set_text(hwnd: HWND, text: &str) {
    let wide = to_wide(text);
    let _ = SetWindowTextW(hwnd, PCWSTR(wide.as_ptr()));
}

unsafe fn get_text(hwnd: HWND) -> String {
    let mut buf = vec![0u16; 4096];
    let n = GetWindowTextW(hwnd, &mut buf);
    let n = n.max(0) as usize;
    String::from_utf16_lossy(&buf[..n])
}

unsafe fn combo_add(hwnd: HWND, text: &str) {
    let wide = to_wide(text);
    let _ = SendMessageW(
        hwnd,
        CB_ADDSTRING,
        Some(WPARAM(0)),
        Some(LPARAM(wide.as_ptr() as isize)),
    );
}

unsafe fn combo_set_sel(hwnd: HWND, idx: i32) {
    let _ = SendMessageW(hwnd, CB_SETCURSEL, Some(WPARAM(idx as usize)), Some(LPARAM(0)));
}

unsafe fn combo_get_sel(hwnd: HWND) -> i32 {
    SendMessageW(hwnd, CB_GETCURSEL, Some(WPARAM(0)), Some(LPARAM(0))).0 as i32
}

unsafe fn btn_set(hwnd: HWND, on: bool) {
    let _ = SendMessageW(
        hwnd,
        BM_SETCHECK,
        Some(WPARAM(if on { BST_CHECKED.0 as usize } else { 0 })),
        Some(LPARAM(0)),
    );
}

unsafe fn btn_get(hwnd: HWND) -> bool {
    SendMessageW(hwnd, BM_GETCHECK, Some(WPARAM(0)), Some(LPARAM(0))).0 != 0
}

unsafe fn list_add_column(hwnd: HWND, i: i32, title: &str, width: i32) {
    let mut wide = to_wide(title);
    let mut col: LVCOLUMNW = std::mem::zeroed();
    col.mask = LVCOLUMNW_MASK(LVCF_TEXT.0 | LVCF_WIDTH.0 | LVCF_SUBITEM.0);
    col.cx = width;
    col.pszText = PWSTR(wide.as_mut_ptr());
    col.iSubItem = i;
    let _ = SendMessageW(
        hwnd,
        LVM_INSERTCOLUMNW,
        Some(WPARAM(i as usize)),
        Some(LPARAM(&mut col as *mut _ as isize)),
    );
}

unsafe fn list_add_row(hwnd: HWND, cols: &[String]) -> i32 {
    // The documented "append at end" sentinel `iItem = -1` is rejected (returns
    // -1) on this Windows/comctl32 combination, so insert at the current end
    // index explicitly instead.
    let idx = list_count(hwnd);
    let mut wide = to_wide(&cols[0]);
    let mut item: LVITEMW = std::mem::zeroed();
    item.mask = LVIF_TEXT;
    item.iItem = idx;
    item.pszText = PWSTR(wide.as_mut_ptr());
    let row = SendMessageW(
        hwnd,
        LVM_INSERTITEMW,
        Some(WPARAM(0)),
        Some(LPARAM(&mut item as *mut _ as isize)),
    )
    .0 as i32;
    debug_assert!(row == idx, "insert index mismatch");
    for (c, t) in cols.iter().enumerate().skip(1) {
        list_set_cell(hwnd, row, c as i32, t);
    }
    row
}

unsafe fn list_set_cell(hwnd: HWND, row: i32, col: i32, text: &str) {
    let mut wide = to_wide(text);
    let mut item: LVITEMW = std::mem::zeroed();
    item.mask = LVIF_TEXT;
    item.iItem = row;
    item.iSubItem = col;
    item.pszText = PWSTR(wide.as_mut_ptr());
    let _ = SendMessageW(
        hwnd,
        LVM_SETITEMTEXTW,
        Some(WPARAM(0)),
        Some(LPARAM(&mut item as *mut _ as isize)),
    );
}

unsafe fn list_clear(hwnd: HWND) {
    let _ = SendMessageW(hwnd, LVM_DELETEALLITEMS, Some(WPARAM(0)), Some(LPARAM(0)));
}

unsafe fn list_selected(hwnd: HWND) -> Option<i32> {
    let r = SendMessageW(
        hwnd,
        LVM_GETNEXTITEM,
        Some(WPARAM((-1i32) as isize as usize)),
        Some(LPARAM(LVNI_SELECTED as isize)),
    )
    .0 as i32;
    if r < 0 {
        None
    } else {
        Some(r)
    }
}

unsafe fn list_count(hwnd: HWND) -> i32 {
    SendMessageW(hwnd, LVM_GETITEMCOUNT, Some(WPARAM(0)), Some(LPARAM(0))).0 as i32
}

unsafe fn list_set_sel(hwnd: HWND, row: i32) {
    let mut item: LVITEMW = std::mem::zeroed();
    item.state = LVITEM_STATE_SEL_FOCUS;
    item.stateMask = LVITEM_STATE_SEL_FOCUS;
    let _ = SendMessageW(
        hwnd,
        LVM_SETITEMSTATE,
        Some(WPARAM(row as usize)),
        Some(LPARAM(&mut item as *mut _ as isize)),
    );
}

/// `LVIS_SELECTED | LVIS_FOCUSED`, combined by hand (flag newtypes have no
/// `BitOr` impl in the pinned windows crate).
const LVITEM_STATE_SEL_FOCUS: windows::Win32::UI::Controls::LIST_VIEW_ITEM_STATE_FLAGS =
    windows::Win32::UI::Controls::LIST_VIEW_ITEM_STATE_FLAGS(LVIS_SELECTED.0 | LVIS_FOCUSED.0);

/// Create a child control. `style` is the low-level `DWORD` style bitset
/// (the crate's `WINDOW_STYLE` is a wrapper we build from it).
unsafe fn mk_child(
    parent: HWND,
    exstyle: WINDOW_EX_STYLE,
    class: PCWSTR,
    text: PCWSTR,
    style: u32,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
    id: isize,
    hinst: HINSTANCE,
) -> HWND {
    CreateWindowExW(
        exstyle,
        class,
        text,
        WINDOW_STYLE(style),
        x,
        y,
        w,
        h,
        Some(parent),
        Some(HMENU(id as isize as *mut c_void)),
        Some(hinst),
        None,
    )
    .expect("CreateWindowExW child control")
}

// ---------------------------------------------------------------------------
// Tray icon + status menu (Shell_NotifyIcon)
// ---------------------------------------------------------------------------

/// Build the `NOTIFYICONDATAW` for the tray icon: message notifications to
/// [`WM_TRAYICON`], a status [`HICON`], and the "aetheris" tooltip. `hIcon` is
/// swapped per status color via `NIM_MODIFY`; the struct is otherwise reused
/// for `NIM_ADD` / `NIM_DELETE`.
fn tray_data(hwnd: HWND, hicon: HICON) -> NOTIFYICONDATAW {
    let mut data = NOTIFYICONDATAW {
        cbSize: std::mem::size_of::<NOTIFYICONDATAW>() as u32,
        hWnd: hwnd,
        uID: TRAY_ICON_ID,
        uFlags: NIF_MESSAGE | NIF_ICON | NIF_TIP,
        uCallbackMessage: WM_TRAYICON,
        hIcon: hicon,
        ..Default::default()
    };
    let tip: Vec<u16> = "aetheris".encode_utf16().collect();
    // szTip is zero-initialized by Default, so a non-null-terminated copy is a
    // valid null-terminated string.
    data.szTip[..tip.len()].copy_from_slice(&tip);
    data
}

unsafe fn tray_send(msg: NOTIFY_ICON_MESSAGE, hwnd: HWND, hicon: HICON) -> bool {
    let data = tray_data(hwnd, hicon);
    unsafe { Shell_NotifyIconW(msg, &data).as_bool() }
}

unsafe fn tray_add(hwnd: HWND, hicon: HICON) -> bool {
    tray_send(NIM_ADD, hwnd, hicon)
}

unsafe fn tray_modify_icon(hwnd: HWND, hicon: HICON) -> bool {
    tray_send(NIM_MODIFY, hwnd, hicon)
}

unsafe fn tray_delete(hwnd: HWND) -> bool {
    tray_send(NIM_DELETE, hwnd, HICON::default())
}

/// Build a solid 16x16 status [`HICON`] in `rgb`.
///
/// Simplest approach that compiles cleanly on the pinned windows crate: fill a
/// compatible bitmap with a solid brush, wrap it with a fully-opaque
/// monochrome mask (all zero bits), and hand both to `CreateIconIndirect`
/// (which copies the bitmaps into the icon). A flat square is all the tray
/// needs to distinguish "service up" (green) from "service down" (gray). Any
/// GDI failure fails closed to a null icon, which `Shell_NotifyIconW` ignores.
unsafe fn make_status_icon(rgb: COLORREF) -> HICON {
    let hdc = GetDC(None);
    if hdc.0.is_null() {
        return HICON::default();
    }
    let mem = CreateCompatibleDC(Some(hdc));
    let bmp = CreateCompatibleBitmap(hdc, 16, 16);
    if mem.0.is_null() || bmp.0.is_null() {
        if !mem.0.is_null() {
            let _ = DeleteDC(mem);
        }
        if !bmp.0.is_null() {
            let _ = DeleteObject(bmp.into());
        }
        let _ = ReleaseDC(None, hdc);
        return HICON::default();
    }
    let old = SelectObject(mem, bmp.into());
    let brush = CreateSolidBrush(rgb);
    let rc = RECT {
        left: 0,
        top: 0,
        right: 16,
        bottom: 16,
    };
    let _ = FillRect(mem, &rc, brush);
    let _ = SelectObject(mem, old);
    let _ = DeleteObject(brush.into());
    let _ = DeleteDC(mem);
    let _ = ReleaseDC(None, hdc);

    // Opaque monochrome mask (all zero bits) so the color bitmap shows through
    // everywhere.
    let mask = CreateBitmap(16, 16, 1, 1, None);
    let info = ICONINFO {
        fIcon: true.into(),
        xHotspot: 0,
        yHotspot: 0,
        hbmMask: mask,
        hbmColor: bmp,
    };
    let icon = CreateIconIndirect(&info);
    // CreateIconIndirect copies the bitmaps into the icon; ours can go now.
    let _ = DeleteObject(bmp.into());
    let _ = DeleteObject(mask.into());
    icon.unwrap_or_default()
}

/// Bring the dialog to the foreground (left-click on the tray icon, "Open UI",
/// or a restore from the taskbar).
unsafe fn show_and_foreground(hwnd: HWND) {
    let _ = ShowWindow(hwnd, SW_SHOW);
    let _ = SetForegroundWindow(hwnd);
}

/// Launch the service elevated via `ShellExecuteW(runas)`. The service is a
/// separate elevated process, so a UAC prompt appears when the UI is not
/// already elevated. Failures (unresolvable exe, refused launch) are logged;
/// the UI keeps running.
fn start_service() {
    let Some(exe) = std::env::current_exe().ok() else {
        log_err("start service: cannot resolve current exe");
        return;
    };
    let exe_w = to_wide(&exe.to_string_lossy());
    let rc = unsafe {
        ShellExecuteW(
            None,
            w!("runas"),
            PCWSTR(exe_w.as_ptr()),
            w!("service"),
            None,
            SW_SHOW,
        )
    };
    // ShellExecuteW returns a value > 32 on success; <= 32 is an error code.
    if (rc.0 as isize) <= 32 {
        log_err("start service: ShellExecuteW(runas) failed");
    }
}

/// Show the tray popup menu at the cursor and dispatch the chosen command.
///
/// The menu is built per click and destroyed after `TrackPopupMenu` returns.
/// `TPM_RETURNCMD` makes the selected command id the return value (0 = no
/// selection), so no `WM_COMMAND` plumbing is needed.
unsafe fn show_tray_menu(hwnd: HWND) {
    let menu = match CreatePopupMenu() {
        Ok(m) => m,
        Err(_) => return,
    };
    let _ = AppendMenuW(menu, MF_STRING, IDM_START_SERVICE as usize, w!("Start service"));
    let _ = AppendMenuW(menu, MF_STRING, IDM_STOP_SERVICE as usize, w!("Stop service"));
    let _ = AppendMenuW(menu, MF_STRING, IDM_TOGGLE_OVERLAY as usize, w!("Toggle overlay"));
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
    let _ = AppendMenuW(menu, MF_STRING, IDM_OPEN_UI as usize, w!("Open UI"));
    let _ = AppendMenuW(menu, MF_STRING, IDM_EXIT as usize, w!("Exit"));
    let mut pt = POINT::default();
    let _ = GetCursorPos(&mut pt);
    // SetForegroundWindow is the documented prerequisite for a popup menu to
    // dismiss on an outside click.
    let _ = SetForegroundWindow(hwnd);
    let cmd = TrackPopupMenu(menu, TPM_RETURNCMD, pt.x, pt.y, Some(0), hwnd, None);
    let _ = DestroyMenu(menu);
    if cmd.0 != 0 {
        // Same pattern as WM_COMMAND: mint the state borrow only for the arms
        // that need it, after the nested menu loop has fully unwound.
        let s = state_mut(hwnd);
        match cmd.0 as isize {
            IDM_START_SERVICE => start_service(),
            IDM_STOP_SERVICE => s.stop_service(),
            IDM_TOGGLE_OVERLAY => s.toggle_overlay(),
            IDM_OPEN_UI => show_and_foreground(hwnd),
            IDM_EXIT => {
                let _ = DestroyWindow(hwnd);
            }
            _ => {}
        }
    }
}

// ---------------------------------------------------------------------------
// Value formatting / parsing (config <-> editor strings)
// ---------------------------------------------------------------------------

fn fmt_priority(p: &Option<PriorityClass>) -> String {
    match p {
        None => "-".to_string(),
        Some(pc) => PRIORITIES
            .iter()
            .find(|(c, _)| c == pc)
            .map(|(_, s)| s.to_string())
            .unwrap_or_default(),
    }
}

fn fmt_affinity(a: &Option<AffinitySpec>) -> String {
    match a {
        Some(spec) => spec
            .cores
            .iter()
            .map(|c| c.to_string())
            .collect::<Vec<_>>()
            .join(","),
        None => String::new(),
    }
}

/// Combo index -> priority; index 0 is "(default)" = `None`.
fn combo_idx(p: &Option<PriorityClass>) -> i32 {
    match p {
        None => 0,
        Some(prio) => PRIORITIES
            .iter()
            .position(|(pc, _)| pc == prio)
            .map(|i| i as i32 + 1)
            .unwrap_or(0),
    }
}

fn combo_to_priority(idx: i32) -> Option<PriorityClass> {
    if idx <= 0 {
        return None;
    }
    PRIORITIES.get((idx - 1) as usize).map(|(pc, _)| *pc)
}

/// Parse "0,1" (or empty) into an affinity spec. Syntax only: a range
/// check (e.g. core >= 64) is left to `Config::validate` at Save time.
fn parse_affinity(s: &str) -> Result<Option<AffinitySpec>, String> {
    let t = s.trim();
    if t.is_empty() {
        return Ok(None);
    }
    let mut cores: Vec<u8> = Vec::new();
    for part in t.split(',') {
        let p = part.trim();
        if p.is_empty() {
            return Err(format!("affinity: empty core entry in '{s}'"));
        }
        let c: u8 = p
            .parse()
            .map_err(|_| format!("affinity: '{p}' is not a valid core index"))?;
        cores.push(c);
    }
    if cores.is_empty() {
        return Err("affinity: no cores".into());
    }
    Ok(Some(AffinitySpec { cores }))
}

/// Parse the qos_cpu_quota edit as a number (or empty = unset). Range
/// validation (1..=100) happens in `Config::validate`.
fn parse_qos(s: &str) -> Result<Option<u32>, String> {
    let t = s.trim();
    if t.is_empty() {
        return Ok(None);
    }
    let q: u32 = t
        .parse()
        .map_err(|_| format!("qos: '{t}' is not a number"))?;
    Ok(Some(q))
}

// ---------------------------------------------------------------------------
// UiState behaviour
// ---------------------------------------------------------------------------

impl UiState {
    fn list_hwnd(&self, kind: ListKind) -> HWND {
        match kind {
            ListKind::Game => self.h_list_game,
            ListKind::Background => self.h_list_bg,
            ListKind::Rule => self.h_list_rule,
        }
    }

    fn rows_mut(&mut self, kind: ListKind) -> &mut Vec<usize> {
        match kind {
            ListKind::Game => &mut self.game_row_to_idx,
            ListKind::Background => &mut self.bg_row_to_idx,
            ListKind::Rule => &mut self.rule_row_to_idx,
        }
    }

    /// Show the last operation outcome in the bottom status line.
    unsafe fn set_result(&mut self, hwnd: HWND, msg: &str) {
        self.last_result = Some(msg.to_string());
        set_text(self.h_result, msg);
        let _ = hwnd;
    }

    /// `set_result`, but a no-op while the startup config load is still failing:
    /// the `init_error` line must stay visible until a `GetConfig` succeeds, so
    /// background status refreshes can't mask the "stub config" warning.
    unsafe fn set_result_if_loaded(&mut self, hwnd: HWND, msg: &str) {
        if self.init_error.is_none() {
            self.set_result(hwnd, msg);
        }
    }

    unsafe fn update_status(&self, hwnd: HWND) {
        let _ = hwnd;
        set_text(self.h_status_mode, &format!("Mode: {}", self.mode));
        let names = if self.boosted.is_empty() {
            "(none)".to_string()
        } else {
            self.boosted
                .iter()
                .map(|p| format!("{} (pid {})", p.name, p.pid))
                .collect::<Vec<_>>()
                .join(", ")
        };
        set_text(
            self.h_status_boosted,
            &format!("Boosted ({}): {}", self.boosted.len(), names),
        );
        let rel = self.last_reload.as_deref().unwrap_or("(none)");
        set_text(self.h_status_reload, &format!("Last reload: {rel}"));
    }

    /// Kick off an IPC call on a worker thread; the outcome is applied on the
    /// UI thread when the posted [`WM_IPC_RESULT`] is dispatched. Never blocks.
    fn spawn(&self, hwnd: HWND, call: IpcCall, req: Request) {
        spawn_worker(hwnd, call.as_wparam(), self.pipe.clone(), req);
    }

    /// Start a Refresh: re-pull `GetState` on a worker thread.
    unsafe fn start_refresh(&self, hwnd: HWND) {
        self.spawn(hwnd, IpcCall::GetState, Request::GetState);
    }

    /// Kick the tray-status `GetState` probe (driven by [`WM_TIMER`]). Skipped
    /// while a previous probe is still in flight so a down service (whose
    /// `client_call` spends its retry budget on a worker thread) can't pile up
    /// stuck workers. The outcome lands in `on_get_state_result`, which flips
    /// the tray icon green/gray.
    unsafe fn start_status_probe(&mut self, hwnd: HWND) {
        if self.status_probe_in_flight {
            return;
        }
        self.status_probe_in_flight = true;
        self.spawn(hwnd, IpcCall::GetState, Request::GetState);
    }

    /// Set the tray icon's status color via `NIM_MODIFY` (green = service
    /// responding, gray = service down). No-op when the icon was never added.
    unsafe fn update_tray_status(&self, hwnd: HWND, running: bool) {
        let icon = if running {
            self.h_icon_green
        } else {
            self.h_icon_gray
        };
        let _ = tray_modify_icon(hwnd, icon);
    }

    /// Ask the service to stop over the pipe (worker thread, never blocks the
    /// UI). A pipe failure means the service is already down — the desired end
    /// state of a stop — so it is logged, not surfaced.
    fn stop_service(&self) {
        let pipe = self.pipe.clone();
        std::thread::spawn(move || match client_call(&pipe, &Request::StopService) {
            Ok(Response::Reload(m)) => log_err(&format!("stop service: {m}")),
            Ok(_) => log_err("stop service: unexpected response"),
            Err(e) => log_err(&format!("stop service: service not running ({e})")),
        });
    }

    /// Ask the service to toggle the overlay (worker thread, never blocks the
    /// UI). A pipe failure means the service is not running — logged.
    fn toggle_overlay(&self) {
        let pipe = self.pipe.clone();
        std::thread::spawn(move || match client_call(&pipe, &Request::ToggleOverlay) {
            Ok(Response::Reload(m)) => log_err(&format!("toggle overlay: {m}")),
            Ok(_) => log_err("toggle overlay: unexpected response"),
            Err(e) => log_err(&format!("toggle overlay: service not running ({e})")),
        });
    }

    /// Apply a completed `GetState` to the status panel. Never touches the
    /// editor's local config copy, and while `init_error` is set it leaves the
    /// result line untouched so the "config failed to load" warning persists.
    ///
    /// Every `GetState` outcome (startup pull, Refresh button, tray-status
    /// timer) also drives the tray icon: green when the service answered, gray
    /// when it did not.
    unsafe fn on_get_state_result(&mut self, hwnd: HWND, result: Result<Response, String>) {
        // The in-flight tray-status probe (if any) has landed — allow the next
        // WM_TIMER tick to start a fresh one.
        self.status_probe_in_flight = false;
        match result {
            Ok(Response::State(s)) => {
                self.mode = s.mode;
                self.boosted = s.boosted;
                self.last_reload = s.last_reload;
                self.update_tray_status(hwnd, true);
                self.set_result_if_loaded(hwnd, "Status refreshed");
            }
            Ok(_) => {
                self.update_tray_status(hwnd, false);
                self.set_result_if_loaded(hwnd, "Refresh: unexpected response");
            }
            Err(e) => {
                self.update_tray_status(hwnd, false);
                self.set_result_if_loaded(hwnd, &format!("Refresh failed: {e}"));
            }
        }
        self.update_status(hwnd);
    }

    /// Rebuild one list from the local config and refresh its row->index map.
    unsafe fn rebuild_list(&mut self, hwnd: HWND, kind: ListKind) {
        let _ = hwnd;
        let list = self.list_hwnd(kind);
        list_clear(list);
        let mut map: Vec<usize> = Vec::new();
        match kind {
            ListKind::Game => {
                for (i, p) in self.cfg.game.processes.iter().enumerate() {
                    list_add_row(list, &[p.clone()]);
                    map.push(i);
                }
            }
            ListKind::Background => {
                for (i, b) in self.cfg.background.iter().enumerate() {
                    let cols = vec![
                        b.name.clone(),
                        fmt_priority(&b.priority),
                        fmt_affinity(&b.affinity),
                        b.qos_cpu_quota.map(|q| q.to_string()).unwrap_or_default(),
                    ];
                    list_add_row(list, &cols);
                    map.push(i);
                }
            }
            ListKind::Rule => {
                for (i, r) in self.cfg.rule.iter().enumerate() {
                    let cols = vec![
                        r.name.clone(),
                        fmt_priority(&r.priority),
                        fmt_affinity(&r.affinity),
                    ];
                    list_add_row(list, &cols);
                    map.push(i);
                }
            }
        }
        *self.rows_mut(kind) = map;
    }

    /// Rebuild all three lists, then restore the previous selection (or pick
    /// the first background row on first load).
    unsafe fn rebuild_all(&mut self, hwnd: HWND) {
        let _busy = BusyGuard::acquire(&mut *self);
        let (prev_active, prev_row) = (self.active, self.cur_row);
        self.rebuild_list(hwnd, ListKind::Game);
        self.rebuild_list(hwnd, ListKind::Background);
        self.rebuild_list(hwnd, ListKind::Rule);
        let count = list_count(self.list_hwnd(prev_active));
        match prev_row {
            Some(r) if r < count as usize => {
                list_set_sel(self.list_hwnd(prev_active), r as i32);
                self.cur_row = Some(r);
            }
            Some(_) if count > 0 => {
                list_set_sel(self.list_hwnd(prev_active), count - 1);
                self.cur_row = Some((count - 1) as usize);
            }
            Some(_) | None if count > 0 && prev_active == ListKind::Background => {
                list_set_sel(self.list_hwnd(prev_active), 0);
                self.cur_row = Some(0);
            }
            _ => self.cur_row = None,
        }
        self.active = prev_active;
        self.load_fields(hwnd);
    }

    /// Enable/disable the editor controls for the current row kind.
    unsafe fn enable_editor(&self, on: bool) {
        let bg = on && self.active == ListKind::Background;
        let non_game = on && self.active != ListKind::Game;
        let _ = EnableWindow(self.h_name, on.into());
        let _ = EnableWindow(self.h_prio, non_game.into());
        let _ = EnableWindow(self.h_aff, non_game.into());
        let _ = EnableWindow(self.h_qos, bg.into());
        let _ = EnableWindow(self.h_suspend, bg.into());
        let _ = EnableWindow(self.h_trim, bg.into());
    }

    /// Load the currently selected row's fields into the editor controls.
    unsafe fn load_fields(&self, hwnd: HWND) {
        let _ = hwnd;
        let Some(row) = self.cur_row else {
            set_text(self.h_name, "");
            combo_set_sel(self.h_prio, 0);
            set_text(self.h_aff, "");
            set_text(self.h_qos, "");
            btn_set(self.h_suspend, false);
            btn_set(self.h_trim, false);
            self.enable_editor(false);
            return;
        };
        self.enable_editor(true);
        match self.active {
            ListKind::Game => {
                if let Some(p) = self.cfg.game.processes.get(row) {
                    set_text(self.h_name, p);
                }
                // Game rows have no rule fields: clear the disabled controls so
                // stale values from a previously selected background/rule row
                // don't linger in the greyed-out editor.
                combo_set_sel(self.h_prio, 0);
                set_text(self.h_aff, "");
                set_text(self.h_qos, "");
                btn_set(self.h_suspend, false);
                btn_set(self.h_trim, false);
            }
            ListKind::Background => {
                if let Some(&idx) = self.bg_row_to_idx.get(row) {
                    if let Some(b) = self.cfg.background.get(idx) {
                        set_text(self.h_name, &b.name);
                        combo_set_sel(self.h_prio, combo_idx(&b.priority));
                        set_text(self.h_aff, &fmt_affinity(&b.affinity));
                        set_text(
                            self.h_qos,
                            &b.qos_cpu_quota.map(|q| q.to_string()).unwrap_or_default(),
                        );
                        btn_set(self.h_suspend, b.suspend);
                        btn_set(self.h_trim, b.trim_memory);
                    }
                }
            }
            ListKind::Rule => {
                if let Some(&idx) = self.rule_row_to_idx.get(row) {
                    if let Some(r) = self.cfg.rule.get(idx) {
                        set_text(self.h_name, &r.name);
                        combo_set_sel(self.h_prio, combo_idx(&r.priority));
                        set_text(self.h_aff, &fmt_affinity(&r.affinity));
                        set_text(self.h_qos, "");
                        btn_set(self.h_suspend, false);
                        btn_set(self.h_trim, false);
                    }
                }
            }
        }
    }

    /// Write the editor controls back into the selected row's local config and
    /// repaint that list. Returns `false` (with a message) if nothing is
    /// selected or a field fails to parse.
    unsafe fn apply_fields(&mut self, hwnd: HWND) -> bool {
        let _busy = BusyGuard::acquire(&mut *self);
        let Some(row) = self.cur_row else {
            self.set_result(hwnd, "No row selected to apply");
            return false;
        };
        let name = get_text(self.h_name).trim().to_string();
        match self.active {
            ListKind::Game => {
                let Some(&idx) = self.game_row_to_idx.get(row) else {
                    return false;
                };
                if let Some(p) = self.cfg.game.processes.get_mut(idx) {
                    *p = name;
                }
            }
            ListKind::Background => {
                let Some(&idx) = self.bg_row_to_idx.get(row) else {
                    return false;
                };
                let affinity = match parse_affinity(&get_text(self.h_aff)) {
                    Ok(a) => a,
                    Err(e) => {
                        self.set_result(hwnd, &e);
                        return false;
                    }
                };
                let qos = match parse_qos(&get_text(self.h_qos)) {
                    Ok(q) => q,
                    Err(e) => {
                        self.set_result(hwnd, &e);
                        return false;
                    }
                };
                if let Some(b) = self.cfg.background.get_mut(idx) {
                    b.name = name;
                    b.priority = combo_to_priority(combo_get_sel(self.h_prio));
                    b.affinity = affinity;
                    b.qos_cpu_quota = qos;
                    b.suspend = btn_get(self.h_suspend);
                    b.trim_memory = btn_get(self.h_trim);
                }
            }
            ListKind::Rule => {
                let Some(&idx) = self.rule_row_to_idx.get(row) else {
                    return false;
                };
                let affinity = match parse_affinity(&get_text(self.h_aff)) {
                    Ok(a) => a,
                    Err(e) => {
                        self.set_result(hwnd, &e);
                        return false;
                    }
                };
                if let Some(r) = self.cfg.rule.get_mut(idx) {
                    r.name = name;
                    r.priority = combo_to_priority(combo_get_sel(self.h_prio));
                    r.affinity = affinity;
                }
            }
        }
        // Repaint the active list and restore the selection so the new values
        // are visible. The busy guard suppresses the reentrant LVN_ITEMCHANGED,
        // so reload the fields explicitly below.
        self.rebuild_list(hwnd, self.active);
        let n = list_count(self.list_hwnd(self.active));
        if row < n as usize {
            list_set_sel(self.list_hwnd(self.active), row as i32);
        }
        self.load_fields(hwnd);
        self.set_result(hwnd, "Applied");
        true
    }

    /// Add a blank row of the active list's type to the local config.
    unsafe fn add_row(&mut self, hwnd: HWND) {
        let _busy = BusyGuard::acquire(&mut *self);
        let target = self.active;
        match target {
            ListKind::Game => self.cfg.game.processes.push(String::new()),
            ListKind::Background => self.cfg.background.push(BackgroundRule::default()),
            ListKind::Rule => self.cfg.rule.push(AlwaysRule::default()),
        }
        self.rebuild_list(hwnd, target);
        let n = list_count(self.list_hwnd(target));
        if n > 0 {
            let new_row = n - 1;
            list_set_sel(self.list_hwnd(target), new_row);
            self.cur_row = Some(new_row as usize);
        }
        self.active = target;
        self.load_fields(hwnd);
        self.set_result(hwnd, "Added a new row");
    }

    /// Re-fetch the service's config into the editor on a worker thread
    /// ("Reload cfg"). On success the working copy is replaced, any startup
    /// `init_error` is cleared (re-enabling Save), and the lists are rebuilt.
    unsafe fn start_reload_cfg(&self, hwnd: HWND) {
        self.spawn(hwnd, IpcCall::ReloadCfg, Request::GetConfig);
    }

    unsafe fn on_reload_cfg_result(&mut self, hwnd: HWND, result: Result<Response, String>) {
        match result {
            Ok(Response::Config(c)) => self.apply_config(hwnd, c, "Config reloaded from service"),
            Ok(_) => self.set_result(hwnd, "Reload config: unexpected response"),
            Err(e) => self.set_result(hwnd, &format!("Reload config failed: {e}")),
        }
    }

    /// Startup `GetConfig` outcome: swap in the real config, or (on failure)
    /// arm the save-blocked guard and surface the error.
    unsafe fn on_get_config_result(&mut self, hwnd: HWND, result: Result<Response, String>) {
        match result {
            Ok(Response::Config(c)) => {
                self.apply_config(hwnd, c, "Config loaded. Click Refresh for live status.");
            }
            Ok(_) => self.fail_init(hwnd, "GetConfig: unexpected response"),
            Err(e) => self.fail_init(hwnd, &format!("GetConfig failed: {e}")),
        }
    }

    /// Swap in a config fetched from the service: replace the working copy,
    /// mark the config loaded (unblocking Save), clear any startup `init_error`
    /// and rebuild the lists.
    unsafe fn apply_config(&mut self, hwnd: HWND, c: Config, msg: &str) {
        self.config_loaded = true;
        self.cfg = c;
        self.init_error = None;
        self.rebuild_all(hwnd);
        self.set_result(hwnd, msg);
    }

    /// Record a failed config load: arm the save-blocked guard and show the
    /// error (kept visible until a `GetConfig` succeeds).
    unsafe fn fail_init(&mut self, hwnd: HWND, msg: &str) {
        self.init_error = Some(msg.to_string());
        self.set_result(hwnd, msg);
    }

    /// Remove the selected row from the local config.
    unsafe fn delete_row(&mut self, hwnd: HWND) {
        let _busy = BusyGuard::acquire(&mut *self);
        let Some(row) = self.cur_row else {
            self.set_result(hwnd, "No row selected to delete");
            return;
        };
        let idx = match self.active {
            ListKind::Game => self.game_row_to_idx.get(row).copied(),
            ListKind::Background => self.bg_row_to_idx.get(row).copied(),
            ListKind::Rule => self.rule_row_to_idx.get(row).copied(),
        };
        let Some(idx) = idx else { return };
        match self.active {
            ListKind::Game => {
                if idx < self.cfg.game.processes.len() {
                    self.cfg.game.processes.remove(idx);
                }
            }
            ListKind::Background => {
                if idx < self.cfg.background.len() {
                    self.cfg.background.remove(idx);
                }
            }
            ListKind::Rule => {
                if idx < self.cfg.rule.len() {
                    self.cfg.rule.remove(idx);
                }
            }
        }
        self.rebuild_list(hwnd, self.active);
        let n = list_count(self.list_hwnd(self.active));
        self.cur_row = if n > 0 {
            let next = (row as i32).min(n - 1).max(0);
            list_set_sel(self.list_hwnd(self.active), next);
            Some(next as usize)
        } else {
            None
        };
        self.load_fields(hwnd);
        self.set_result(hwnd, "Deleted");
    }

    /// Save: commit the editor fields to the selected row, validate the whole
    /// local config, then push it to the service on a worker thread. Invalid
    /// configs are rejected locally *before* any round-trip (the service
    /// validates again on its side, so an invalid config can never reach the
    /// file).
    ///
    /// If no `GetConfig` has succeeded yet, the local config is a
    /// `Config::default()` stub; saving that stub would overwrite the real
    /// config on disk, so Save is refused until a successful load (startup or
    /// "Reload cfg") — tracked by `init_error`/`config_loaded`.
    unsafe fn do_save(&mut self, hwnd: HWND) {
        if self.save_in_flight {
            self.set_result(hwnd, "Save already in progress");
            return;
        }
        if self.init_error.is_some() || !self.config_loaded {
            self.set_result(
                hwnd,
                "Save blocked: config not loaded from service — click 'Reload cfg' first",
            );
            return;
        }
        if !self.apply_fields(hwnd) {
            return;
        }
        match self.cfg.validate() {
            Err(e) => self.set_result(hwnd, &format!("Config invalid: {e}")),
            Ok(_) => {
                let cfg = self.cfg.clone();
                self.save_in_flight = true;
                self.spawn(hwnd, IpcCall::Save, Request::SaveConfig(cfg));
            }
        }
    }

    unsafe fn on_save_result(&mut self, hwnd: HWND, result: Result<Response, String>) {
        // The one in-flight save has landed (success or error); clear the flag so
        // the next click starts a fresh save.
        self.save_in_flight = false;
        match result {
            Ok(Response::SaveConfig(Ok(m))) => self.set_result(hwnd, &format!("Saved: {m}")),
            Ok(Response::SaveConfig(Err(e))) => self.set_result(hwnd, &format!("Save failed: {e}")),
            Ok(_) => self.set_result(hwnd, "Save: unexpected response"),
            Err(e) => self.set_result(hwnd, &format!("Save failed: {e}")),
        }
    }

    /// Ask the service to re-read its config file from disk (worker thread).
    unsafe fn do_reload(&self, hwnd: HWND) {
        self.spawn(hwnd, IpcCall::Reload, Request::ReloadConfig);
    }

    unsafe fn on_reload_result(&mut self, hwnd: HWND, result: Result<Response, String>) {
        match result {
            Ok(Response::Reload(m)) => {
                self.set_result(hwnd, &format!("Reload queued: {m}"));
                // Pull the fresh snapshot so last_reload reflects the outcome.
                self.start_refresh(hwnd);
            }
            Ok(_) => self.set_result(hwnd, "Reload: unexpected response"),
            Err(e) => self.set_result(hwnd, &format!("Reload failed: {e}")),
        }
    }

    /// Set up listview columns and the priority combo items (called once from
    /// `WM_CREATE` after the controls exist).
    unsafe fn setup_columns(&self) {
        list_add_column(self.h_list_game, 0, "Process", 180);
        list_add_column(self.h_list_bg, 0, "Name", 110);
        list_add_column(self.h_list_bg, 1, "Priority", 85);
        list_add_column(self.h_list_bg, 2, "Affinity", 70);
        list_add_column(self.h_list_bg, 3, "QoS", 45);
        list_add_column(self.h_list_rule, 0, "Name", 110);
        list_add_column(self.h_list_rule, 1, "Priority", 85);
        list_add_column(self.h_list_rule, 2, "Affinity", 70);
        combo_add(self.h_prio, "(default)");
        for &(_, s) in PRIORITIES {
            combo_add(self.h_prio, s);
        }
    }

    /// First paint: rebuild the lists and show a loading placeholder. The
    /// startup `GetConfig`/`GetState` are in flight on worker threads and
    /// replace it (with the real config, or the load error) when they land.
    unsafe fn init_widgets(&mut self, hwnd: HWND) {
        self.rebuild_all(hwnd);
        self.update_status(hwnd);
        self.set_result(hwnd, "Loading config from service...");
    }
}

// ---------------------------------------------------------------------------
// Window procedure
// ---------------------------------------------------------------------------

unsafe extern "system" fn wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // A Rust panic crossing the Win32 callback boundary would otherwise
    // terminate the process with 0xc000041d and no message; catch it so a
    // misbehaving handler degrades instead of crashing the dialog.
    let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match msg {
        WM_CREATE => {
            // Consume the startup payload handed in via lpParam.
            let cs = &*(lparam.0 as *const windows::Win32::UI::WindowsAndMessaging::CREATESTRUCTW);
            let init = Box::from_raw(cs.lpCreateParams as *mut InitData);
            let st = create_state(hwnd, init.pipe);
            SetWindowLongPtrW(
                hwnd,
                GWLP_USERDATA,
                Box::into_raw(Box::new(st)) as isize,
            );
            let s = state_mut(hwnd);
            s.setup_columns();
            s.init_widgets(hwnd);
            // Register the tray icon (gray until the first GetState flips it).
            let _ = tray_add(hwnd, s.h_icon_gray);
            LRESULT(0)
        }

        WM_TRAYICON => {
            // `lparam` carries the mouse message that activated the icon.
            match lparam.0 as u32 {
                WM_LBUTTONUP => show_and_foreground(hwnd),
                WM_RBUTTONUP => show_tray_menu(hwnd),
                _ => {}
            }
            LRESULT(0)
        }

        WM_SIZE => {
            // Minimize-to-tray: hide instead of minimizing so the dialog
            // disappears from the taskbar while the tray icon stays as the
            // only surface.
            if wparam.0 as u32 == SIZE_MINIMIZED {
                let _ = ShowWindow(hwnd, SW_HIDE);
            }
            LRESULT(0)
        }

        WM_TIMER => {
            if wparam.0 == TRAY_STATUS_TIMER_ID {
                let s = state_mut(hwnd);
                s.start_status_probe(hwnd);
            }
            LRESULT(0)
        }

        WM_COMMAND => {
            let id = (wparam.0 & 0xffff) as isize;
            let code = ((wparam.0 >> 16) & 0xffff) as u32;
            let s = state_mut(hwnd);
            match id {
                // The pipe-touching buttons just hand the request to a worker
                // thread and return; the result is applied when the posted
                // WM_IPC_RESULT is dispatched, so the dialog never blocks on a
                // down service.
                IDC_BTN_REFRESH if code == 0 => s.start_refresh(hwnd),
                IDC_BTN_SAVE if code == 0 => s.do_save(hwnd),
                IDC_BTN_RELOAD if code == 0 => s.do_reload(hwnd),
                IDC_BTN_EXIT if code == 0 => {
                    let _ = DestroyWindow(hwnd);
                }
                IDC_BTN_ADD if code == 0 => s.add_row(hwnd),
                IDC_BTN_RELOAD_CFG if code == 0 => s.start_reload_cfg(hwnd),
                IDC_BTN_DELETE if code == 0 => s.delete_row(hwnd),
                IDC_BTN_APPLY if code == 0 => {
                    s.apply_fields(hwnd);
                }
                _ => {}
            }
            LRESULT(0)
        }

        WM_NOTIFY => {
            let nm = &*(lparam.0 as *const NMHDR);
            if nm.code == LVN_ITEMCHANGED {
                let kind = match nm.idFrom as isize {
                    IDC_LIST_GAME => Some(ListKind::Game),
                    IDC_LIST_BG => Some(ListKind::Background),
                    IDC_LIST_RULE => Some(ListKind::Rule),
                    _ => None,
                };
                if let Some(kind) = kind {
                    let s = state_mut(hwnd);
                    // A selection change driven by our own list mutation
                    // (apply/add/delete/rebuild) arrives re-entrantly while the
                    // outer frame already holds `&mut self`. Refuse to handle it
                    // so we don't mint a second `&mut UiState` mid-mutation;
                    // the mutating method loads the fields itself afterwards.
                    if s.busy {
                        return LRESULT(0);
                    }
                    s.active = kind;
                    let list = s.list_hwnd(kind);
                    s.cur_row = list_selected(list).map(|r| r as usize);
                    s.load_fields(hwnd);
                }
            }
            LRESULT(0)
        }

        WM_IPC_RESULT => {
            // Take ownership of the worker's result box (the worker registered
            // the pointer in PENDING_IPC before posting; we untrack and free
            // it). If it is no longer tracked, WM_DESTROY's teardown already
            // freed it because the dialog was closed mid-call — drop the stray
            // message without touching the (now-freed) pointer.
            let Some(boxed) = ipc_reclaim(lparam.0) else {
                return LRESULT(0);
            };
            let result = *boxed;
            let s = state_mut(hwnd);
            match IpcCall::from_wparam(wparam.0) {
                Some(IpcCall::GetConfig) => s.on_get_config_result(hwnd, result),
                Some(IpcCall::GetState) => s.on_get_state_result(hwnd, result),
                Some(IpcCall::ReloadCfg) => s.on_reload_cfg_result(hwnd, result),
                Some(IpcCall::Reload) => s.on_reload_result(hwnd, result),
                Some(IpcCall::Save) => s.on_save_result(hwnd, result),
                // Unknown call id: the box was freed above; nothing to apply.
                None => {}
            }
            LRESULT(0)
        }

        WM_DESTROY => {
            // Free any in-flight worker results before the window state, so
            // closing the dialog (e.g. with the service down and calls still
            // retrying) leaks nothing.
            ipc_teardown();
            let _ = KillTimer(Some(hwnd), TRAY_STATUS_TIMER_ID);
            let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
            if p != 0 {
                // Remove the tray icon and free the status HICONs before the
                // state box (which owns them) is dropped.
                let st = &mut *(p as *mut UiState);
                let _ = tray_delete(hwnd);
                let _ = DestroyIcon(st.h_icon_green);
                let _ = DestroyIcon(st.h_icon_gray);
                drop(Box::from_raw(p as *mut UiState));
                let _ = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            }
            PostQuitMessage(0);
            LRESULT(0)
        }

        _ => DefWindowProcW(hwnd, msg, wparam, lparam),
    }));
    match out {
        Ok(r) => r,
        Err(payload) => {
            let text = payload
                .downcast_ref::<&str>()
                .map(|s| (*s).to_string())
                .or_else(|| payload.downcast_ref::<String>().cloned())
                .unwrap_or_else(|| format!("{payload:?}"));
            report_error(&format!("panic in wndproc (msg {msg:#010x}): {text}"));
            LRESULT(0)
        }
    }
}

/// Create every child control and assemble the initial `UiState`. The config
/// starts as a `Config::default()` stub with `config_loaded` unset: the real
/// config arrives from the startup `GetConfig` worker, which also arms (or
/// clears) the Save guard.
unsafe fn create_state(hwnd: HWND, pipe: String) -> UiState {
    let hinst: HINSTANCE = GetModuleHandleW(None)
        .expect("module handle")
        .into();

    let h_status_mode = mk_child(
        hwnd,
        WINDOW_EX_STYLE::default(),
        w!("Static"),
        w!("Mode: -"),
        WS_CHILD.0 | WS_VISIBLE.0,
        10,
        10,
        460,
        20,
        IDC_STATUS_MODE,
        hinst,
    );
    let h_status_boosted = mk_child(
        hwnd,
        WINDOW_EX_STYLE::default(),
        w!("Static"),
        w!("Boosted: -"),
        WS_CHILD.0 | WS_VISIBLE.0,
        10,
        32,
        760,
        20,
        IDC_STATUS_BOOSTED,
        hinst,
    );
    let h_status_reload = mk_child(
        hwnd,
        WINDOW_EX_STYLE::default(),
        w!("Static"),
        w!("Last reload: -"),
        WS_CHILD.0 | WS_VISIBLE.0,
        10,
        54,
        760,
        16,
        IDC_STATUS_RELOAD,
        hinst,
    );
    mk_child(
        hwnd,
        WINDOW_EX_STYLE::default(),
        w!("Button"),
        w!("Refresh"),
        WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0,
        790,
        12,
        96,
        26,
        IDC_BTN_REFRESH,
        hinst,
    );

    let h_list_game = mk_list(
        hwnd,
        IDC_LIST_GAME,
        10,
        82,
        200,
        228,
        hinst,
    );
    let h_list_bg = mk_list(hwnd, IDC_LIST_BG, 220, 82, 320, 228, hinst);
    let h_list_rule = mk_list(hwnd, IDC_LIST_RULE, 550, 82, 330, 228, hinst);

    mk_child(
        hwnd,
        WINDOW_EX_STYLE::default(),
        w!("Static"),
        w!("Selected rule:"),
        WS_CHILD.0 | WS_VISIBLE.0,
        10,
        318,
        160,
        16,
        0,
        hinst,
    );
    mk_child(
        hwnd,
        WINDOW_EX_STYLE::default(),
        w!("Static"),
        w!("Name"),
        WS_CHILD.0 | WS_VISIBLE.0,
        10,
        340,
        48,
        18,
        0,
        hinst,
    );
    let h_name = mk_child(
        hwnd,
        WS_EX_CLIENTEDGE,
        w!("Edit"),
        PCWSTR::null(),
        WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | WS_BORDER.0 | ES_AUTOHSCROLL as u32,
        60,
        338,
        180,
        24,
        IDC_EDIT_NAME,
        hinst,
    );
    mk_child(
        hwnd,
        WINDOW_EX_STYLE::default(),
        w!("Static"),
        w!("Priority"),
        WS_CHILD.0 | WS_VISIBLE.0,
        250,
        340,
        56,
        18,
        0,
        hinst,
    );
    let h_prio = mk_child(
        hwnd,
        WS_EX_CLIENTEDGE,
        w!("ComboBox"),
        PCWSTR::null(),
        WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | CBS_DROPDOWNLIST as u32 | WS_VSCROLL.0,
        306,
        336,
        140,
        200,
        IDC_COMBO_PRIORITY,
        hinst,
    );
    mk_child(
        hwnd,
        WINDOW_EX_STYLE::default(),
        w!("Static"),
        w!("Affinity"),
        WS_CHILD.0 | WS_VISIBLE.0,
        456,
        340,
        54,
        18,
        0,
        hinst,
    );
    let h_aff = mk_child(
        hwnd,
        WS_EX_CLIENTEDGE,
        w!("Edit"),
        PCWSTR::null(),
        WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | WS_BORDER.0 | ES_AUTOHSCROLL as u32,
        510,
        338,
        92,
        24,
        IDC_EDIT_AFFINITY,
        hinst,
    );
    mk_child(
        hwnd,
        WINDOW_EX_STYLE::default(),
        w!("Static"),
        w!("QoS"),
        WS_CHILD.0 | WS_VISIBLE.0,
        612,
        340,
        42,
        18,
        0,
        hinst,
    );
    let h_qos = mk_child(
        hwnd,
        WS_EX_CLIENTEDGE,
        w!("Edit"),
        PCWSTR::null(),
        WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | WS_BORDER.0 | ES_AUTOHSCROLL as u32,
        654,
        338,
        64,
        24,
        IDC_EDIT_QOS,
        hinst,
    );

    let h_suspend = mk_child(
        hwnd,
        WINDOW_EX_STYLE::default(),
        w!("Button"),
        w!("Suspend"),
        WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | BS_AUTOCHECKBOX as u32,
        10,
        368,
        110,
        20,
        IDC_CHK_SUSPEND,
        hinst,
    );
    let h_trim = mk_child(
        hwnd,
        WINDOW_EX_STYLE::default(),
        w!("Button"),
        w!("Trim"),
        WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | BS_AUTOCHECKBOX as u32,
        130,
        368,
        110,
        20,
        IDC_CHK_TRIM,
        hinst,
    );

    mk_child(
        hwnd,
        WINDOW_EX_STYLE::default(),
        w!("Button"),
        w!("Add"),
        WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0,
        280,
        366,
        74,
        28,
        IDC_BTN_ADD,
        hinst,
    );
    mk_child(
        hwnd,
        WINDOW_EX_STYLE::default(),
        w!("Button"),
        w!("Reload cfg"),
        WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0,
        360,
        366,
        74,
        28,
        IDC_BTN_RELOAD_CFG,
        hinst,
    );
    mk_child(
        hwnd,
        WINDOW_EX_STYLE::default(),
        w!("Button"),
        w!("Delete"),
        WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0,
        440,
        366,
        74,
        28,
        IDC_BTN_DELETE,
        hinst,
    );
    mk_child(
        hwnd,
        WINDOW_EX_STYLE::default(),
        w!("Button"),
        w!("Apply"),
        WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0,
        520,
        366,
        74,
        28,
        IDC_BTN_APPLY,
        hinst,
    );

    let h_result = mk_child(
        hwnd,
        WS_EX_STATICEDGE,
        w!("Static"),
        w!(""),
        WS_CHILD.0 | WS_VISIBLE.0,
        10,
        444,
        560,
        16,
        IDC_STATUS_RESULT,
        hinst,
    );
    mk_child(
        hwnd,
        WINDOW_EX_STYLE::default(),
        w!("Button"),
        w!("Save"),
        WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0,
        620,
        440,
        92,
        30,
        IDC_BTN_SAVE,
        hinst,
    );
    mk_child(
        hwnd,
        WINDOW_EX_STYLE::default(),
        w!("Button"),
        w!("Reload"),
        WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0,
        718,
        440,
        92,
        30,
        IDC_BTN_RELOAD,
        hinst,
    );
    mk_child(
        hwnd,
        WINDOW_EX_STYLE::default(),
        w!("Button"),
        w!("Exit"),
        WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0,
        816,
        440,
        84,
        30,
        IDC_BTN_EXIT,
        hinst,
    );

    // Solid status icons for the tray: green (0x00FF00) = service up, gray
    // (0x808080) = service down. Owned here and freed on WM_DESTROY.
    let h_icon_green = make_status_icon(COLORREF(0x0000FF00));
    let h_icon_gray = make_status_icon(COLORREF(0x00808080));

    UiState {
        pipe,
        cfg: Config::default(),
        init_error: None,
        config_loaded: false,
        save_in_flight: false,
        busy: false,
        mode: String::new(),
        boosted: Vec::new(),
        last_reload: None,
        last_result: None,
        game_row_to_idx: Vec::new(),
        bg_row_to_idx: Vec::new(),
        rule_row_to_idx: Vec::new(),
        active: ListKind::Background,
        cur_row: None,
        h_status_mode,
        h_status_boosted,
        h_status_reload,
        h_result,
        h_list_game,
        h_list_bg,
        h_list_rule,
        h_name,
        h_prio,
        h_aff,
        h_qos,
        h_suspend,
        h_trim,
        h_icon_green,
        h_icon_gray,
        status_probe_in_flight: false,
    }
}

/// Create a report-view `SysListView32` child with full-row selection.
unsafe fn mk_list(parent: HWND, id: isize, x: i32, y: i32, w: i32, h: i32, hinst: HINSTANCE) -> HWND {
    let list = mk_child(
        parent,
        WS_EX_CLIENTEDGE,
        w!("SysListView32"),
        PCWSTR::null(),
        WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | WS_BORDER.0 | WS_VSCROLL.0 | LVS_REPORT
            | LVS_SINGLESEL,
        x,
        y,
        w,
        h,
        id,
        hinst,
    );
    let _ = SendMessageW(
        list,
        LVM_SETEXTENDEDLISTVIEWSTYLE,
        Some(WPARAM(LVS_EX_FULLROWSELECT as usize)),
        Some(LPARAM(LVS_EX_FULLROWSELECT as isize)),
    );
    list
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub fn main(args: Vec<String>) -> i32 {
    // Detach from any console: the UI is a GUI mode, so no console window may
    // appear alongside the dialog. Errors surface via `report_error` (message
    // box + log file) instead of `eprintln!`.
    let _ = unsafe { windows::Win32::System::Console::FreeConsole() };
    run(args)
}

fn run(args: Vec<String>) -> i32 {
    // Parse `--pipe <name>`; default to the service's well-known pipe. The
    // `ui` subcommand word has already been consumed by the dispatcher, so
    // parse the passed slice directly (not `std::env::args()`).
    let mut pipe = DEFAULT_PIPE.to_string();
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--pipe" if i + 1 < args.len() => {
                pipe = args[i + 1].clone();
                i += 2;
            }
            "--pipe" => {
                report_error("--pipe requires a value");
                return 2;
            }
            other => {
                report_error(&format!("unknown argument: {other}"));
                return 2;
            }
        }
    }

    // Make sure the common controls (SysListView32) are registered.
    unsafe {
        let icce = INITCOMMONCONTROLSEX {
            dwSize: std::mem::size_of::<INITCOMMONCONTROLSEX>() as u32,
            dwICC: ICC_LISTVIEW_CLASSES,
        };
        let _ = InitCommonControlsEx(&icce);
    }

    // The dialog opens immediately on a default config; the real config is
    // fetched on a worker thread (spawned below) and applied when the posted
    // WM_IPC_RESULT is dispatched. With the service down the dialog is still
    // responsive — client_call's retry budget is spent off the UI thread, and
    // the load error (if any) surfaces in the result line when it lands.
    let result: windows::core::Result<()> = (|| {
        let hwnd: HWND;
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

            // Size the window so the 900x520 client layout fits exactly.
            let mut rc = RECT {
                left: 0,
                top: 0,
                right: 900,
                bottom: 520,
            };
            let _ = AdjustWindowRectEx(
                &mut rc,
                WS_OVERLAPPEDWINDOW,
                false,
                WINDOW_EX_STYLE::default(),
            );
            let win_w = rc.right - rc.left;
            let win_h = rc.bottom - rc.top;

            // Hand the pipe to the dialog via lpParam (consumed in WM_CREATE).
            let init = Box::into_raw(Box::new(InitData { pipe }));

            hwnd = CreateWindowExW(
                WINDOW_EX_STYLE::default(),
                w!("aetheris_main"),
                w!("aetheris"),
                WS_OVERLAPPEDWINDOW | WS_CLIPSIBLINGS,
                CW_USEDEFAULT,
                CW_USEDEFAULT,
                win_w,
                win_h,
                None,
                None,
                Some(hinstance),
                Some(init as *mut c_void),
            )?;

            let _ = ShowWindow(hwnd, SW_SHOW);

            // Tray status probe: every TRAY_STATUS_INTERVAL_MS the WM_TIMER
            // handler re-pulls GetState on a worker thread and flips the tray
            // icon green/gray. Runs for the whole dialog lifetime so the icon
            // tracks the service even while the window is hidden to the tray.
            let _ = SetTimer(
                Some(hwnd),
                TRAY_STATUS_TIMER_ID,
                TRAY_STATUS_INTERVAL_MS,
                None,
            );
        }

        // Kick off the startup config load and status pull on worker threads;
        // the outcomes are applied on the UI thread when the posted
        // WM_IPC_RESULT messages arrive (the Refresh button re-pulls later).
        unsafe {
            let s = state_mut(hwnd);
            s.spawn(hwnd, IpcCall::GetConfig, Request::GetConfig);
            s.spawn(hwnd, IpcCall::GetState, Request::GetState);
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
    })();

    match result {
        Ok(()) => 0,
        Err(e) => {
            report_error(&format!("{e}"));
            1
        }
    }
}
