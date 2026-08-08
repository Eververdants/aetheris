//! aetheris-ui: status panel + rule editor + save flow.
//!
//! A programmatic Win32 dialog (no `.rc`, no GUI framework) wired to the
//! running aetheris service over the named pipe:
//!
//! * **Status panel** (top): live mode, boosted-process count/names and the
//!   last-reload result, re-pulled on demand by the Refresh button
//!   (`client_call(GetState)`).
//! * **Rule editor** (middle): three `SysListView32` lists — game processes,
//!   `[[background]]` rules and `[[rule]]` always-rules. Selecting a background
//!   or rule row loads its fields into the shared editor (name, priority combo,
//!   affinity, qos_cpu_quota, suspend/trim checkboxes). Add/Delete/Apply
//!   mutate a local `Config` copy; Apply writes the editor controls back to the
//!   selected row; "Reload cfg" re-fetches `GetConfig` into the editor (and
//!   clears the startup load error once a fetch succeeds).
//! * **Save / Reload / Exit** (bottom): Save validates the local config and
//!   pushes it to the service via `client_call(SaveConfig(local))` — refused
//!   while the startup `GetConfig` failed, so the empty stub config can't
//!   overwrite the real config on disk; Reload asks the service to re-read its
//!   config file; Exit closes the window.
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

use windows::core::{w, PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    HINSTANCE, HWND, LPARAM, LRESULT, RECT, WPARAM,
};
use windows::Win32::Graphics::Gdi::{GetSysColorBrush, COLOR_BTNFACE};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::UI::Controls::{
    InitCommonControlsEx, INITCOMMONCONTROLSEX, ICC_LISTVIEW_CLASSES, BST_CHECKED,
    LVM_DELETEALLITEMS, LVM_GETITEMCOUNT, LVM_GETNEXTITEM, LVM_INSERTCOLUMNW, LVM_INSERTITEMW,
    LVM_SETEXTENDEDLISTVIEWSTYLE, LVM_SETITEMSTATE, LVM_SETITEMTEXTW, LVN_ITEMCHANGED, LVCOLUMNW,
    LVCOLUMNW_MASK, LVITEMW, LVCF_SUBITEM, LVCF_TEXT, LVCF_WIDTH, LVIF_TEXT, LVIS_FOCUSED,
    LVIS_SELECTED, LVNI_SELECTED, LVS_EX_FULLROWSELECT, LVS_REPORT, LVS_SINGLESEL, NMHDR,
};
use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, BM_GETCHECK, BM_SETCHECK, BS_AUTOCHECKBOX, CB_ADDSTRING, CB_GETCURSEL,
    CB_SETCURSEL, CBS_DROPDOWNLIST, CreateWindowExW, CW_USEDEFAULT, DefWindowProcW, DestroyWindow,
    DispatchMessageW, ES_AUTOHSCROLL, GetMessageW, GetWindowLongPtrW, GetWindowTextW,
    GWLP_USERDATA, HMENU, IDC_ARROW, IDI_APPLICATION, LoadCursorW, LoadIconW, MSG, PostQuitMessage,
    RegisterClassW, SendMessageW, SetWindowLongPtrW, SetWindowTextW, ShowWindow, SW_SHOW,
    TranslateMessage, WINDOW_EX_STYLE, WINDOW_STYLE, WM_COMMAND, WM_CREATE, WM_DESTROY, WM_NOTIFY,
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
    init_error: Option<String>,
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
/// `WM_CREATE` (which runs synchronously inside the call).
struct InitData {
    pipe: String,
    cfg: Config,
    error: Option<String>,
}

// ---------------------------------------------------------------------------
// Small Win32 helpers
// ---------------------------------------------------------------------------

fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
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

    /// Re-pull `GetState` and repaint the status panel. Never touches the
    /// editor's local config copy, and while `init_error` is set it leaves the
    /// result line untouched so the "config failed to load" warning persists.
    unsafe fn refresh_from_service(&mut self, hwnd: HWND) {
        let pipe = self.pipe.clone();
        match client_call(&pipe, &Request::GetState) {
            Ok(Response::State(s)) => {
                self.mode = s.mode;
                self.boosted = s.boosted;
                self.last_reload = s.last_reload;
                self.set_result_if_loaded(hwnd, "Status refreshed");
            }
            Ok(_) => self.set_result_if_loaded(hwnd, "Refresh: unexpected response"),
            Err(e) => self.set_result_if_loaded(hwnd, &format!("Refresh failed: {e}")),
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

    /// Re-fetch the service's config into the editor. On success the working
    /// copy is replaced, any startup `init_error` is cleared (re-enabling
    /// Save), and the lists are rebuilt. Replaces the former no-op "Edit" row.
    unsafe fn reload_config(&mut self, hwnd: HWND) {
        let pipe = self.pipe.clone();
        match client_call(&pipe, &Request::GetConfig) {
            Ok(Response::Config(c)) => {
                self.cfg = c;
                self.init_error = None;
                self.rebuild_all(hwnd);
                self.set_result(hwnd, "Config reloaded from service");
            }
            Ok(_) => self.set_result(hwnd, "Reload config: unexpected response"),
            Err(e) => self.set_result(hwnd, &format!("Reload config failed: {e}")),
        }
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
    /// local config, then push it to the service. Invalid configs are rejected
    /// locally *before* any round-trip (the service validates again on its side,
    /// so an invalid config can never reach the file).
    ///
    /// If the startup `GetConfig` failed, the local config is a `Config::default()`
    /// stub and `init_error` is set; saving that stub would overwrite the real
    /// config on disk, so Save is refused until a successful `reload_config`.
    unsafe fn do_save(&mut self, hwnd: HWND) {
        if self.init_error.is_some() {
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
                let pipe = self.pipe.clone();
                let cfg = self.cfg.clone();
                match client_call(&pipe, &Request::SaveConfig(cfg)) {
                    Ok(Response::SaveConfig(Ok(m))) => {
                        self.set_result(hwnd, &format!("Saved: {m}"));
                    }
                    Ok(Response::SaveConfig(Err(e))) => {
                        self.set_result(hwnd, &format!("Save failed: {e}"));
                    }
                    Ok(_) => self.set_result(hwnd, "Save: unexpected response"),
                    Err(e) => self.set_result(hwnd, &format!("Save failed: {e}")),
                }
            }
        }
    }

    /// Ask the service to re-read its config file from disk.
    unsafe fn do_reload(&mut self, hwnd: HWND) {
        let pipe = self.pipe.clone();
        match client_call(&pipe, &Request::ReloadConfig) {
            Ok(Response::Reload(m)) => {
                self.set_result(hwnd, &format!("Reload queued: {m}"));
                // Pull the fresh snapshot so last_reload reflects the outcome.
                self.refresh_from_service(hwnd);
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

    /// First paint: rebuild the lists, show startup status, surface any
    /// startup load error.
    unsafe fn init_widgets(&mut self, hwnd: HWND) {
        self.rebuild_all(hwnd);
        self.update_status(hwnd);
        match self.init_error.clone() {
            Some(e) => self.set_result(hwnd, &e),
            None => self.set_result(hwnd, "Ready. Click Refresh for live status."),
        }
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
            let st = create_state(hwnd, *init);
            SetWindowLongPtrW(
                hwnd,
                GWLP_USERDATA,
                Box::into_raw(Box::new(st)) as isize,
            );
            let s = state_mut(hwnd);
            s.setup_columns();
            s.init_widgets(hwnd);
            LRESULT(0)
        }

        WM_COMMAND => {
            let id = (wparam.0 & 0xffff) as isize;
            let code = ((wparam.0 >> 16) & 0xffff) as u32;
            let s = state_mut(hwnd);
            match id {
                IDC_BTN_REFRESH if code == 0 => s.refresh_from_service(hwnd),
                IDC_BTN_SAVE if code == 0 => s.do_save(hwnd),
                IDC_BTN_RELOAD if code == 0 => s.do_reload(hwnd),
                IDC_BTN_EXIT if code == 0 => {
                    let _ = DestroyWindow(hwnd);
                }
                IDC_BTN_ADD if code == 0 => s.add_row(hwnd),
                IDC_BTN_RELOAD_CFG if code == 0 => s.reload_config(hwnd),
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

        WM_DESTROY => {
            let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
            if p != 0 {
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
            eprintln!("aetheris-ui: panic in wndproc (msg {msg:#010x}): {text}");
            LRESULT(0)
        }
    }
}

/// Create every child control and assemble the initial `UiState`.
unsafe fn create_state(hwnd: HWND, init: InitData) -> UiState {
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

    UiState {
        pipe: init.pipe,
        cfg: init.cfg,
        init_error: init.error,
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

fn main() {
    if let Err(e) = run() {
        eprintln!("aetheris-ui: {e}");
        std::process::exit(1);
    }
}

fn run() -> windows::core::Result<()> {
    // Parse `--pipe <name>`; default to the service's well-known pipe.
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
                eprintln!("aetheris-ui: --pipe requires a value");
                std::process::exit(2);
            }
            other => {
                eprintln!("aetheris-ui: unknown argument: {other}");
                std::process::exit(2);
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

    // Load the current config once. If the service is not reachable we still
    // open the editor with an empty config and surface the error in the result
    // line (bounded by client_call's pipe-retry timeout).
    let (mut cfg, mut init_error) = (Config::default(), None);
    match client_call(&pipe, &Request::GetConfig) {
        Ok(Response::Config(c)) => cfg = c,
        Ok(_) => init_error = Some("GetConfig: unexpected response".to_string()),
        Err(e) => init_error = Some(format!("GetConfig failed: {e}")),
    }

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

        // Hand the pipe + config to the dialog via lpParam (consumed in WM_CREATE).
        let init = Box::into_raw(Box::new(InitData {
            pipe,
            cfg,
            error: init_error,
        }));

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
    }

    // Pull live status once after the window is up (Refresh re-pulls later).
    unsafe {
        let s = state_mut(hwnd); // hwnd moved? No — `hwnd` is a Copy handle.
        s.refresh_from_service(hwnd);
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
