//! Windows GUI mode of the single `aetheris` binary. On entry [`main`] calls
//! `FreeConsole()` so the dialog detaches from any console; with no console,
//! `eprintln!` output is invisible — startup/fatal errors are surfaced via
//! [`report_error`] (message box + `%TEMP%\aetheris-ui.log`).
//!
//! aetheris-ui: simplified bilingual checklist main view.
//!
//! A programmatic Win32 dialog (no `.rc`, no GUI framework) wired to the
//! running aetheris service over the named pipe:
//!
//! * **Status area** (top): a status static (`正在优化中 / 未运行` per lang)
//!   plus `[启动服务]` / `[停止服务]` buttons — the same actions the tray menu
//!   exposes, now in-window.
//! * **游戏 list** (middle-left): a single-column `SysListView32` with
//!   `LVS_EX_CHECKBOXES`; rows = `cfg.game.processes`. A checked row means the
//!   process enters GameBoost when it launches. `[从运行中选]` opens the
//!   running-process picker; `[添加]` prompts for a process name (`*` wildcard
//!   allowed).
//! * **后台应用 list**: a single-column checkbox list; rows = `cfg.background`
//!   (checkbox = rule enabled). Same add buttons.
//! * **优化方式 group**: four global toggles — 挂起 (suspend), 降优先级
//!   (below_normal), 限制CPU (with a 低/中/高 combo → 30/50/70) and 清理内存
//!   (trim_memory). On Save these are the default fields applied to every
//!   checked background row; Task 4's advanced editor can override per row.
//! * **保存 / 高级设置** (bottom): Save builds a `Config` from the checklists +
//!   toggles + any per-row overrides in `UiState.bg_overrides` and pushes it via
//!   `SaveConfig`. `[高级设置 ▸]` (collapsed by default) expands the advanced
//!   panel: the per-row editor (name / priority / affinity / QoS / suspend /
//!   trim for the selected background row, plus `Reload 配置` and `Apply`),
//!   writing into `bg_overrides` so per-row divergence survives a Save.
//! * **语言 / Language** button (top-right): cycles `Zh ↔ En`, re-renders every
//!   control via `apply_language` and persists the choice to `ui.toml`
//!   (`aetheris_core::i18n::save_ui_settings`).
//!
//! The running-process picker and the add-by-name prompt are small modal popups
//! (separate top-level windows + a nested filtered message loop, with the main
//! window disabled while they are open). They post their results back to the
//! main window as the custom [`WM_PICK_RESULT`] / [`WM_PROMPT_RESULT`] messages.
//!
//! Every pipe call (the startup `GetConfig`/`GetState`, Save, tray/status
//! probes) runs on a detached worker thread: the worker calls [`client_call`]
//! and posts the outcome back as a custom `WM_IPC_RESULT` message whose `wparam`
//! is the call id and whose `lparam` is a `*mut Result<Response, String>` the
//! worker allocates and the wndproc frees. The UI thread never blocks on the
//! pipe, so with the service down the dialog still opens instantly and stays
//! responsive (the retry budget in `client_call` is spent off the UI thread).
//!
//! The dialog state (pipe name, working `Config`, control handles) lives in a
//! `UiState` box stored on the window via `SetWindowLongPtrW(GWLP_USERDATA)` and
//! freed on `WM_DESTROY`. Programmatic list mutations set a `busy` flag so the
//! reentrant `LVN_ITEMCHANGED` notification (fired synchronously by
//! `LVM_SETITEMSTATE`) is ignored — it would otherwise touch the `UiState` while
//! the outer frame's `&mut` is live (two simultaneous `&mut` = UB).
//!
//! Config is loaded once with `GetConfig` on startup; the tray-status timer
//! re-pulls `GetState` every few seconds for the tray icon's green/gray color.

use std::collections::HashMap;
use std::ffi::c_void;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::Duration;

use windows::core::{w, PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    CloseHandle, COLORREF, GetLastError, HINSTANCE, HWND, LPARAM, LRESULT, POINT, RECT, WPARAM,
};
use windows::Win32::Graphics::Gdi::{
    CreateBitmap, CreateCompatibleBitmap, CreateCompatibleDC, CreateSolidBrush, DeleteDC,
    DeleteObject, FillRect, GetDC, GetSysColorBrush, ReleaseDC, SelectObject, COLOR_BTNFACE,
};
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::ProcessStatus::EnumProcesses;
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Controls::{
    InitCommonControlsEx, INITCOMMONCONTROLSEX, ICC_LISTVIEW_CLASSES, BST_CHECKED,
    LVM_DELETEALLITEMS, LVM_GETITEMCOUNT, LVM_GETITEMSTATE, LVM_GETNEXTITEM, LVM_INSERTCOLUMNW,
    LVM_INSERTITEMW, LVM_SETCOLUMNW, LVM_SETEXTENDEDLISTVIEWSTYLE, LVM_SETITEMSTATE,
    LVM_SETITEMTEXTW, LVN_ITEMCHANGED, LVCOLUMNW, LVCOLUMNW_MASK, LVITEMW, LVCF_SUBITEM,
    LVCF_TEXT, LVCF_WIDTH, LVIF_TEXT, LVIS_SELECTED, LVIS_STATEIMAGEMASK, LVNI_SELECTED,
    LVS_EX_CHECKBOXES, LVS_EX_FULLROWSELECT, LVS_REPORT, NMHDR, NMLISTVIEW,
    LIST_VIEW_ITEM_STATE_FLAGS,
};
use windows::Win32::UI::Input::KeyboardAndMouse::EnableWindow;
use windows::Win32::UI::Shell::{
    ShellExecuteW, Shell_NotifyIconW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE,
    NIM_MODIFY, NOTIFYICONDATAW, NOTIFY_ICON_MESSAGE,
};
use windows::Win32::UI::WindowsAndMessaging::{
    AdjustWindowRectEx, AppendMenuW, BM_GETCHECK, BM_SETCHECK, BS_AUTOCHECKBOX, CB_ADDSTRING,
    CB_GETCURSEL, CB_RESETCONTENT, CB_SETCURSEL, CBS_DROPDOWNLIST, CreateIconIndirect,
    CreatePopupMenu, CreateWindowExW, CW_USEDEFAULT, DefWindowProcW, DestroyIcon, DestroyMenu,
    DestroyWindow, DispatchMessageW, ES_AUTOHSCROLL, GetCursorPos, GetMessageW,
    GetWindowLongPtrW, GetWindowTextW, GWLP_USERDATA, HICON, HMENU, ICONINFO, IDC_ARROW,
    IDI_APPLICATION, IDYES, KillTimer, LoadCursorW, LoadIconW, MB_ICONERROR, MB_ICONQUESTION,
    MB_OK, MB_YESNO, MESSAGEBOX_STYLE, MessageBoxW, MF_SEPARATOR, MF_STRING, MSG, PostMessageW,
    PostQuitMessage, RegisterClassW, SendMessageW, SetForegroundWindow, SetTimer,
    SetWindowLongPtrW, SetWindowTextW, ShowWindow, SIZE_MINIMIZED, SW_HIDE, SW_SHOW,
    TPM_RETURNCMD, TrackPopupMenu, TranslateMessage, WINDOW_EX_STYLE, WINDOW_STYLE, WM_APP,
    WM_CLOSE, WM_COMMAND, WM_CREATE, WM_DESTROY, WM_LBUTTONUP, WM_NOTIFY, WM_RBUTTONUP, WM_SIZE,
    WM_TIMER, WNDCLASSW, WNDPROC, WS_BORDER, WS_CHILD, WS_CLIPSIBLINGS, WS_OVERLAPPEDWINDOW,
    WS_TABSTOP, WS_VISIBLE, WS_VSCROLL, WS_EX_CLIENTEDGE, WS_EX_STATICEDGE,
};

use aetheris_core::config::{AffinitySpec, BackgroundRule, Config, PriorityClass};
use aetheris_core::i18n::{Lang, UiSettings};
use aetheris_core::ipc::{client_call, Request, Response, DEFAULT_PIPE};

// ---------------------------------------------------------------------------
// Bilingual string table (zh/en)
// ---------------------------------------------------------------------------

/// Bilingual UI strings. `tr(lang, key)` returns `key`'s translation in
/// `lang`; every key resolves to a non-empty, non-placeholder string in BOTH
/// languages, enforced by `tests::table_is_complete_for_both_langs`, which
/// walks `keys` (generated from the same match arms, so the two can never
/// drift).
///
/// Keys cover every user-visible string the UI can render: window title,
/// status lines, buttons, list column headers, global options, the running-
/// process picker, the add-by-name prompt, save messages, the tray menu and the
/// advanced editor's field labels (Task 4). `apply_language` drives every
/// control from this table.
macro_rules! define_strings {
    ($( $key:literal => $zh:literal, $en:literal ),* $(,)?) => {
        fn tr(lang: Lang, key: &str) -> &'static str {
            let s: (&'static str, &'static str) = match key {
                $( $key => ($zh, $en), )*
                other => {
                    // A caller asking for a key the table doesn't know is a
                    // programmer error: catch it in debug builds (and in tests)
                    // rather than silently returning a placeholder.
                    debug_assert!(false, "tr: unknown key '{other}'");
                    ("", "")
                }
            };
            match lang {
                Lang::Zh => s.0,
                Lang::En => s.1,
            }
        }

        /// Every key [`tr`] translates, in match-arm order. `keys` is the
        /// canonical list the table-completeness test walks; being generated
        /// from the same arms, it can never miss one.
        #[cfg(test)]
        fn keys() -> &'static [&'static str] {
            &[ $( $key ),* ]
        }
    };
}

define_strings! {
    // Window title (product name — identical in both languages).
    "title" => "aetheris", "aetheris",
    // Status area: service mode.
    "status_running" => "正在优化中", "Optimizing",
    "status_stopped" => "未运行", "Not running",
    // Service lifecycle messages (startup probe).
    "service_starting" => "正在启动服务…", "Starting service…",
    "service_giveup" => "无法启动服务 — 请手动(以管理员身份)运行 `aetheris service`",
        "unable to start service — run `aetheris service` manually (elevated)",
    // Main-view buttons.
    "btn_start" => "启动服务", "Start service",
    "btn_stop" => "停止服务", "Stop service",
    "btn_save" => "保存", "Save",
    "btn_add" => "添加", "Add",
    "btn_pick_running" => "从运行中的进程选择", "Pick from running",
    "btn_advanced" => "高级设置", "Advanced",
    "btn_reload" => "重载", "Reload",
    // List group labels + column headers.
    "list_games" => "游戏(启动时进入优化模式)", "Games (enter optimization on launch)",
    "list_background" => "后台应用(游戏运行时优化)", "Background apps (optimized while a game runs)",
    "list_process_name" => "进程名", "Process name",
    // Global optimization options. This label is the user-facing status note
    // documenting the global model: a Save applies these toggles to every
    // checked background row (Task-4 per-row `bg_overrides` keep their values).
    "opt_group" => "优化方式(应用于全部勾选的后台应用)",
        "Optimization (applies to all checked background apps)",
    "opt_suspend" => "挂起", "Suspend",
    "opt_low_priority" => "降低优先级", "Lower priority",
    "opt_cpu" => "限制 CPU", "Limit CPU",
    "opt_mem" => "清理内存", "Trim memory",
    // CPU-limit levels.
    "cpu_low" => "低", "Low",
    "cpu_med" => "中", "Medium",
    "cpu_high" => "高", "High",
    // Language toggle button (Task 4).
    "lang" => "语言", "Language",
    // Running-process picker dialog.
    "pick_title" => "选择运行中的进程", "Pick running process",
    "pick_hint" => "勾选要添加的进程,然后点击确定", "Check the processes to add, then OK",
    "pick_ok" => "确定", "OK",
    "pick_cancel" => "取消", "Cancel",
    // Add-by-name prompt.
    "add_prompt_title" => "添加进程", "Add process",
    "add_prompt_hint" => "输入进程名(支持 * 通配)", "Enter a process name (* wildcard supported)",
    // Loading / save outcomes.
    "loading" => "正在从服务加载配置…", "Loading config from service…",
    "cfg_loaded" => "已从服务加载配置", "Config loaded from service",
    "cfg_load_failed" => "加载配置失败", "Config load failed",
    "cfg_invalid" => "配置无效", "Config invalid",
    "status_refreshed" => "状态已刷新", "Status refreshed",
    "status_failed" => "状态刷新失败", "Status refresh failed",
    "save_ok" => "保存成功", "Saved",
    "save_failed" => "保存失败", "Save failed",
    "save_in_progress" => "保存正在进行中", "Save already in progress",
    "save_blocked" => "配置尚未从服务加载,无法保存", "Config not loaded from service — cannot save",
    "save_requires_elevation" => "保存需要管理员权限", "Save requires administrator rights",
    "save_relaunch_prompt" => "保存需要管理员权限。\n\n以管理员身份重新启动将关闭本窗口,未保存的更改将丢失。\n\n是否以管理员身份重新启动 aetheris 并重试?",
        "Save requires administrator rights.\n\nRelaunching as administrator will close this editor and any unsaved changes will be lost.\n\nRelaunch aetheris as administrator and try again?",
    "unexpected_response" => "意外的响应", "Unexpected response",
    "added" => "已添加", "Added",
    // Tray popup menu.
    "tray_start" => "启动服务", "Start service",
    "tray_stop" => "停止服务", "Stop service",
    "tray_overlay" => "切换悬浮窗", "Toggle overlay",
    "tray_open" => "打开界面", "Open UI",
    "tray_exit" => "退出", "Exit",
    // Advanced editor (Task 4).
    "advanced_title" => "高级设置", "Advanced settings",
    "field_name" => "名称", "Name",
    "field_priority" => "优先级", "Priority",
    "field_affinity" => "亲和性", "Affinity",
    "field_qos" => "QoS", "QoS",
    "field_suspend" => "挂起", "Suspend",
    "field_trim" => "清理内存", "Trim memory",
    "btn_apply" => "应用", "Apply",
    "adv_no_selection" => "请先在后台应用列表中选择一行", "Select a background-app row first",
    "adv_applied" => "已应用到该行", "Applied to the row",
    "adv_no_name" => "名称不能为空", "Name must not be empty",
    "adv_invalid_affinity" => "亲和性无效", "Invalid affinity",
    "adv_invalid_qos" => "QoS 无效", "Invalid QoS",
    "adv_reloading" => "正在重新加载配置…", "Reloading config…",
    // Priority-class combo labels (PriorityClass -> index 0..=5).
    "prio_idle" => "空闲", "Idle",
    "prio_below_normal" => "低于正常", "Below normal",
    "prio_normal" => "正常", "Normal",
    "prio_above_normal" => "高于正常", "Above normal",
    "prio_high" => "高", "High",
    "prio_realtime" => "实时", "Realtime",
}

/// Re-render the window in `lang`: set the window title, every control's text,
/// the list column headers, the CPU-limit + priority combos, the advanced-panel
/// labels, and the status line, and record the active language (the tray popup,
/// rebuilt fresh on each right-click, reads it).
///
/// Called at startup (after the dialog is created, before it is shown) with the
/// loaded language, and re-run by the language toggle whenever the user
/// switches zh/en. Takes the live `&mut UiState` so callers never mint a second
/// borrow through `state_mut` while one is already on the stack.
///
/// # Safety
/// `hwnd` must be the dialog window with a live [`UiState`] in `GWLP_USERDATA`
/// (i.e. after `WM_CREATE` has run inside `CreateWindowExW`), and `s` must be
/// that state.
unsafe fn apply_language(s: &mut UiState, hwnd: HWND, lang: Lang) {
    // Record the active language first: `show_tray_menu` rebuilds the popup on
    // every right-click from this field, so the tray strings follow the toggle
    // without needing a persistent menu handle to rebuild here.
    s.lang = lang;
    set_text(hwnd, tr(lang, "title"));
    set_text(s.h_btn_start, tr(lang, "btn_start"));
    set_text(s.h_btn_stop, tr(lang, "btn_stop"));
    set_text(s.h_btn_lang, tr(lang, "lang"));
    set_text(s.h_btn_save, tr(lang, "btn_save"));
    set_text(s.h_btn_pick_game, tr(lang, "btn_pick_running"));
    set_text(s.h_btn_add_game, tr(lang, "btn_add"));
    set_text(s.h_btn_pick_bg, tr(lang, "btn_pick_running"));
    set_text(s.h_btn_add_bg, tr(lang, "btn_add"));
    set_text(s.h_label_game, tr(lang, "list_games"));
    set_text(s.h_label_bg, tr(lang, "list_background"));
    set_text(s.h_label_opt, tr(lang, "opt_group"));
    set_text(s.h_opt_suspend, tr(lang, "opt_suspend"));
    set_text(s.h_opt_low_prio, tr(lang, "opt_low_priority"));
    set_text(s.h_opt_cpu, tr(lang, "opt_cpu"));
    set_text(s.h_opt_mem, tr(lang, "opt_mem"));
    // List column headers (both lists show the process-name column).
    list_set_column(s.h_list_game, 0, tr(lang, "list_process_name"), 220);
    list_set_column(s.h_list_bg, 0, tr(lang, "list_process_name"), 220);
    // CPU-limit combo: rebuild the items in the current language and restore
    // the selection.
    let _ = SendMessageW(s.h_combo_cpu, CB_RESETCONTENT, Some(WPARAM(0)), Some(LPARAM(0)));
    for key in ["cpu_low", "cpu_med", "cpu_high"] {
        combo_add(s.h_combo_cpu, tr(lang, key));
    }
    combo_set_sel(s.h_combo_cpu, s.cpu_level as i32);
    // Advanced panel: labels, buttons and the priority combo. The priority
    // selection is restored by index — the combo order is fixed, so a loaded
    // row's priority survives a language switch.
    set_text(s.h_adv_title, tr(lang, "advanced_title"));
    set_text(s.h_adv_lbl_name, tr(lang, "field_name"));
    set_text(s.h_adv_lbl_prio, tr(lang, "field_priority"));
    set_text(s.h_adv_lbl_affinity, tr(lang, "field_affinity"));
    set_text(s.h_adv_lbl_qos, tr(lang, "field_qos"));
    set_text(s.h_adv_suspend, tr(lang, "field_suspend"));
    set_text(s.h_adv_trim, tr(lang, "field_trim"));
    set_text(s.h_adv_reload, tr(lang, "btn_reload"));
    set_text(s.h_adv_apply, tr(lang, "btn_apply"));
    let prio_sel = combo_get_sel(s.h_adv_combo_prio).max(0) as usize;
    let _ = SendMessageW(
        s.h_adv_combo_prio,
        CB_RESETCONTENT,
        Some(WPARAM(0)),
        Some(LPARAM(0)),
    );
    for key in [
        "prio_idle",
        "prio_below_normal",
        "prio_normal",
        "prio_above_normal",
        "prio_high",
        "prio_realtime",
    ] {
        combo_add(s.h_adv_combo_prio, tr(lang, key));
    }
    combo_set_sel(s.h_adv_combo_prio, prio_sel as i32);
    // The advanced button carries the expand/collapse arrow on top of its base
    // label.
    s.set_advanced_button_text();
    // If the panel is open, re-load the selected row so the editor renders in
    // the new language (and the arrow text above is redrawn).
    if s.advanced_expanded {
        s.load_selected_row_into_editor();
    }
    // Refresh the status line in the new language.
    s.update_status(hwnd);
}

// ---------------------------------------------------------------------------
// Control identifiers
// ---------------------------------------------------------------------------

// Status area (top).
const IDC_STATUS: isize = 100;
const IDC_BTN_START: isize = 101;
const IDC_BTN_STOP: isize = 102;
const IDC_BTN_LANG: isize = 103;

// Games + background-apps lists and their labels.
const IDC_LIST_GAME: isize = 110;
const IDC_LIST_BG: isize = 111;
const IDC_LABEL_GAME: isize = 112;
const IDC_LABEL_BG: isize = 113;
const IDC_BTN_PICK_GAME: isize = 120;
const IDC_BTN_ADD_GAME: isize = 121;
const IDC_BTN_PICK_BG: isize = 122;
const IDC_BTN_ADD_BG: isize = 123;

// Optimization group.
const IDC_LABEL_OPT: isize = 130;
const IDC_OPT_SUSPEND: isize = 131;
const IDC_OPT_LOW_PRIO: isize = 132;
const IDC_OPT_CPU: isize = 133;
const IDC_COMBO_CPU: isize = 134;
const IDC_OPT_MEM: isize = 135;

// Result line + save/advanced.
const IDC_STATUS_RESULT: isize = 140;
const IDC_BTN_SAVE: isize = 141;
const IDC_BTN_ADVANCED: isize = 142;

// Advanced panel (per-row editor).
const IDC_ADV_TITLE: isize = 150;
const IDC_ADV_LBL_NAME: isize = 151;
const IDC_ADV_NAME: isize = 152;
const IDC_ADV_LBL_PRIO: isize = 153;
const IDC_ADV_COMBO_PRIO: isize = 154;
const IDC_ADV_LBL_AFFINITY: isize = 155;
const IDC_ADV_AFFINITY: isize = 156;
const IDC_ADV_LBL_QOS: isize = 157;
const IDC_ADV_QOS: isize = 158;
const IDC_ADV_SUSPEND: isize = 159;
const IDC_ADV_TRIM: isize = 160;
const IDC_ADV_RELOAD: isize = 161;
const IDC_ADV_APPLY: isize = 162;

// Modal popup controls (child ids are per-window, so they don't clash with the
// main window's ids).
const IDC_PICK_HINT: isize = 1;
const IDC_PICK_LIST: isize = 2;
const IDC_PICK_OK: isize = 3;
const IDC_PICK_CANCEL: isize = 4;
const IDC_PROMPT_HINT: isize = 5;
const IDC_PROMPT_EDIT: isize = 6;
const IDC_PROMPT_OK: isize = 7;
const IDC_PROMPT_CANCEL: isize = 8;

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

/// Posted by the startup probe worker once it has launched the service via UAC;
/// the dialog shows "starting service…" in the result line.
const WM_SERVICE_START: u32 = WM_APP + 3;

/// Posted by the startup probe worker when a retry probe finds the service up
/// after a UAC launch; the dialog re-runs its startup `GetConfig`/`GetState` so
/// the config editor and Save path unblock without a manual "Reload cfg".
const WM_SERVICE_UP: u32 = WM_APP + 4;

/// Posted by the startup probe worker when its ~5 s retry window closes with
/// the service still down; the dialog replaces the stale "starting service…"
/// line with a manual-launch instruction instead of leaving it stuck.
const WM_SERVICE_GIVEUP: u32 = WM_APP + 5;

/// Posted by the running-process picker with the chosen process names (`lparam`
/// = `*mut Vec<String>`, `wparam` = target [`ListKind`]); the main wndproc
/// appends them to the focused list.
const WM_PICK_RESULT: u32 = WM_APP + 6;

/// Posted by the add-by-name prompt with the typed name (`lparam` = `*mut
/// String`, `wparam` = target [`ListKind`]); the main wndproc appends it.
const WM_PROMPT_RESULT: u32 = WM_APP + 7;

/// One-shot guard so only a single elevated service launch is in flight at a
/// time. Two UI instances, or a tray "Start service" click while the startup
/// probe is mid-retry, must not each call `ShellExecuteW(runas)` — that would
/// double-prompt UAC and double-launch the service. Won with
/// `compare_exchange(false, true)` immediately before the elevated launch and
/// released when the launching probe succeeds or gives up (or a tray launch
/// returns), so any concurrent launcher sees `true` and skips.
static LAUNCH_PENDING: AtomicBool = AtomicBool::new(false);

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
    /// `GetState` — startup status pull, the tray/status probes and the
    /// start/stop button refresh.
    GetState,
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
            2 => Some(IpcCall::Save),
            _ => None,
        }
    }
}

/// Which of the two main lists an add/pick action targets. The value doubles as
/// the `wparam` carried by the picker/prompt result messages.
#[derive(Debug, Clone, Copy, PartialEq)]
enum ListKind {
    Game,
    Background,
}

impl ListKind {
    fn to_usize(self) -> usize {
        match self {
            ListKind::Game => 0,
            ListKind::Background => 1,
        }
    }

    fn from_usize(u: usize) -> Option<ListKind> {
        match u {
            0 => Some(ListKind::Game),
            1 => Some(ListKind::Background),
            _ => None,
        }
    }
}

/// Per-window dialog state, stashed in `GWLP_USERDATA` as a heap box.
struct UiState {
    pipe: String,
    /// The currently active UI language, loaded at startup from ui.toml
    /// (defaulting to the detected system language) and applied via
    /// [`apply_language`]. The tray menu is rebuilt per click from this field;
    /// the language toggle (Task 4) flips it and re-runs [`apply_language`].
    lang: Lang,
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
    /// checkbox toggle triggered by `LVM_SETITEMSTATE` during a rebuild cannot
    /// touch the `UiState` while the outer frame's `&mut` is live.
    busy: bool,
    /// Service mode from the last successful `GetState`. Empty means the
    /// service has not answered — shown as "未运行".
    mode: String,
    /// CPU-limit combo selection (0 = low/30, 1 = medium/50, 2 = high/70).
    /// Survives the `apply_language` rebuild of the combo items.
    cpu_level: usize,
    /// True while the advanced per-row editor is expanded (default: collapsed).
    /// Drives the `[高级设置 ▸/▾]` arrow and which child controls are visible.
    advanced_expanded: bool,
    /// Task 4: per-row background overrides keyed by process name. The
    /// advanced editor writes into this map (keyed by the row's name); the save
    /// mapping reads it — a row with an override uses it wholesale instead of
    /// the global toggles. Cleared on a config re-fetch (`Reload 配置` /
    /// startup), pruned of dropped rows after a Save.
    bg_overrides: HashMap<String, BackgroundRule>,
    h_status: HWND,
    h_btn_start: HWND,
    h_btn_stop: HWND,
    h_result: HWND,
    h_list_game: HWND,
    h_list_bg: HWND,
    h_btn_pick_game: HWND,
    h_btn_add_game: HWND,
    h_btn_pick_bg: HWND,
    h_btn_add_bg: HWND,
    h_label_game: HWND,
    h_label_bg: HWND,
    h_label_opt: HWND,
    h_opt_suspend: HWND,
    h_opt_low_prio: HWND,
    h_opt_cpu: HWND,
    h_combo_cpu: HWND,
    h_opt_mem: HWND,
    h_btn_save: HWND,
    h_btn_advanced: HWND,
    /// Language toggle button (top-right; cycles `Zh ↔ En`).
    h_btn_lang: HWND,
    // Advanced panel (per-row editor; hidden while collapsed). All created in
    // `create_state`, shown/hidden by `show_advanced`.
    h_adv_title: HWND,
    h_adv_lbl_name: HWND,
    h_adv_name: HWND,
    h_adv_lbl_prio: HWND,
    h_adv_combo_prio: HWND,
    h_adv_lbl_affinity: HWND,
    h_adv_affinity: HWND,
    h_adv_lbl_qos: HWND,
    h_adv_qos: HWND,
    h_adv_suspend: HWND,
    h_adv_trim: HWND,
    h_adv_reload: HWND,
    h_adv_apply: HWND,
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

/// Read `UiState::busy` without minting a `&mut`. A reentrant `LVN_ITEMCHANGED`
/// can fire synchronously (via `LVM_SETITEMSTATE`) while the outer wndproc
/// frame already holds `&mut UiState`; reading the flag through a raw pointer
/// avoids creating a second `&mut` to the same memory.
unsafe fn state_is_busy(hwnd: HWND) -> bool {
    let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
    p != 0 && (*(p as *const UiState)).busy
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

/// `INDEXTOSTATEIMAGEMASK(2)` — the listview checkbox column's "checked" state
/// image index, shifted into the state-image bit field.
const LVIS_CHECKED: u32 = 2 << 12;

/// Check / uncheck a listview row's checkbox (`LVM_SETITEMSTATE` with the
/// state-image mask). Setting an item state fires a reentrant `LVN_ITEMCHANGED`;
/// callers run under the `busy` guard.
unsafe fn list_set_checked(hwnd: HWND, row: i32, on: bool) {
    let mut item: LVITEMW = std::mem::zeroed();
    item.state = LIST_VIEW_ITEM_STATE_FLAGS(if on { LVIS_CHECKED } else { 1 << 12 });
    item.stateMask = LIST_VIEW_ITEM_STATE_FLAGS(LVIS_STATEIMAGEMASK.0);
    let _ = SendMessageW(
        hwnd,
        LVM_SETITEMSTATE,
        Some(WPARAM(row as usize)),
        Some(LPARAM(&mut item as *mut _ as isize)),
    );
}

/// Select / deselect a listview row (`LVM_SETITEMSTATE` with the selection
/// mask). Setting the selection fires a reentrant `LVN_ITEMCHANGED`; callers
/// run under the `busy` guard.
unsafe fn list_set_selected(hwnd: HWND, row: i32, on: bool) {
    let mut item: LVITEMW = std::mem::zeroed();
    item.state = LIST_VIEW_ITEM_STATE_FLAGS(if on { LVIS_SELECTED.0 } else { 0 });
    item.stateMask = LIST_VIEW_ITEM_STATE_FLAGS(LVIS_SELECTED.0);
    let _ = SendMessageW(
        hwnd,
        LVM_SETITEMSTATE,
        Some(WPARAM(row as usize)),
        Some(LPARAM(&mut item as *mut _ as isize)),
    );
}

/// Read a listview row's checkbox state (`LVM_GETITEMSTATE`): state-image index
/// 2 = checked.
unsafe fn list_checked(hwnd: HWND, row: i32) -> bool {
    let state = SendMessageW(
        hwnd,
        LVM_GETITEMSTATE,
        Some(WPARAM(row as usize)),
        Some(LPARAM(LVIS_STATEIMAGEMASK.0 as isize)),
    )
    .0 as u32;
    (state >> 12) == 2
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

/// Re-label an existing listview column (`LVM_SETCOLUMNW`). Used by
/// `apply_language` to re-render column headers on a language switch.
unsafe fn list_set_column(hwnd: HWND, i: i32, title: &str, width: i32) {
    let mut wide = to_wide(title);
    let mut col: LVCOLUMNW = std::mem::zeroed();
    col.mask = LVCOLUMNW_MASK(LVCF_TEXT.0 | LVCF_WIDTH.0 | LVCF_SUBITEM.0);
    col.cx = width;
    col.pszText = PWSTR(wide.as_mut_ptr());
    col.iSubItem = i;
    let _ = SendMessageW(
        hwnd,
        LVM_SETCOLUMNW,
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

unsafe fn list_count(hwnd: HWND) -> i32 {
    SendMessageW(hwnd, LVM_GETITEMCOUNT, Some(WPARAM(0)), Some(LPARAM(0))).0 as i32
}

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
    let ok = tray_send(NIM_ADD, hwnd, hicon);
    if !ok {
        log_err("tray: NIM_ADD failed");
    }
    ok
}

unsafe fn tray_modify_icon(hwnd: HWND, hicon: HICON) -> bool {
    let ok = tray_send(NIM_MODIFY, hwnd, hicon);
    if !ok {
        log_err("tray: NIM_MODIFY failed");
    }
    ok
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
    // everywhere. The bits are passed explicitly: CreateBitmap with NULL
    // lpvBits leaves the mask buffer uninitialized (not guaranteed zero), and
    // nonzero mask bits would make icon pixels transparent/speckled. The buffer
    // only needs to live for the CreateBitmap call (which copies the bits);
    // keep it in scope for the whole function anyway.
    let mask_bits = vec![0u8; (16 * 16) / 8];
    let mask = CreateBitmap(16, 16, 1, 1, Some(mask_bits.as_ptr().cast()));
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

/// Launch this exe's `service` subcommand elevated via `ShellExecuteW(runas)`.
/// The service is a separate elevated process, so a UAC prompt appears when the
/// caller is not already elevated. Returns `true` on success (ShellExecuteW
/// returns a value > 32 on success; <= 32 is an error code, e.g. the consent
/// prompt was cancelled).
fn shell_launch_service() -> bool {
    let Some(exe) = std::env::current_exe().ok() else {
        log_err("start service: cannot resolve current exe");
        return false;
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
    let ok = (rc.0 as isize) > 32;
    if !ok {
        log_err("start service: ShellExecuteW(runas) failed");
    }
    ok
}

/// Launch the service elevated (tray menu "Start service", or the in-window
/// `[启动服务]` button). Failures (unresolvable exe, refused launch) are logged;
/// the UI keeps running. While [`LAUNCH_PENDING`] is set (the startup probe's
/// retry window, or another launch already underway), the click is skipped so
/// UAC isn't double-prompted.
fn start_service() {
    if LAUNCH_PENDING
        .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
        .is_err()
    {
        return; // a launch is already in flight; skip the duplicate
    }
    let _ = shell_launch_service();
    LAUNCH_PENDING.store(false, Ordering::Release);
}

/// Startup probe: check once whether the service answers `GetState`; if it is
/// down, launch the service elevated via UAC and retry a few times over ~5 s so
/// the dialog's own startup `GetConfig`/`GetState` stand a chance of landing
/// after the service comes up. Runs on a worker thread — the UI thread never
/// blocks on the pipe (`client_call` against a down service fails fast, and
/// `ShellExecuteW(runas)` blocks on the consent prompt off the UI thread).
///
/// Posts [`WM_SERVICE_START`] once the launch is underway and [`WM_SERVICE_UP`]
/// once a retry succeeds, so the dialog can show "starting service…" and then
/// re-run its config load (the original workers failed while the service was
/// down, and the Refresh button only re-pulls status, not the config).
fn startup_probe(hwnd: HWND, pipe: String) {
    let raw_hwnd = hwnd.0 as isize;
    std::thread::spawn(move || {
        let probe = || client_call(&pipe, &Request::GetState).is_ok();
        if probe() {
            return; // service already up; the normal startup workers handle it
        }
        // Win the single-launch guard before UAC-prompting so a concurrent tray
        // "Start service" or a second UI instance cannot double-launch the
        // service. Held until the probe succeeds or gives up.
        if LAUNCH_PENDING
            .compare_exchange(false, true, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            return; // another launch is already in flight
        }
        if !shell_launch_service() {
            LAUNCH_PENDING.store(false, Ordering::Release); // launch refused/cancelled
            return; // launch refused/cancelled; the dialog reports the pipe error
        }
        let hwnd = HWND(raw_hwnd as *mut c_void);
        let _ = unsafe { PostMessageW(Some(hwnd), WM_SERVICE_START, WPARAM(0), LPARAM(0)) };
        for _ in 0..5 {
            std::thread::sleep(Duration::from_millis(1000));
            if probe() {
                LAUNCH_PENDING.store(false, Ordering::Release); // launched successfully
                let _ = unsafe { PostMessageW(Some(hwnd), WM_SERVICE_UP, WPARAM(0), LPARAM(0)) };
                return;
            }
        }
        // The retry window closed with the service still down. Release the
        // launch guard and replace the stale "starting service…" line with an
        // explicit manual-launch instruction.
        LAUNCH_PENDING.store(false, Ordering::Release);
        let _ = unsafe { PostMessageW(Some(hwnd), WM_SERVICE_GIVEUP, WPARAM(0), LPARAM(0)) };
    });
}

/// Show the tray popup menu at the cursor and dispatch the chosen command.
///
/// The menu is built per click and destroyed after `TrackPopupMenu` returns.
/// `TPM_RETURNCMD` makes the selected command id the return value (0 = no
/// selection), so no `WM_COMMAND` plumbing is needed.
unsafe fn show_tray_menu(hwnd: HWND) {
    // The popup is ephemeral — built per click — so the strings come from the
    // current language (stored on the state by `apply_language`) rather than a
    // persistent menu the toggle would have to rebuild.
    let lang = state_mut(hwnd).lang;
    let menu = match CreatePopupMenu() {
        Ok(m) => m,
        Err(_) => return,
    };
    let start = to_wide(tr(lang, "tray_start"));
    let _ = AppendMenuW(menu, MF_STRING, IDM_START_SERVICE as usize, PCWSTR(start.as_ptr()));
    let stop = to_wide(tr(lang, "tray_stop"));
    let _ = AppendMenuW(menu, MF_STRING, IDM_STOP_SERVICE as usize, PCWSTR(stop.as_ptr()));
    let overlay = to_wide(tr(lang, "tray_overlay"));
    let _ = AppendMenuW(menu, MF_STRING, IDM_TOGGLE_OVERLAY as usize, PCWSTR(overlay.as_ptr()));
    let _ = AppendMenuW(menu, MF_SEPARATOR, 0, None);
    let open = to_wide(tr(lang, "tray_open"));
    let _ = AppendMenuW(menu, MF_STRING, IDM_OPEN_UI as usize, PCWSTR(open.as_ptr()));
    let exit = to_wide(tr(lang, "tray_exit"));
    let _ = AppendMenuW(menu, MF_STRING, IDM_EXIT as usize, PCWSTR(exit.as_ptr()));
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
// Checklist <-> Config mapping
// ---------------------------------------------------------------------------

/// Build the per-row `BackgroundRule` from the global optimization toggles.
///
/// This is the global model: **Save applies these fields to every checked
/// background row** (documented to the user by the [`tr`] `opt_group` status
/// note). A row that has a Task-4 [`UiState::bg_overrides`] entry (keyed by
/// name) is *not* built from these defaults — the save mapping in
/// [`UiState::build_config_from_ui`] uses the override wholesale instead, so a
/// pre-existing divergent config survives via its per-row override rather than
/// being flattened here.
fn bg_rule_from_toggles(
    name: String,
    suspend: bool,
    low_priority: bool,
    cpu_limit: bool,
    cpu_pct: u32,
    trim_memory: bool,
) -> BackgroundRule {
    BackgroundRule {
        name,
        suspend,
        priority: low_priority.then_some(PriorityClass::BelowNormal),
        qos_cpu_quota: cpu_limit.then_some(cpu_pct),
        trim_memory,
        affinity: None,
    }
}

/// CPU-limit combo level → `qos_cpu_quota` percentage (低/中/高 → 30/50/70).
fn cpu_level_to_pct(level: usize) -> u32 {
    match level {
        0 => 30,
        2 => 70,
        _ => 50,
    }
}

/// `qos_cpu_quota` percentage → CPU-limit combo level (30 → 低, 70 → 高,
/// anything else → 中).
fn pct_to_cpu_level(pct: u32) -> usize {
    match pct {
        30 => 0,
        70 => 2,
        _ => 1,
    }
}

/// [`PriorityClass`] → advanced-priority-combo index (the combo order is the
/// enum order: idle … realtime).
fn priority_to_idx(p: PriorityClass) -> usize {
    match p {
        PriorityClass::Idle => 0,
        PriorityClass::BelowNormal => 1,
        PriorityClass::Normal => 2,
        PriorityClass::AboveNormal => 3,
        PriorityClass::High => 4,
        PriorityClass::Realtime => 5,
    }
}

/// Advanced-priority-combo index → [`PriorityClass`]. `None` for an out-of-range
/// index (the combo only ever holds the six enum labels, so this is defensive).
fn idx_to_priority(i: usize) -> Option<PriorityClass> {
    match i {
        0 => Some(PriorityClass::Idle),
        1 => Some(PriorityClass::BelowNormal),
        2 => Some(PriorityClass::Normal),
        3 => Some(PriorityClass::AboveNormal),
        4 => Some(PriorityClass::High),
        5 => Some(PriorityClass::Realtime),
        _ => None,
    }
}

/// Parse the advanced editor's affinity text ("0,1,2") into an [`AffinitySpec`].
/// Empty/whitespace text → `Ok(None)` (no affinity). A non-numeric token, an
/// empty token ("0,,1"), or a core index ≥ 64 → `Err` (the service rejects
/// those, so the editor rejects them too rather than letting Save fail).
fn parse_affinity(text: &str) -> Result<Option<AffinitySpec>, String> {
    let t = text.trim();
    if t.is_empty() {
        return Ok(None);
    }
    let mut cores = Vec::new();
    for part in t.split(',') {
        let part = part.trim();
        if part.is_empty() {
            return Err(format!("empty core in '{t}'"));
        }
        let c: u8 = part.parse().map_err(|_| format!("bad core '{part}'"))?;
        if c >= 64 {
            return Err(format!("core {c} >= 64"));
        }
        cores.push(c);
    }
    if cores.is_empty() {
        return Err(format!("no cores in '{t}'"));
    }
    Ok(Some(AffinitySpec { cores }))
}

/// Parse the advanced editor's QoS text into `qos_cpu_quota`. Empty text →
/// `Ok(None)`; a non-numeric value or a quota outside 1..=100 → `Err`.
fn parse_qos(text: &str) -> Result<Option<u32>, String> {
    let t = text.trim();
    if t.is_empty() {
        return Ok(None);
    }
    let q: u32 = t.parse().map_err(|_| format!("bad number '{t}'"))?;
    if q == 0 || q > 100 {
        return Err(format!("must be 1..=100, got {q}"));
    }
    Ok(Some(q))
}

/// Index of the selected row in a listview, or `None` when nothing is selected.
unsafe fn list_selected_row(hwnd: HWND) -> Option<i32> {
    let idx = SendMessageW(
        hwnd,
        LVM_GETNEXTITEM,
        Some(WPARAM(usize::MAX)),
        Some(LPARAM(LVNI_SELECTED as isize)),
    )
    .0 as i32;
    if idx >= 0 {
        Some(idx)
    } else {
        None
    }
}

// ---------------------------------------------------------------------------
// UiState behaviour
// ---------------------------------------------------------------------------

impl UiState {
    fn list_hwnd(&self, kind: ListKind) -> HWND {
        match kind {
            ListKind::Game => self.h_list_game,
            ListKind::Background => self.h_list_bg,
        }
    }

    /// Show the last operation outcome in the bottom status line.
    unsafe fn set_result(&mut self, hwnd: HWND, msg: &str) {
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

    /// Render the status static: "正在优化中" once the service has answered a
    /// `GetState` (mode non-empty), "未运行" otherwise.
    unsafe fn update_status(&self, hwnd: HWND) {
        let _ = hwnd;
        let lang = self.lang;
        let text = if self.mode.is_empty() {
            tr(lang, "status_stopped").to_string()
        } else {
            tr(lang, "status_running").to_string()
        };
        set_text(self.h_status, &text);
    }

    /// Kick off an IPC call on a worker thread; the outcome is applied on the
    /// UI thread when the posted [`WM_IPC_RESULT`] is dispatched. Never blocks.
    fn spawn(&self, hwnd: HWND, call: IpcCall, req: Request) {
        spawn_worker(hwnd, call.as_wparam(), self.pipe.clone(), req);
    }

    /// Start a refresh: re-pull `GetState` on a worker thread.
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

    /// Apply a completed `GetState` to the status line and tray icon. Never
    /// touches the editor's local config copy, and while `init_error` is set it
    /// leaves the result line untouched so the "config failed to load" warning
    /// persists.
    ///
    /// Every `GetState` outcome (startup pull, Refresh, tray-status timer, the
    /// start/stop buttons' refresh) also drives the tray icon: green when the
    /// service answered, gray when it did not.
    unsafe fn on_get_state_result(&mut self, hwnd: HWND, result: Result<Response, String>) {
        // The in-flight tray-status probe (if any) has landed — allow the next
        // WM_TIMER tick to start a fresh one.
        self.status_probe_in_flight = false;
        match result {
            Ok(Response::State(s)) => {
                self.mode = s.mode;
                self.update_tray_status(hwnd, true);
                self.set_result_if_loaded(hwnd, tr(self.lang, "status_refreshed"));
            }
            Ok(_) => {
                self.mode.clear();
                self.update_tray_status(hwnd, false);
                self.set_result_if_loaded(hwnd, tr(self.lang, "unexpected_response"));
            }
            Err(e) => {
                self.mode.clear();
                self.update_tray_status(hwnd, false);
                self.set_result_if_loaded(
                    hwnd,
                    &format!("{}: {e}", tr(self.lang, "status_failed")),
                );
            }
        }
        self.update_status(hwnd);
    }

    /// Rebuild one list from the local config. Rows map 1:1 to the config
    /// vectors (row *i* of the game list = `cfg.game.processes[i]`, etc.), so a
    /// Save can read the checkboxes straight from the list. Every row starts
    /// checked — a row present in the list is an enabled entry.
    unsafe fn rebuild_list(&mut self, hwnd: HWND, kind: ListKind) {
        let _ = hwnd;
        let list = self.list_hwnd(kind);
        list_clear(list);
        match kind {
            ListKind::Game => {
                for p in &self.cfg.game.processes {
                    let row = list_add_row(list, std::slice::from_ref(p));
                    list_set_checked(list, row, true);
                }
            }
            ListKind::Background => {
                for b in &self.cfg.background {
                    let row = list_add_row(list, std::slice::from_ref(&b.name));
                    list_set_checked(list, row, true);
                }
            }
        }
    }

    /// Append a process `name` to the given list and its local config (no
    /// duplicates), then rebuild that list. Called for both the add-by-name
    /// prompt (`WM_PROMPT_RESULT`) and the running-process picker
    /// (`WM_PICK_RESULT`).
    unsafe fn append_name(&mut self, hwnd: HWND, kind: ListKind, name: &str) {
        let name = name.trim();
        if name.is_empty() {
            return;
        }
        let _busy = BusyGuard::acquire(&mut *self);
        match kind {
            ListKind::Game => {
                if !self.cfg.game.processes.iter().any(|p| p == name) {
                    self.cfg.game.processes.push(name.to_string());
                }
            }
            ListKind::Background => {
                if !self.cfg.background.iter().any(|b| b.name == name) {
                    self.cfg.background.push(BackgroundRule {
                        name: name.to_string(),
                        ..Default::default()
                    });
                }
            }
        }
        self.rebuild_list(hwnd, kind);
        self.set_result(hwnd, &format!("{}: {name}", tr(self.lang, "added")));
    }

    /// Open the modal running-process picker for `kind`.
    ///
    /// `lang` is handed in as a `Copy` so the picker never re-reads the
    /// parent's [`UiState`] via `state_mut(parent)` while the `WM_COMMAND`
    /// frame's `&mut UiState` is live on the stack — the same aliasing hazard
    /// [`state_is_busy`] exists to avoid.
    unsafe fn open_picker(&mut self, hwnd: HWND, kind: ListKind) {
        let hinst: HINSTANCE = GetModuleHandleW(None)
            .expect("module handle")
            .into();
        show_process_picker(hwnd, kind.to_usize() as isize, hinst, self.lang);
    }

    /// Open the modal add-by-name prompt for `kind`.
    ///
    /// Same as [`UiState::open_picker`]: `lang` is passed by value so the
    /// prompt never touches the parent's [`UiState`] under the live `&mut`.
    unsafe fn open_prompt(&mut self, hwnd: HWND, kind: ListKind) {
        let hinst: HINSTANCE = GetModuleHandleW(None)
            .expect("module handle")
            .into();
        show_name_prompt(hwnd, kind.to_usize() as isize, hinst, self.lang);
    }

    /// Build a fresh `Config` from the current UI state: games = checked game
    /// rows; background = checked background rows with the global toggles
    /// applied, except rows that have a Task-4 per-row override in
    /// `bg_overrides` (keyed by name), which use the override's values
    /// wholesale instead; `rule` / `protected_extra` / `network` / `overlay`
    /// carried over unchanged.
    unsafe fn build_config_from_ui(&mut self) -> Config {
        let mut cfg = self.cfg.clone();

        // Games: checked rows only (unchecked game rows are dropped on save).
        let mut processes = Vec::new();
        for row in 0..list_count(self.h_list_game) {
            if list_checked(self.h_list_game, row) {
                if let Some(name) = self.cfg.game.processes.get(row as usize) {
                    processes.push(name.clone());
                }
            }
        }
        cfg.game.processes = processes;

        // Background apps: checked rows become rules built from the global
        // toggles — the global model, documented to the user by the `opt_group`
        // label ("applies to all checked background apps"). A row with a Task-4
        // `bg_overrides` entry (keyed by name) uses the override's per-row
        // values wholesale instead of the global defaults, so divergent rows
        // survive a Save via their override. If no checked row has an override,
        // the toggles legitimately rewrite them all — that is the intended
        // model (see `sync_toggles_from_cfg`).
        let suspend = btn_get(self.h_opt_suspend);
        let low_prio = btn_get(self.h_opt_low_prio);
        let cpu = btn_get(self.h_opt_cpu);
        let cpu_pct = cpu_level_to_pct(combo_get_sel(self.h_combo_cpu).max(0) as usize);
        let trim = btn_get(self.h_opt_mem);
        let mut rules = Vec::new();
        for row in 0..list_count(self.h_list_bg) {
            if !list_checked(self.h_list_bg, row) {
                continue;
            }
            let Some(name) = self.cfg.background.get(row as usize).map(|b| b.name.clone())
            else {
                continue;
            };
            let rule = if let Some(ov) = self.bg_overrides.get(&name) {
                ov.clone()
            } else {
                bg_rule_from_toggles(name, suspend, low_prio, cpu, cpu_pct, trim)
            };
            rules.push(rule);
        }
        cfg.background = rules;
        cfg
    }

    /// Save: build the `Config` from the checklists + toggles, validate it,
    /// then push it to the service on a worker thread. Invalid configs are
    /// rejected locally *before* any round-trip (the service validates again on
    /// its side, so an invalid config can never reach the file).
    ///
    /// If no `GetConfig` has succeeded yet, the local config is a
    /// `Config::default()` stub; saving that stub would overwrite the real
    /// config on disk, so Save is refused until a successful load — tracked by
    /// `init_error`/`config_loaded`.
    unsafe fn do_save(&mut self, hwnd: HWND) {
        if self.save_in_flight {
            self.set_result(hwnd, tr(self.lang, "save_in_progress"));
            return;
        }
        if self.init_error.is_some() || !self.config_loaded {
            self.set_result(hwnd, tr(self.lang, "save_blocked"));
            return;
        }
        let cfg = self.build_config_from_ui();
        match cfg.validate() {
            Err(e) => {
                self.set_result(hwnd, &format!("{}: {e}", tr(self.lang, "cfg_invalid")));
            }
            Ok(_) => {
                // Keep the working copy in sync with what is about to be saved
                // so the lists/toggles stay consistent after the round-trip.
                //
                // `cfg` is the checkbox-filtered build (unchecked rows were
                // dropped), so the lists must be rebuilt from it too: otherwise
                // a second Save reads names by row index against the now-trimmed
                // config while the lists still show the old full row set, and
                // silently drops or wrong-maps checked rows. Rebuild both lists
                // and re-sync the global toggles under the busy guard (the
                // unchecked rows visibly disappear, keeping rows ↔ `cfg` 1:1).
                self.cfg = cfg.clone();
                // Drop overrides for rows that were unchecked (and therefore no
                // longer in the saved config) — an override keyed by a dropped
                // name is stale. Surviving rows keep their overrides: Save's
                // rebuild re-adds every `cfg.background` row checked, so the
                // checked state of survivors is preserved.
                let surviving: Vec<String> = cfg.background.iter().map(|b| b.name.clone()).collect();
                self.bg_overrides.retain(|name, _| surviving.contains(name));
                let _busy = BusyGuard::acquire(&mut *self);
                self.rebuild_list(hwnd, ListKind::Game);
                self.rebuild_list(hwnd, ListKind::Background);
                self.sync_toggles_from_cfg();
                if self.advanced_expanded {
                    self.load_selected_row_into_editor();
                }
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
            Ok(Response::SaveConfig(Ok(m))) => {
                self.set_result(hwnd, &format!("{}: {m}", tr(self.lang, "save_ok")));
            }
            Ok(Response::SaveConfig(Err(e))) => self.on_save_elevation(hwnd, &e),
            Ok(_) => self.set_result(hwnd, tr(self.lang, "unexpected_response")),
            Err(e) => {
                self.set_result(hwnd, &format!("{}: {e}", tr(self.lang, "save_failed")));
            }
        }
    }

    /// A `SaveConfig` the service refused for lack of elevation. If this UI
    /// process is not itself elevated, offer to relaunch as administrator: the
    /// elevated instance connects to the (elevated) service and can save. On
    /// accept, launch `aetheris ui` via `ShellExecuteW(runas)` and quit this
    /// instance (exit 0, the new instance takes over). Declining just surfaces
    /// the service's error.
    unsafe fn on_save_elevation(&mut self, hwnd: HWND, msg: &str) {
        if needs_elevation(msg) && !aetheris_core::actions::is_elevated() {
            let wide = to_wide(tr(self.lang, "save_relaunch_prompt"));
            let rc = MessageBoxW(
                Some(hwnd),
                PCWSTR(wide.as_ptr()),
                w!("aetheris"),
                MESSAGEBOX_STYLE(MB_YESNO.0 | MB_ICONQUESTION.0),
            );
            if rc == IDYES {
                let exe_w = std::env::current_exe()
                    .ok()
                    .map(|e| to_wide(&e.to_string_lossy()));
                let launched = match exe_w {
                    Some(exe_w) => {
                        let rc = unsafe {
                            ShellExecuteW(
                                None,
                                w!("runas"),
                                PCWSTR(exe_w.as_ptr()),
                                w!("ui"),
                                None,
                                SW_SHOW,
                            )
                        };
                        (rc.0 as isize) > 32
                    }
                    None => false,
                };
                if launched {
                    // The new elevated instance takes over; quit this one.
                    PostQuitMessage(0);
                    return;
                }
            }
        }
        self.set_result(
            hwnd,
            &format!("{}: {msg}", tr(self.lang, "save_failed")),
        );
    }

    /// Initialize the global optimization toggles from the loaded config so an
    /// existing config is not silently rewritten on the first Save. Uses the
    /// first background rule as the representative; falls back to defaults (all
    /// off, CPU = medium) when there are no rules. Task 4's advanced editor is
    /// where per-row precision lives.
    ///
    /// These toggles are the global model: **Save applies them to every checked
    /// background row** (the `opt_group` status note says so). A pre-existing
    /// heterogeneous config is therefore flattened by a Save unless the
    /// divergent rows have Task-4 `bg_overrides` entries, which
    /// `build_config_from_ui` uses wholesale (per-row values, including a qos
    /// outside {30,50,70}) instead of these globals — that is how per-row
    /// divergence survives. Deriving from the first rule only is a
    /// representative snapshot, not a merge.
    unsafe fn sync_toggles_from_cfg(&mut self) {
        let default = BackgroundRule::default();
        // Derive the toggles from the first background rule that does NOT have a
        // per-row override. A row with an override carries its own values (which
        // can be far from the globals — e.g. qos outside {30,50,70}), so
        // deriving from it would silently drag the global toggles after a Save;
        // the override-backed row keeps its values via `build_config_from_ui`
        // regardless of what the toggles say. When every row is overridden (or
        // there are none), fall back to the defaults (all off, CPU = medium).
        let b = self
            .cfg
            .background
            .iter()
            .find(|b| !self.bg_overrides.contains_key(&b.name))
            .unwrap_or(&default);
        btn_set(self.h_opt_suspend, b.suspend);
        btn_set(self.h_opt_low_prio, b.priority.is_some());
        let cpu_on = b.qos_cpu_quota.is_some();
        btn_set(self.h_opt_cpu, cpu_on);
        self.cpu_level = b.qos_cpu_quota.map(pct_to_cpu_level).unwrap_or(1);
        combo_set_sel(self.h_combo_cpu, self.cpu_level as i32);
        btn_set(self.h_opt_mem, b.trim_memory);
    }

    /// Set the `[高级设置 ▸/▾]` button label: the base `btn_advanced` string
    /// plus the expand (`▸`) / collapse (`▾`) arrow for the current
    /// [`Self::advanced_expanded`] state.
    unsafe fn set_advanced_button_text(&mut self) {
        let base = tr(self.lang, "btn_advanced");
        let arrow = if self.advanced_expanded { "▾" } else { "▸" };
        set_text(self.h_btn_advanced, &format!("{base} {arrow}"));
    }

    /// Toggle the advanced panel between its hidden (collapsed) and visible
    /// (expanded) state. Collapsing never touches the local `Config` or
    /// `bg_overrides`, so nothing typed is lost by switching views.
    unsafe fn toggle_advanced(&mut self, hwnd: HWND) {
        self.advanced_expanded = !self.advanced_expanded;
        self.show_advanced(self.advanced_expanded);
        self.set_advanced_button_text();
        if self.advanced_expanded {
            let loaded = self.load_selected_row_into_editor();
            if !loaded {
                self.set_result(hwnd, tr(self.lang, "adv_no_selection"));
            }
        }
    }

    /// Show or hide the advanced-panel controls (and the main-view controls
    /// they replace: the background list, its buttons, and the global
    /// optimization group). The games list and the bottom result/save row stay
    /// visible in both states.
    unsafe fn show_advanced(&mut self, expanded: bool) {
        let vis = |hwnd: HWND, on: bool| unsafe {
            let _ = ShowWindow(hwnd, if on { SW_SHOW } else { SW_HIDE });
        };
        for c in [
            self.h_adv_title,
            self.h_adv_lbl_name,
            self.h_adv_name,
            self.h_adv_lbl_prio,
            self.h_adv_combo_prio,
            self.h_adv_lbl_affinity,
            self.h_adv_affinity,
            self.h_adv_lbl_qos,
            self.h_adv_qos,
            self.h_adv_suspend,
            self.h_adv_trim,
            self.h_adv_reload,
            self.h_adv_apply,
        ] {
            vis(c, expanded);
        }
        for c in [
            self.h_label_bg,
            self.h_list_bg,
            self.h_btn_pick_bg,
            self.h_btn_add_bg,
            self.h_label_opt,
            self.h_opt_suspend,
            self.h_opt_low_prio,
            self.h_opt_cpu,
            self.h_combo_cpu,
            self.h_opt_mem,
        ] {
            vis(c, !expanded);
        }
    }

    /// Load the currently selected background row (or the first row when none
    /// is selected) into the advanced editor, merging any per-row override
    /// (`bg_overrides[name]`) over the base rule. Returns `false` when there is
    /// nothing to load (no background rows).
    unsafe fn load_selected_row_into_editor(&mut self) -> bool {
        let n = list_count(self.h_list_bg);
        if n <= 0 {
            return false;
        }
        let row = list_selected_row(self.h_list_bg).unwrap_or(0);
        let Some(base) = self.cfg.background.get(row as usize) else {
            return false;
        };
        let rule = self
            .bg_overrides
            .get(&base.name)
            .cloned()
            .unwrap_or_else(|| base.clone());
        set_text(self.h_adv_name, &rule.name);
        let prio_idx = rule.priority.map(priority_to_idx).unwrap_or(2);
        combo_set_sel(self.h_adv_combo_prio, prio_idx as i32);
        let aff = rule
            .affinity
            .as_ref()
            .map(|a| {
                a.cores
                    .iter()
                    .map(|c| c.to_string())
                    .collect::<Vec<_>>()
                    .join(",")
            })
            .unwrap_or_default();
        set_text(self.h_adv_affinity, &aff);
        let qos = rule.qos_cpu_quota.map(|q| q.to_string()).unwrap_or_default();
        set_text(self.h_adv_qos, &qos);
        btn_set(self.h_adv_suspend, rule.suspend);
        btn_set(self.h_adv_trim, rule.trim_memory);
        true
    }

    /// Apply the advanced editor's current values to the selected background
    /// row, writing a per-row override into `bg_overrides` (keyed by the row's
    /// name). Renames the row when the name field changed, keeping the list
    /// row ↔ `cfg.background` mapping 1:1. Field parse failures are rejected
    /// locally (like the save-side validation) before anything is written.
    unsafe fn apply_advanced(&mut self, hwnd: HWND) {
        let n = list_count(self.h_list_bg);
        let Some(row) = list_selected_row(self.h_list_bg).or(if n > 0 { Some(0) } else { None })
        else {
            self.set_result(hwnd, tr(self.lang, "adv_no_selection"));
            return;
        };
        let Some(old_name) = self.cfg.background.get(row as usize).map(|b| b.name.clone()) else {
            return;
        };
        let name = get_text(self.h_adv_name).trim().to_string();
        if name.is_empty() {
            self.set_result(hwnd, tr(self.lang, "adv_no_name"));
            return;
        }
        let prio_idx = combo_get_sel(self.h_adv_combo_prio).max(0) as usize;
        let affinity = match parse_affinity(&get_text(self.h_adv_affinity)) {
            Ok(a) => a,
            Err(e) => {
                self.set_result(
                    hwnd,
                    &format!("{}: {e}", tr(self.lang, "adv_invalid_affinity")),
                );
                return;
            }
        };
        let qos = match parse_qos(&get_text(self.h_adv_qos)) {
            Ok(q) => q,
            Err(e) => {
                self.set_result(hwnd, &format!("{}: {e}", tr(self.lang, "adv_invalid_qos")));
                return;
            }
        };
        let rule = BackgroundRule {
            name: name.clone(),
            suspend: btn_get(self.h_adv_suspend),
            priority: idx_to_priority(prio_idx),
            affinity,
            qos_cpu_quota: qos,
            trim_memory: btn_get(self.h_adv_trim),
        };
        let renamed = name != old_name;
        if renamed {
            self.cfg.background[row as usize].name = name.clone();
            self.bg_overrides.remove(&old_name);
        }
        self.bg_overrides.insert(name.clone(), rule);
        // Rebuild the list only when the row's NAME changed — the override
        // values live in `bg_overrides`, which Save reads, so they never need a
        // list rebuild. A rebuild clears the selection and re-checks every row,
        // so snapshot both BEFORE it and restore them after: the edited row
        // stays selected (a second Apply targets the same row, not row 0) and
        // rows the user unchecked stay unchecked (instead of being silently
        // re-added on the rebuild).
        if renamed {
            let _busy = BusyGuard::acquire(&mut *self);
            let sel = list_selected_row(self.h_list_bg);
            let mut checked = Vec::with_capacity(n as usize);
            for i in 0..n {
                checked.push(list_checked(self.h_list_bg, i));
            }
            self.rebuild_list(hwnd, ListKind::Background);
            for (i, &on) in checked.iter().enumerate() {
                list_set_checked(self.h_list_bg, i as i32, on);
            }
            // Restore the pre-rebuild selection; when nothing was selected (the
            // apply fell back to row 0) select the edited row so it stays the
            // target of a subsequent Apply.
            list_set_selected(self.h_list_bg, sel.unwrap_or(row), true);
        }
        self.set_result(hwnd, tr(self.lang, "adv_applied"));
    }

    /// "Reload 配置": re-fetch the config from the service (the same
    /// `GetConfig` path as the startup load). `apply_config`, which runs when
    /// the result lands, replaces the working copy, clears pending overrides
    /// (an unsaved override is a local edit — reload discards it) and rebuilds
    /// the lists + toggles.
    unsafe fn reload_advanced(&mut self, hwnd: HWND) {
        self.set_result(hwnd, tr(self.lang, "adv_reloading"));
        self.spawn(hwnd, IpcCall::GetConfig, Request::GetConfig);
    }

    /// Cycle the UI language `Zh ↔ En`, persist it to ui.toml, and re-render
    /// every control (incl. the tray menu, which reads `UiState.lang` per
    /// click). `apply_language` takes `self` rather than minting its own state
    /// borrow, so calling it from a `WM_COMMAND` frame that already holds `s`
    /// cannot create a second `&mut UiState`.
    unsafe fn toggle_language(&mut self, hwnd: HWND) {
        let new = match self.lang {
            Lang::Zh => Lang::En,
            Lang::En => Lang::Zh,
        };
        if let Err(e) = aetheris_core::i18n::save_ui_settings(&UiSettings { lang: new }) {
            log_err(&format!("save ui settings: {e}"));
        }
        apply_language(self, hwnd, new);
    }

    /// Set up listview columns and the CPU-limit combo items (called once from
    /// `WM_CREATE` after the controls exist; `apply_language` re-renders them
    /// with the loaded language before the window is shown).
    unsafe fn setup_columns(&self) {
        list_add_column(self.h_list_game, 0, tr(self.lang, "list_process_name"), 220);
        list_add_column(self.h_list_bg, 0, tr(self.lang, "list_process_name"), 220);
        for key in ["cpu_low", "cpu_med", "cpu_high"] {
            combo_add(self.h_combo_cpu, tr(self.lang, key));
        }
        combo_set_sel(self.h_combo_cpu, 1);
        // Advanced-priority combo: the six PriorityClass labels; selection
        // defaults to Normal. `apply_language` re-renders these before the
        // window is shown, so the placeholder language is only visible inside
        // WM_CREATE.
        for key in [
            "prio_idle",
            "prio_below_normal",
            "prio_normal",
            "prio_above_normal",
            "prio_high",
            "prio_realtime",
        ] {
            combo_add(self.h_adv_combo_prio, tr(self.lang, key));
        }
        combo_set_sel(self.h_adv_combo_prio, 2);
    }

    /// First paint: rebuild the lists, initialize the toggles from the (stub)
    /// config and show a loading placeholder. The startup `GetConfig`/`GetState`
    /// are in flight on worker threads and replace it (with the real config, or
    /// the load error) when they land.
    unsafe fn init_widgets(&mut self, hwnd: HWND) {
        self.rebuild_list(hwnd, ListKind::Game);
        self.rebuild_list(hwnd, ListKind::Background);
        self.sync_toggles_from_cfg();
        self.update_status(hwnd);
        self.set_result(hwnd, tr(self.lang, "loading"));
    }

    /// Startup `GetConfig` outcome: swap in the real config, or (on failure)
    /// arm the save-blocked guard and surface the error.
    unsafe fn on_get_config_result(&mut self, hwnd: HWND, result: Result<Response, String>) {
        match result {
            Ok(Response::Config(c)) => self.apply_config(hwnd, c, tr(self.lang, "cfg_loaded")),
            Ok(_) => self.fail_init(hwnd, tr(self.lang, "unexpected_response")),
            Err(e) => {
                self.fail_init(hwnd, &format!("{}: {e}", tr(self.lang, "cfg_load_failed")));
            }
        }
    }

    /// Swap in a config fetched from the service: replace the working copy,
    /// mark the config loaded (unblocking Save), clear any startup `init_error`
    /// and rebuild the lists + toggles.
    unsafe fn apply_config(&mut self, hwnd: HWND, c: Config, msg: &str) {
        self.config_loaded = true;
        self.cfg = c;
        self.init_error = None;
        // A config re-fetch (startup, WM_SERVICE_UP, or the advanced "Reload
        // 配置" button) re-reads the on-disk config, so any not-yet-saved
        // per-row overrides — local editor state only — are discarded.
        self.bg_overrides.clear();
        let _busy = BusyGuard::acquire(&mut *self);
        self.rebuild_list(hwnd, ListKind::Game);
        self.rebuild_list(hwnd, ListKind::Background);
        self.sync_toggles_from_cfg();
        if self.advanced_expanded {
            self.load_selected_row_into_editor();
        }
        self.set_result(hwnd, msg);
    }

    /// Record a failed config load: arm the save-blocked guard and show the
    /// error (kept visible until a `GetConfig` succeeds).
    unsafe fn fail_init(&mut self, hwnd: HWND, msg: &str) {
        self.init_error = Some(msg.to_string());
        self.set_result(hwnd, msg);
    }
}

// ---------------------------------------------------------------------------
// Running-process picker + add-by-name prompt (modal popups)
// ---------------------------------------------------------------------------

/// List distinct running process image names via `EnumProcesses` +
/// `QueryFullProcessImageNameW`, sorted and deduplicated. On any failure
/// returns an empty list.
unsafe fn enumerate_processes() -> Vec<String> {
    let mut pids = vec![0u32; 4096];
    let mut needed = 0u32;
    let mut set: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    if EnumProcesses(
        pids.as_mut_ptr(),
        (pids.len() * std::mem::size_of::<u32>()) as u32,
        &mut needed,
    )
    .is_err()
    {
        return Vec::new();
    }
    let count = ((needed as usize) / std::mem::size_of::<u32>()).min(pids.len());
    for &pid in &pids[..count] {
        if pid == 0 {
            continue;
        }
        // PROCESS_QUERY_LIMITED_INFORMATION is enough for the image name and is
        // available to non-elevated callers for other users' processes.
        let Ok(h) = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) else {
            continue;
        };
        let mut buf = vec![0u16; 260];
        let mut sz = buf.len() as u32;
        let ok =
            QueryFullProcessImageNameW(h, PROCESS_NAME_WIN32, PWSTR(buf.as_mut_ptr()), &mut sz);
        let _ = CloseHandle(h);
        if ok.is_err() {
            continue;
        }
        let full = String::from_utf16_lossy(&buf[..sz as usize]);
        let name = std::path::Path::new(&full)
            .file_name()
            .map(|f| f.to_string_lossy().into_owned())
            .unwrap_or_else(|| full.clone());
        if !name.is_empty() {
            set.insert(name);
        }
    }
    set.into_iter().collect()
}

/// Register a top-level class for one of the modal popups. `RegisterClassW`
/// returns 0 for an already-registered class (1410 = `ERROR_CLASS_ALREADY_EXISTS`),
/// which is expected when a popup is opened more than once — treat it as a
/// success.
///
/// # Safety
/// `name` must be a static null-terminated wide string; `wndproc` must be a
/// valid window-procedure entry point.
unsafe fn register_popup_class(name: PCWSTR, wndproc: WNDPROC, hinst: HINSTANCE) {
    let wc = WNDCLASSW {
        style: Default::default(),
        lpfnWndProc: wndproc,
        cbClsExtra: 0,
        cbWndExtra: 0,
        hInstance: hinst,
        hIcon: LoadIconW(None, IDI_APPLICATION).unwrap_or_default(),
        hCursor: LoadCursorW(None, IDC_ARROW).unwrap_or_default(),
        hbrBackground: GetSysColorBrush(COLOR_BTNFACE),
        lpszMenuName: PCWSTR::null(),
        lpszClassName: name,
    };
    let rc = RegisterClassW(&wc);
    if rc == 0 && GetLastError().0 != 1410 {
        log_err(&format!("register popup class: last error {}", GetLastError().0));
    }
}

/// State of the running-process picker popup, stashed in its `GWLP_USERDATA`.
struct PickerState {
    list: HWND,
    /// The main window (owner); receives `WM_PICK_RESULT`.
    parent: HWND,
    /// `ListKind` of the list to append to (`wparam` of `WM_PICK_RESULT`).
    target: isize,
    /// The enumerated names in list-row order (rows map 1:1 to this vector).
    names: Vec<String>,
    /// Set in `WM_DESTROY`; the nested modal loop polls it to know when to
    /// return.
    done: bool,
}

unsafe fn pick_state_mut(hwnd: HWND) -> &'static mut PickerState {
    let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
    assert!(p != 0, "aetheris-ui: picker state missing");
    &mut *(p as *mut PickerState)
}

/// Collect the names of the picker rows whose checkbox is checked.
unsafe fn pick_collect_checked(st: &PickerState) -> Vec<String> {
    let mut chosen = Vec::new();
    for row in 0..list_count(st.list) {
        if list_checked(st.list, row) {
            if let Some(name) = st.names.get(row as usize) {
                chosen.push(name.clone());
            }
        }
    }
    chosen
}

/// State of the add-by-name prompt popup, stashed in its `GWLP_USERDATA`.
struct PromptState {
    edit: HWND,
    /// The main window (owner); receives `WM_PROMPT_RESULT`.
    parent: HWND,
    /// `ListKind` of the list to append to (`wparam` of `WM_PROMPT_RESULT`).
    target: isize,
    /// Set in `WM_DESTROY`; the nested modal loop polls it.
    done: bool,
}

unsafe fn prompt_state_mut(hwnd: HWND) -> &'static mut PromptState {
    let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
    assert!(p != 0, "aetheris-ui: prompt state missing");
    &mut *(p as *mut PromptState)
}

unsafe extern "system" fn pick_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match msg {
        // WM_CREATE does nothing here: the children and state are created by
        // `show_process_picker` after the window exists (no lpParam box).
        WM_COMMAND => {
            let id = (wparam.0 & 0xffff) as isize;
            let code = ((wparam.0 >> 16) & 0xffff) as u32;
            match id {
                IDC_PICK_OK if code == 0 => {
                    // Scope the state borrow so it is dropped before the
                    // reentrant WM_DESTROY from DestroyWindow runs.
                    let (parent, target, chosen) = {
                        let st = pick_state_mut(hwnd);
                        (st.parent, st.target, pick_collect_checked(st))
                    };
                    if !chosen.is_empty() {
                        let boxed = Box::into_raw(Box::new(chosen));
                        let _ = PostMessageW(
                            Some(parent),
                            WM_PICK_RESULT,
                            WPARAM(target as usize),
                            LPARAM(boxed as isize),
                        );
                    }
                    let _ = DestroyWindow(hwnd);
                    LRESULT(0)
                }
                IDC_PICK_CANCEL if code == 0 => {
                    let _ = DestroyWindow(hwnd);
                    LRESULT(0)
                }
                _ => LRESULT(0),
            }
        }
        WM_DESTROY => {
            let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
            if p != 0 {
                (*(p as *mut PickerState)).done = true;
                drop(Box::from_raw(p as *mut PickerState));
                let _ = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            }
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
            report_error(&format!("panic in picker wndproc (msg {msg:#010x}): {text}"));
            LRESULT(0)
        }
    }
}

unsafe extern "system" fn prompt_wndproc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match msg {
        WM_COMMAND => {
            let id = (wparam.0 & 0xffff) as isize;
            let code = ((wparam.0 >> 16) & 0xffff) as u32;
            match id {
                IDC_PROMPT_OK if code == 0 => {
                    let (parent, target, text) = {
                        let st = prompt_state_mut(hwnd);
                        (st.parent, st.target, get_text(st.edit))
                    };
                    let text = text.trim().to_string();
                    if !text.is_empty() {
                        let boxed = Box::into_raw(Box::new(text));
                        let _ = PostMessageW(
                            Some(parent),
                            WM_PROMPT_RESULT,
                            WPARAM(target as usize),
                            LPARAM(boxed as isize),
                        );
                    }
                    let _ = DestroyWindow(hwnd);
                    LRESULT(0)
                }
                IDC_PROMPT_CANCEL if code == 0 => {
                    let _ = DestroyWindow(hwnd);
                    LRESULT(0)
                }
                _ => LRESULT(0),
            }
        }
        WM_DESTROY => {
            let p = GetWindowLongPtrW(hwnd, GWLP_USERDATA);
            if p != 0 {
                (*(p as *mut PromptState)).done = true;
                drop(Box::from_raw(p as *mut PromptState));
                let _ = SetWindowLongPtrW(hwnd, GWLP_USERDATA, 0);
            }
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
            report_error(&format!("panic in prompt wndproc (msg {msg:#010x}): {text}"));
            LRESULT(0)
        }
    }
}

/// Run a modal nested message loop for `popup`, disabling the owner `parent`
/// while it is open and re-enabling it on return. `done` points at the popup
/// state's `done` flag (and — after `WM_DESTROY` zeroes `GWLP_USERDATA` — the
/// loop exits via the `p == 0` short-circuit before ever dereferencing the
/// freed state).
///
/// # Safety
/// `done` must be a pointer into the popup's live `GWLP_USERDATA` state.
unsafe fn run_modal(popup: HWND, parent: HWND, done: *const bool) {
    let _ = EnableWindow(parent, false);
    let _ = ShowWindow(popup, SW_SHOW);
    let _ = SetForegroundWindow(popup);
    let mut msg = MSG::default();
    loop {
        let r = GetMessageW(&mut msg, Some(popup), 0, 0);
        if r.0 == 0 || r.0 == -1 {
            break;
        }
        let _ = TranslateMessage(&msg);
        let _ = DispatchMessageW(&msg);
        let p = GetWindowLongPtrW(popup, GWLP_USERDATA);
        if p == 0 || *done {
            break;
        }
    }
    let _ = EnableWindow(parent, true);
    let _ = SetForegroundWindow(parent);
}

/// Create the running-process picker modal popup and run it. On OK it posts
/// `WM_PICK_RESULT` to `parent` with the checked process names.
///
/// `lang` is the parent's active language, passed by value (it is `Copy`) so
/// this modal never re-enters the parent's [`UiState`] while the calling
/// `WM_COMMAND` frame's `&mut UiState` is live on the stack.
unsafe fn show_process_picker(parent: HWND, target: isize, hinst: HINSTANCE, lang: Lang) {
    register_popup_class(w!("aetheris_pick"), Some(pick_wndproc), hinst);
    let names = enumerate_processes();
    let title = to_wide(tr(lang, "pick_title"));
    let mut rc = RECT {
        left: 0,
        top: 0,
        right: 480,
        bottom: 520,
    };
    let _ = AdjustWindowRectEx(&mut rc, WS_OVERLAPPEDWINDOW, false, WINDOW_EX_STYLE::default());
    let Ok(popup) = CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        w!("aetheris_pick"),
        PCWSTR(title.as_ptr()),
        WS_OVERLAPPEDWINDOW | WS_CLIPSIBLINGS,
        CW_USEDEFAULT,
        CW_USEDEFAULT,
        rc.right - rc.left,
        rc.bottom - rc.top,
        Some(parent),
        None,
        Some(hinst),
        None,
    ) else {
        return;
    };

    // Children (created after the popup exists, before it is shown).
    let hint_w = to_wide(tr(lang, "pick_hint"));
    let _ = mk_child(
        popup,
        WINDOW_EX_STYLE::default(),
        w!("Static"),
        PCWSTR(hint_w.as_ptr()),
        WS_CHILD.0 | WS_VISIBLE.0,
        10,
        10,
        460,
        16,
        IDC_PICK_HINT,
        hinst,
    );
    let list = mk_list(popup, IDC_PICK_LIST, 10, 32, 460, 420, hinst);
    list_add_column(list, 0, tr(lang, "list_process_name"), 300);
    for name in &names {
        list_add_row(list, std::slice::from_ref(name));
        // New rows start unchecked: the hint says to check what to add.
    }
    let ok_w = to_wide(tr(lang, "pick_ok"));
    let _ = mk_child(
        popup,
        WINDOW_EX_STYLE::default(),
        w!("Button"),
        PCWSTR(ok_w.as_ptr()),
        WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0,
        280,
        472,
        90,
        30,
        IDC_PICK_OK,
        hinst,
    );
    let cancel_w = to_wide(tr(lang, "pick_cancel"));
    let _ = mk_child(
        popup,
        WINDOW_EX_STYLE::default(),
        w!("Button"),
        PCWSTR(cancel_w.as_ptr()),
        WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0,
        380,
        472,
        90,
        30,
        IDC_PICK_CANCEL,
        hinst,
    );

    let st = Box::new(PickerState {
        list,
        parent,
        target,
        names,
        done: false,
    });
    let done: *const bool = &st.done;
    SetWindowLongPtrW(popup, GWLP_USERDATA, Box::into_raw(st) as isize);
    run_modal(popup, parent, done);
}

/// Create the add-by-name prompt modal popup and run it. On OK it posts
/// `WM_PROMPT_RESULT` to `parent` with the typed name.
///
/// `lang` is the parent's active language, passed by value (it is `Copy`) so
/// this modal never re-enters the parent's [`UiState`] while the calling
/// `WM_COMMAND` frame's `&mut UiState` is live on the stack.
unsafe fn show_name_prompt(parent: HWND, target: isize, hinst: HINSTANCE, lang: Lang) {
    register_popup_class(w!("aetheris_prompt"), Some(prompt_wndproc), hinst);
    let title = to_wide(tr(lang, "add_prompt_title"));
    let mut rc = RECT {
        left: 0,
        top: 0,
        right: 380,
        bottom: 200,
    };
    let _ = AdjustWindowRectEx(&mut rc, WS_OVERLAPPEDWINDOW, false, WINDOW_EX_STYLE::default());
    let Ok(popup) = CreateWindowExW(
        WINDOW_EX_STYLE::default(),
        w!("aetheris_prompt"),
        PCWSTR(title.as_ptr()),
        WS_OVERLAPPEDWINDOW | WS_CLIPSIBLINGS,
        CW_USEDEFAULT,
        CW_USEDEFAULT,
        rc.right - rc.left,
        rc.bottom - rc.top,
        Some(parent),
        None,
        Some(hinst),
        None,
    ) else {
        return;
    };

    let hint_w = to_wide(tr(lang, "add_prompt_hint"));
    let _ = mk_child(
        popup,
        WINDOW_EX_STYLE::default(),
        w!("Static"),
        PCWSTR(hint_w.as_ptr()),
        WS_CHILD.0 | WS_VISIBLE.0,
        10,
        14,
        360,
        16,
        IDC_PROMPT_HINT,
        hinst,
    );
    let edit = mk_child(
        popup,
        WS_EX_CLIENTEDGE,
        w!("Edit"),
        PCWSTR::null(),
        WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | WS_BORDER.0 | ES_AUTOHSCROLL as u32,
        10,
        38,
        360,
        24,
        IDC_PROMPT_EDIT,
        hinst,
    );
    let ok_w = to_wide(tr(lang, "pick_ok"));
    let _ = mk_child(
        popup,
        WINDOW_EX_STYLE::default(),
        w!("Button"),
        PCWSTR(ok_w.as_ptr()),
        WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0,
        170,
        140,
        90,
        30,
        IDC_PROMPT_OK,
        hinst,
    );
    let cancel_w = to_wide(tr(lang, "pick_cancel"));
    let _ = mk_child(
        popup,
        WINDOW_EX_STYLE::default(),
        w!("Button"),
        PCWSTR(cancel_w.as_ptr()),
        WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0,
        270,
        140,
        90,
        30,
        IDC_PROMPT_CANCEL,
        hinst,
    );

    let st = Box::new(PromptState {
        edit,
        parent,
        target,
        done: false,
    });
    let done: *const bool = &st.done;
    SetWindowLongPtrW(popup, GWLP_USERDATA, Box::into_raw(st) as isize);
    run_modal(popup, parent, done);
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

        WM_CLOSE => {
            // Title-bar close: hide to the tray instead of destroying the
            // window. Destroying would run WM_DESTROY -> PostQuitMessage and
            // quit the UI; only the tray Exit item (IDM_EXIT) calls
            // DestroyWindow and is the quit path.
            let _ = ShowWindow(hwnd, SW_HIDE);
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
                LRESULT(0)
            } else {
                // Let DefWindowProc handle normal restores/resizes (reflowing
                // min/max buttons etc.) instead of swallowing them.
                DefWindowProcW(hwnd, msg, wparam, lparam)
            }
        }

        WM_TIMER => {
            if wparam.0 == TRAY_STATUS_TIMER_ID {
                let s = state_mut(hwnd);
                s.start_status_probe(hwnd);
            }
            LRESULT(0)
        }

        WM_SERVICE_START => {
            // The startup probe has launched the service via UAC.
            let s = state_mut(hwnd);
            s.set_result(hwnd, tr(s.lang, "service_starting"));
            LRESULT(0)
        }

        WM_SERVICE_UP => {
            let s = state_mut(hwnd);
            // The service came up after our UAC launch; re-run the startup
            // config load and status pull (the original workers failed while it
            // was down).
            s.spawn(hwnd, IpcCall::GetConfig, Request::GetConfig);
            s.start_refresh(hwnd);
            LRESULT(0)
        }

        WM_SERVICE_GIVEUP => {
            // The startup probe gave up: its ~5 s retry window closed with the
            // service still down. Drop the stale "starting service…" line for an
            // explicit manual-launch instruction.
            let s = state_mut(hwnd);
            s.set_result(hwnd, tr(s.lang, "service_giveup"));
            LRESULT(0)
        }

        WM_PICK_RESULT => {
            // The running-process picker posted the checked process names
            // (`lparam` = `*mut Vec<String>`, `wparam` = target ListKind).
            let names = Box::from_raw(lparam.0 as *mut Vec<String>);
            if let Some(kind) = ListKind::from_usize(wparam.0) {
                let s = state_mut(hwnd);
                for name in names.iter() {
                    s.append_name(hwnd, kind, name);
                }
            }
            LRESULT(0)
        }

        WM_PROMPT_RESULT => {
            // The add-by-name prompt posted the typed name (`lparam` = `*mut
            // String`, `wparam` = target ListKind).
            let name = Box::from_raw(lparam.0 as *mut String);
            if let Some(kind) = ListKind::from_usize(wparam.0) {
                let s = state_mut(hwnd);
                s.append_name(hwnd, kind, name.as_str());
            }
            LRESULT(0)
        }

        WM_COMMAND => {
            let id = (wparam.0 & 0xffff) as isize;
            let code = ((wparam.0 >> 16) & 0xffff) as u32;
            // A synchronous re-entry — e.g. CBN_SELCHANGE from a `combo_set_sel`
            // (CB_SETCURSEL) inside a guarded `load_selected_row_into_editor`, or
            // EN_CHANGE from a programmatic `set_text` — arrives while the outer
            // frame already holds `&mut UiState`. Back out without minting a
            // second reference (matching the `state_is_busy` discipline used by
            // the LVN_ITEMCHANGED handler).
            if state_is_busy(hwnd) {
                return LRESULT(0);
            }
            let s = state_mut(hwnd);
            match id {
                // The pipe-touching buttons just hand the request to a worker
                // thread and return; the result is applied when the posted
                // WM_IPC_RESULT is dispatched, so the dialog never blocks on a
                // down service.
                IDC_BTN_START if code == 0 => {
                    start_service();
                    // Refresh promptly so the status line follows the launch.
                    s.start_refresh(hwnd);
                }
                IDC_BTN_STOP if code == 0 => {
                    s.stop_service();
                    // The service may already be down; a refresh shows 未运行.
                    s.start_refresh(hwnd);
                }
                IDC_BTN_SAVE if code == 0 => s.do_save(hwnd),
                // `btn_advanced` toggles the collapsible advanced panel;
                // `btn_lang` cycles the UI language (persisted to ui.toml).
                IDC_BTN_ADVANCED if code == 0 => s.toggle_advanced(hwnd),
                IDC_BTN_LANG if code == 0 => s.toggle_language(hwnd),
                IDC_ADV_APPLY if code == 0 => s.apply_advanced(hwnd),
                IDC_ADV_RELOAD if code == 0 => s.reload_advanced(hwnd),
                IDC_BTN_PICK_GAME if code == 0 => s.open_picker(hwnd, ListKind::Game),
                IDC_BTN_ADD_GAME if code == 0 => s.open_prompt(hwnd, ListKind::Game),
                IDC_BTN_PICK_BG if code == 0 => s.open_picker(hwnd, ListKind::Background),
                IDC_BTN_ADD_BG if code == 0 => s.open_prompt(hwnd, ListKind::Background),
                _ => {}
            }
            LRESULT(0)
        }

        WM_NOTIFY => {
            let nm = &*(lparam.0 as *const NMHDR);
            if nm.code == LVN_ITEMCHANGED
                && (nm.idFrom as isize == IDC_LIST_GAME || nm.idFrom as isize == IDC_LIST_BG)
            {
                // A checkbox toggle / selection change fires this reentrantly
                // (e.g. `LVM_SETITEMSTATE` during a rebuild). When busy, the
                // outer frame holds `&mut UiState`, so back out without
                // touching state.
                if state_is_busy(hwnd) {
                    return LRESULT(0);
                }
                // A selection change on the background list reloads the
                // advanced editor for the newly selected row (only meaningful
                // while the panel is expanded). A checkbox toggle, which also
                // fires LVN_ITEMCHANGED, leaves the editor alone — only the
                // LVIS_SELECTED bit differing between old/new state counts.
                if nm.idFrom as isize == IDC_LIST_BG {
                    let nmlv = &*(lparam.0 as *const NMLISTVIEW);
                    let sel_changed = ((nmlv.uNewState ^ nmlv.uOldState) & LVIS_SELECTED.0) != 0;
                    if sel_changed {
                        let s = state_mut(hwnd);
                        if s.advanced_expanded {
                            // Guard the load: `load_selected_row_into_editor`
                            // programs the priority combo (`combo_set_sel` /
                            // CB_SETCURSEL), which synchronously re-enters
                            // WM_COMMAND while this frame's `&mut UiState` is
                            // live. Holding the busy flag suppresses the
                            // re-entrant state access (the same `state_is_busy`
                            // discipline the LVN_ITEMCHANGED rebuild path uses).
                            let _busy = BusyGuard::acquire(&mut *s);
                            s.load_selected_row_into_editor();
                        }
                    }
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

    // --- Status area (top) ---
    let h_status = mk_child(
        hwnd,
        WINDOW_EX_STYLE::default(),
        w!("Static"),
        w!("Not running"),
        WS_CHILD.0 | WS_VISIBLE.0,
        10,
        12,
        440,
        20,
        IDC_STATUS,
        hinst,
    );
    let h_btn_start = mk_child(
        hwnd,
        WINDOW_EX_STYLE::default(),
        w!("Button"),
        w!("Start service"),
        WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0,
        470,
        8,
        130,
        28,
        IDC_BTN_START,
        hinst,
    );
    let h_btn_stop = mk_child(
        hwnd,
        WINDOW_EX_STYLE::default(),
        w!("Button"),
        w!("Stop service"),
        WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0,
        610,
        8,
        130,
        28,
        IDC_BTN_STOP,
        hinst,
    );
    // Language toggle (top-right): cycles Zh ↔ En; labeled via `tr(lang, "lang")`
    // by `apply_language`.
    let h_btn_lang = mk_child(
        hwnd,
        WINDOW_EX_STYLE::default(),
        w!("Button"),
        w!("Language"),
        WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0,
        750,
        8,
        140,
        28,
        IDC_BTN_LANG,
        hinst,
    );

    // --- Games list + buttons ---
    let h_label_game = mk_child(
        hwnd,
        WINDOW_EX_STYLE::default(),
        w!("Static"),
        w!("Games"),
        WS_CHILD.0 | WS_VISIBLE.0,
        10,
        44,
        420,
        16,
        IDC_LABEL_GAME,
        hinst,
    );
    let h_list_game = mk_list(hwnd, IDC_LIST_GAME, 10, 62, 420, 180, hinst);
    let h_btn_pick_game = mk_child(
        hwnd,
        WINDOW_EX_STYLE::default(),
        w!("Button"),
        w!("Pick from running"),
        WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0,
        440,
        62,
        140,
        28,
        IDC_BTN_PICK_GAME,
        hinst,
    );
    let h_btn_add_game = mk_child(
        hwnd,
        WINDOW_EX_STYLE::default(),
        w!("Button"),
        w!("Add"),
        WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0,
        440,
        96,
        140,
        28,
        IDC_BTN_ADD_GAME,
        hinst,
    );

    // --- Background-apps list + buttons ---
    let h_label_bg = mk_child(
        hwnd,
        WINDOW_EX_STYLE::default(),
        w!("Static"),
        w!("Background apps"),
        WS_CHILD.0 | WS_VISIBLE.0,
        10,
        250,
        420,
        16,
        IDC_LABEL_BG,
        hinst,
    );
    let h_list_bg = mk_list(hwnd, IDC_LIST_BG, 10, 268, 420, 140, hinst);
    let h_btn_pick_bg = mk_child(
        hwnd,
        WINDOW_EX_STYLE::default(),
        w!("Button"),
        w!("Pick from running"),
        WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0,
        440,
        268,
        140,
        28,
        IDC_BTN_PICK_BG,
        hinst,
    );
    let h_btn_add_bg = mk_child(
        hwnd,
        WINDOW_EX_STYLE::default(),
        w!("Button"),
        w!("Add"),
        WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0,
        440,
        302,
        140,
        28,
        IDC_BTN_ADD_BG,
        hinst,
    );

    // --- Optimization group ---
    let h_label_opt = mk_child(
        hwnd,
        WINDOW_EX_STYLE::default(),
        w!("Static"),
        w!("Optimization"),
        WS_CHILD.0 | WS_VISIBLE.0,
        10,
        416,
        540,
        16,
        IDC_LABEL_OPT,
        hinst,
    );
    let h_opt_suspend = mk_child(
        hwnd,
        WINDOW_EX_STYLE::default(),
        w!("Button"),
        w!("Suspend"),
        WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | BS_AUTOCHECKBOX as u32,
        16,
        436,
        90,
        20,
        IDC_OPT_SUSPEND,
        hinst,
    );
    let h_opt_low_prio = mk_child(
        hwnd,
        WINDOW_EX_STYLE::default(),
        w!("Button"),
        w!("Lower priority"),
        WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | BS_AUTOCHECKBOX as u32,
        112,
        436,
        120,
        20,
        IDC_OPT_LOW_PRIO,
        hinst,
    );
    let h_opt_cpu = mk_child(
        hwnd,
        WINDOW_EX_STYLE::default(),
        w!("Button"),
        w!("Limit CPU"),
        WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | BS_AUTOCHECKBOX as u32,
        240,
        436,
        90,
        20,
        IDC_OPT_CPU,
        hinst,
    );
    let h_combo_cpu = mk_child(
        hwnd,
        WS_EX_CLIENTEDGE,
        w!("ComboBox"),
        PCWSTR::null(),
        WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | CBS_DROPDOWNLIST as u32 | WS_VSCROLL.0,
        334,
        434,
        96,
        120,
        IDC_COMBO_CPU,
        hinst,
    );
    let h_opt_mem = mk_child(
        hwnd,
        WINDOW_EX_STYLE::default(),
        w!("Button"),
        w!("Trim memory"),
        WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | BS_AUTOCHECKBOX as u32,
        436,
        436,
        110,
        20,
        IDC_OPT_MEM,
        hinst,
    );

    // --- Result line + save/advanced ---
    let h_result = mk_child(
        hwnd,
        WS_EX_STATICEDGE,
        w!("Static"),
        w!(""),
        WS_CHILD.0 | WS_VISIBLE.0,
        10,
        474,
        470,
        16,
        IDC_STATUS_RESULT,
        hinst,
    );
    let h_btn_save = mk_child(
        hwnd,
        WINDOW_EX_STYLE::default(),
        w!("Button"),
        w!("Save"),
        WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0,
        600,
        470,
        120,
        30,
        IDC_BTN_SAVE,
        hinst,
    );
    let h_btn_advanced = mk_child(
        hwnd,
        WINDOW_EX_STYLE::default(),
        w!("Button"),
        w!("Advanced"),
        WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0,
        730,
        470,
        160,
        30,
        IDC_BTN_ADVANCED,
        hinst,
    );

    // --- Advanced panel (per-row editor; collapsed by default) ---
    // All controls are created WITHOUT `WS_VISIBLE`: `show_advanced` reveals
    // them on expand and hides them on collapse. Labels are placeholders here —
    // `apply_language` re-renders everything before the window is shown.
    let h_adv_title = mk_child(
        hwnd,
        WINDOW_EX_STYLE::default(),
        w!("Static"),
        w!("Advanced settings"),
        WS_CHILD.0,
        10,
        250,
        400,
        16,
        IDC_ADV_TITLE,
        hinst,
    );
    let h_adv_lbl_name = mk_child(
        hwnd,
        WINDOW_EX_STYLE::default(),
        w!("Static"),
        w!("Name"),
        WS_CHILD.0,
        14,
        272,
        100,
        20,
        IDC_ADV_LBL_NAME,
        hinst,
    );
    let h_adv_name = mk_child(
        hwnd,
        WS_EX_CLIENTEDGE,
        w!("Edit"),
        PCWSTR::null(),
        WS_CHILD.0 | WS_TABSTOP.0 | WS_BORDER.0 | ES_AUTOHSCROLL as u32,
        120,
        270,
        320,
        22,
        IDC_ADV_NAME,
        hinst,
    );
    let h_adv_lbl_prio = mk_child(
        hwnd,
        WINDOW_EX_STYLE::default(),
        w!("Static"),
        w!("Priority"),
        WS_CHILD.0,
        14,
        298,
        100,
        20,
        IDC_ADV_LBL_PRIO,
        hinst,
    );
    let h_adv_combo_prio = mk_child(
        hwnd,
        WS_EX_CLIENTEDGE,
        w!("ComboBox"),
        PCWSTR::null(),
        WS_CHILD.0 | WS_TABSTOP.0 | CBS_DROPDOWNLIST as u32 | WS_VSCROLL.0,
        120,
        296,
        180,
        140,
        IDC_ADV_COMBO_PRIO,
        hinst,
    );
    let h_adv_lbl_affinity = mk_child(
        hwnd,
        WINDOW_EX_STYLE::default(),
        w!("Static"),
        w!("Affinity"),
        WS_CHILD.0,
        14,
        324,
        100,
        20,
        IDC_ADV_LBL_AFFINITY,
        hinst,
    );
    let h_adv_affinity = mk_child(
        hwnd,
        WS_EX_CLIENTEDGE,
        w!("Edit"),
        PCWSTR::null(),
        WS_CHILD.0 | WS_TABSTOP.0 | WS_BORDER.0 | ES_AUTOHSCROLL as u32,
        120,
        322,
        180,
        22,
        IDC_ADV_AFFINITY,
        hinst,
    );
    let h_adv_lbl_qos = mk_child(
        hwnd,
        WINDOW_EX_STYLE::default(),
        w!("Static"),
        w!("QoS"),
        WS_CHILD.0,
        316,
        324,
        50,
        20,
        IDC_ADV_LBL_QOS,
        hinst,
    );
    let h_adv_qos = mk_child(
        hwnd,
        WS_EX_CLIENTEDGE,
        w!("Edit"),
        PCWSTR::null(),
        WS_CHILD.0 | WS_TABSTOP.0 | WS_BORDER.0 | ES_AUTOHSCROLL as u32,
        368,
        322,
        100,
        22,
        IDC_ADV_QOS,
        hinst,
    );
    let h_adv_suspend = mk_child(
        hwnd,
        WINDOW_EX_STYLE::default(),
        w!("Button"),
        w!("Suspend"),
        WS_CHILD.0 | WS_TABSTOP.0 | BS_AUTOCHECKBOX as u32,
        14,
        350,
        110,
        20,
        IDC_ADV_SUSPEND,
        hinst,
    );
    let h_adv_trim = mk_child(
        hwnd,
        WINDOW_EX_STYLE::default(),
        w!("Button"),
        w!("Trim memory"),
        WS_CHILD.0 | WS_TABSTOP.0 | BS_AUTOCHECKBOX as u32,
        130,
        350,
        130,
        20,
        IDC_ADV_TRIM,
        hinst,
    );
    let h_adv_reload = mk_child(
        hwnd,
        WINDOW_EX_STYLE::default(),
        w!("Button"),
        w!("Reload"),
        WS_CHILD.0 | WS_TABSTOP.0,
        14,
        384,
        120,
        30,
        IDC_ADV_RELOAD,
        hinst,
    );
    let h_adv_apply = mk_child(
        hwnd,
        WINDOW_EX_STYLE::default(),
        w!("Button"),
        w!("Apply"),
        WS_CHILD.0 | WS_TABSTOP.0,
        144,
        384,
        120,
        30,
        IDC_ADV_APPLY,
        hinst,
    );

    // Solid status icons for the tray: green (0x00FF00) = service up, gray
    // (0x808080) = service down. Owned here and freed on WM_DESTROY.
    let h_icon_green = make_status_icon(COLORREF(0x0000FF00));
    let h_icon_gray = make_status_icon(COLORREF(0x00808080));

    UiState {
        pipe,
        // Placeholder until `apply_language` runs right after window creation
        // with the language loaded from ui.toml (default: detected system).
        lang: Lang::En,
        cfg: Config::default(),
        init_error: None,
        config_loaded: false,
        save_in_flight: false,
        busy: false,
        mode: String::new(),
        cpu_level: 1,
        advanced_expanded: false,
        bg_overrides: HashMap::new(),
        h_status,
        h_btn_start,
        h_btn_stop,
        h_result,
        h_list_game,
        h_list_bg,
        h_btn_pick_game,
        h_btn_add_game,
        h_btn_pick_bg,
        h_btn_add_bg,
        h_label_game,
        h_label_bg,
        h_label_opt,
        h_opt_suspend,
        h_opt_low_prio,
        h_opt_cpu,
        h_combo_cpu,
        h_opt_mem,
        h_btn_save,
        h_btn_advanced,
        h_btn_lang,
        h_adv_title,
        h_adv_lbl_name,
        h_adv_name,
        h_adv_lbl_prio,
        h_adv_combo_prio,
        h_adv_lbl_affinity,
        h_adv_affinity,
        h_adv_lbl_qos,
        h_adv_qos,
        h_adv_suspend,
        h_adv_trim,
        h_adv_reload,
        h_adv_apply,
        h_icon_green,
        h_icon_gray,
        status_probe_in_flight: false,
    }
}

/// Create a report-view `SysListView32` child with full-row selection and
/// checkboxes (`LVS_EX_CHECKBOXES` — used by both main lists and the picker).
unsafe fn mk_list(parent: HWND, id: isize, x: i32, y: i32, w: i32, h: i32, hinst: HINSTANCE) -> HWND {
    let list = mk_child(
        parent,
        WS_EX_CLIENTEDGE,
        w!("SysListView32"),
        PCWSTR::null(),
        WS_CHILD.0 | WS_VISIBLE.0 | WS_TABSTOP.0 | WS_BORDER.0 | WS_VSCROLL.0 | LVS_REPORT,
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
        Some(WPARAM((LVS_EX_FULLROWSELECT | LVS_EX_CHECKBOXES) as usize)),
        Some(LPARAM((LVS_EX_FULLROWSELECT | LVS_EX_CHECKBOXES) as isize)),
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
            let init = Box::into_raw(Box::new(InitData { pipe: pipe.clone() }));

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

            // Apply the loaded language (ui.toml, defaulting to the detected
            // system language) BEFORE the window is shown, so every control
            // (labels, buttons, list headers, combos, status line) paints in
            // the right language on first paint. The state borrow is scoped so
            // the later `state_mut` calls below are a fresh, unaliased borrow.
            let lang = aetheris_core::i18n::load_ui_settings().lang;
            {
                let s = state_mut(hwnd);
                apply_language(s, hwnd, lang);
            }

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
        // WM_IPC_RESULT messages arrive.
        unsafe {
            let s = state_mut(hwnd);
            s.spawn(hwnd, IpcCall::GetConfig, Request::GetConfig);
            s.spawn(hwnd, IpcCall::GetState, Request::GetState);
        }

        // Startup probe: if the service is down, UAC-launch it and retry over
        // ~5 s (worker thread; never blocks the UI thread or the dialog).
        startup_probe(hwnd, pipe);

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

// ---------------------------------------------------------------------------
// Value formatting / parsing helpers (Task 4 reuses these)
// ---------------------------------------------------------------------------

/// True when the service's Save refusal means "run the UI elevated". The
/// service refuses `SaveConfig` with different phrasings across builds, so the
/// detection is a pure, unit-tested classifier rather than inline string
/// sniffing that a message reword could silently break.
fn needs_elevation(msg: &str) -> bool {
    msg.to_ascii_lowercase().contains("elevat")
        || msg.to_ascii_lowercase().contains("administrator")
}

#[cfg(test)]
mod tests {
    use super::{
        bg_rule_from_toggles, cpu_level_to_pct, idx_to_priority, keys, needs_elevation,
        parse_affinity, parse_qos, pct_to_cpu_level, priority_to_idx, tr,
    };
    use aetheris_core::config::PriorityClass;
    use aetheris_core::i18n::Lang;

    /// Every key in the string table resolves to a non-empty, non-placeholder
    /// string for BOTH languages. `keys()` is generated from the same match
    /// arms as `tr`, so this test can never drift from the table.
    #[test]
    fn table_is_complete_for_both_langs() {
        assert!(!keys().is_empty(), "string table must define at least one key");
        for &key in keys() {
            for lang in [Lang::Zh, Lang::En] {
                let s = tr(lang, key);
                assert!(
                    !s.is_empty(),
                    "tr({lang:?}, {key:?}) is an empty string (missing translation)"
                );
                assert_ne!(
                    s,
                    key,
                    "tr({lang:?}, {key:?}) returns the key itself (untranslated)"
                );
            }
        }
    }

    #[test]
    fn save_refusal_mentions_elevated_client() {
        assert!(needs_elevation(
            "SaveConfig requires an elevated client — run aetheris-ui as administrator"
        ));
    }

    #[test]
    fn save_refusal_mentions_administrator() {
        assert!(needs_elevation("requires administrator rights"));
    }

    #[test]
    fn unrelated_pipe_error_is_not_elevation() {
        assert!(!needs_elevation("pipe unavailable after retries"));
    }

    #[test]
    fn cpu_level_maps_to_quota() {
        // 低/中/高 combo -> qos_cpu_quota 30/50/70.
        assert_eq!(cpu_level_to_pct(0), 30);
        assert_eq!(cpu_level_to_pct(1), 50);
        assert_eq!(cpu_level_to_pct(2), 70);
        // And back.
        assert_eq!(pct_to_cpu_level(30), 0);
        assert_eq!(pct_to_cpu_level(70), 2);
        assert_eq!(pct_to_cpu_level(50), 1);
    }

    #[test]
    fn bg_rule_from_toggles_applies_global_fields() {
        // All toggles on: every field is set from the global defaults.
        let r = bg_rule_from_toggles("x.exe".to_string(), true, true, true, 50, true);
        assert_eq!(r.name, "x.exe");
        assert!(r.suspend);
        assert_eq!(r.priority, Some(PriorityClass::BelowNormal));
        assert_eq!(r.qos_cpu_quota, Some(50));
        assert!(r.trim_memory);
        assert!(r.affinity.is_none());

        // All toggles off: a name-only rule.
        let r2 = bg_rule_from_toggles("y.exe".to_string(), false, false, false, 30, false);
        assert_eq!(r2.name, "y.exe");
        assert!(!r2.suspend);
        assert_eq!(r2.priority, None);
        assert_eq!(r2.qos_cpu_quota, None);
        assert!(!r2.trim_memory);
        assert!(r2.affinity.is_none());
    }

    #[test]
    fn priority_roundtrip_combo_index() {
        // The advanced priority combo order is the enum order; both directions
        // agree for every variant.
        let all = [
            PriorityClass::Idle,
            PriorityClass::BelowNormal,
            PriorityClass::Normal,
            PriorityClass::AboveNormal,
            PriorityClass::High,
            PriorityClass::Realtime,
        ];
        for (idx, p) in all.iter().enumerate() {
            assert_eq!(priority_to_idx(*p), idx, "priority_to_idx({p:?})");
            assert_eq!(idx_to_priority(idx), Some(*p), "idx_to_priority({idx})");
        }
        assert_eq!(idx_to_priority(6), None, "out-of-range index");
        assert_eq!(idx_to_priority(usize::MAX), None);
    }

    #[test]
    fn parse_affinity_text() {
        assert!(parse_affinity("").unwrap().is_none());
        assert!(parse_affinity("   ").unwrap().is_none());
        let a = parse_affinity("0,1,2").unwrap().unwrap();
        assert_eq!(a.cores, vec![0u8, 1, 2]);
        let a2 = parse_affinity("0, 4").unwrap().unwrap();
        assert_eq!(a2.cores, vec![0u8, 4]);
        assert!(parse_affinity("0,,").is_err(), "empty token rejected");
        assert!(parse_affinity("abc").is_err(), "non-numeric rejected");
        assert!(parse_affinity("64").is_err(), "core index >= 64 rejected");
    }

    #[test]
    fn parse_qos_text() {
        assert_eq!(parse_qos("").unwrap(), None);
        assert_eq!(parse_qos("   ").unwrap(), None);
        assert_eq!(parse_qos("50").unwrap(), Some(50));
        assert_eq!(parse_qos("100").unwrap(), Some(100));
        assert!(parse_qos("0").is_err(), "quota 0 rejected");
        assert!(parse_qos("101").is_err(), "quota > 100 rejected");
        assert!(parse_qos("abc").is_err(), "non-numeric rejected");
    }
}
