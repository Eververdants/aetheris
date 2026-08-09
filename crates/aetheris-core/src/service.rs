//! Service: channel hub, main loop, reload, and graceful degradation.
//!
//! Dedicated producer threads — the ETW process monitor, the foreground window
//! watcher, and the named-pipe IPC server — each feed one shared
//! [`std::sync::mpsc::channel`] of [`ServiceMsg`]; the single-threaded main loop
//! `recv()`s and dispatches to the [`PolicyEngine`]. A separate stop channel lets
//! the launcher ask the loop to exit via [`ServiceMsg::Stop`].
//!
//! Graceful degradation: on [`ServiceMsg::Proc`] events the engine is only
//! consulted when `system_load_percent()` is at or below 85; above that the
//! action is deferred with a warn (throttled to once per second). The sampler
//! reads `NtQuerySystemInformation(SystemProcessorPerformanceInformation)` and on
//! any failure returns 0, so the hook never self-throttles incorrectly.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{channel, Receiver, Sender};
use std::sync::{Arc, Mutex, RwLock};

use windows::Win32::Foundation::HANDLE;

use crate::actions::OsBackend;
use crate::config::Config;
use crate::events::{ForegroundEvent, ProcessEvent};
use crate::ipc::{
    IpcServer, ProcessInfo, Request, Response, StateSnapshot, DEFAULT_PIPE, DEFAULT_PIPE_DACL,
};
use crate::log;
use crate::policy::{Mode, PolicyEngine};

/// Messages consumed by the service main loop.
#[derive(Debug)]
pub enum ServiceMsg {
    Proc(ProcessEvent),
    Foreground(ForegroundEvent),
    Reload,
    /// Persist `cfg` (validate, atomic temp+rename, reload into the engine) and
    /// report the outcome on `reply`. The IPC thread sends this and blocks on
    /// `reply` so a `SaveConfig` request gets a synchronous result.
    SaveConfig {
        cfg: Config,
        reply: Sender<Result<String, String>>,
    },
    /// Toggle the overlay: close the running overlay window if one exists
    /// (graceful `WM_CLOSE`), otherwise launch `aetheris-overlay.exe` next to
    /// the service. Sent by the hotkey watcher thread on a configured hotkey
    /// press.
    ToggleOverlay,
    Stop,
}

/// Minimum interval between snapshot rebuilds on the integrated message path.
///
/// [`Service::current_state`] clones every process name into a fresh `processes`
/// / `boosted` Vec, so rebuilding per event is O(N) String allocations per
/// message under churn (a game spawning many processes/threads). Throttling to
/// this interval keeps the IPC snapshot fresh enough while bounding allocations.
/// `Reload` / `Stop` always force a rebuild so a reload outcome and the
/// post-stop restore are never stale in the snapshot.
const SNAPSHOT_REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_millis(250);

pub struct Service {
    cfg_path: PathBuf,
    engine: PolicyEngine<OsBackend>,
    stop_tx: Sender<ServiceMsg>,
    stop_rx: Option<Receiver<ServiceMsg>>,
    /// Shared IPC snapshot: rebuilt by the main loop (throttled to
    /// [`SNAPSHOT_REFRESH_INTERVAL`]) and read by the IPC thread to answer
    /// `GetState` / `QueryProcess` without touching the engine.
    state: Arc<RwLock<StateSnapshot>>,
    /// Last time the shared snapshot was rebuilt on the integrated message
    /// path; `None` before the first message. Used to throttle the O(N)
    /// rebuilds in [`Service::current_state`].
    last_refresh: Option<std::time::Instant>,
}

impl Service {
    /// Build the service and the shared state snapshot `Arc`. The caller hands
    /// the `Arc` to whichever thread must answer live-state queries (in v1 the
    /// IPC thread spawned by [`Service::run`]).
    pub fn new(cfg_path: &Path, cfg: Config) -> (Self, Arc<RwLock<StateSnapshot>>) {
        // Reconcile stale network-QoS tweaks left by a service that died
        // mid-GameBoost: if the crash marker exists, revert exactly what it
        // lists and remove it — BEFORE the engine starts, so a previous crash
        // never leaves applied tweaks with no revert path. Safe: only reverts
        // what the marker says the service set, and only when the marker exists.
        let network_marker = crate::network::default_marker_path();
        crate::network::reconcile(&network_marker);

        let backend = OsBackend::new();
        if let Err(e) = backend.enable_privileges() {
            log::warn(format!("privilege bootstrap failed: {e}"));
        }
        let (stop_tx, stop_rx) = channel::<ServiceMsg>();
        let state = Arc::new(RwLock::new(StateSnapshot::default()));
        // Seed the shared snapshot's config up front: the snapshot is only
        // rebuilt on the first message, so without this a client connecting
        // before any event would read `Config::default()` from GetConfig (and
        // an unedited save would clobber the real config).
        state.write().unwrap().config = cfg.clone();
        let mut engine = PolicyEngine::new(cfg, backend);
        // The engine writes/clears this same marker on GameBoost entry/exit, so
        // the next startup reconciles precisely what this run applied.
        engine.set_network_marker(network_marker);
        (
            Self {
                cfg_path: cfg_path.to_path_buf(),
                engine,
                stop_tx,
                stop_rx: Some(stop_rx),
                state: state.clone(),
                last_refresh: None,
            },
            state,
        )
    }

    pub fn cfg_path(&self) -> &Path {
        &self.cfg_path
    }

    /// Sender used by the launcher to stop the main loop via [`ServiceMsg::Stop`].
    pub fn stop_sender(&self) -> Sender<ServiceMsg> {
        self.stop_tx.clone()
    }

    /// Dispatch a message to the engine. The testable core of the loop: `Reload`
    /// re-reads the config file and swaps it in (which exits GameBoost cleanly);
    /// `Stop` exits GameBoost so every boosted process is restored (a suspended
    /// or down-prioritized process must not survive the service). The shared
    /// snapshot is refreshed on the way out, throttled to
    /// [`SNAPSHOT_REFRESH_INTERVAL`] so churn does not rebuild the O(N) process
    /// Vecs per event; `Reload` / `Stop` always force a fresh snapshot, and
    /// `SaveConfig` refreshes *before* its reply so a `GetConfig` arriving
    /// immediately after cannot read a pre-save config.
    pub fn handle_message(&mut self, msg: &ServiceMsg) -> Result<(), String> {
        let res = match msg {
            ServiceMsg::Proc(ev) => {
                self.engine.on_process_event(ev);
                Ok(())
            }
            ServiceMsg::Foreground(ev) => {
                self.engine.on_foreground(ev);
                Ok(())
            }
            ServiceMsg::Reload => self.reload(),
            ServiceMsg::SaveConfig { cfg, reply } => {
                let res = self.persist_config(cfg);
                // Refresh BEFORE replying so a GetConfig arriving right after
                // SaveConfig reads the just-saved config, not the pre-save one.
                // A failed `reply.send` is surfaced as `Err` (the reply is the
                // only delivery a SaveConfig has, so its failure must be
                // observable, not swallowed).
                self.maybe_refresh_state(msg);
                reply
                    .send(res)
                    .map_err(|e| format!("save config reply failed: {e}"))
            }
            ServiceMsg::Stop => {
                // Restore every boosted process before the loop breaks, so a
                // Ctrl-C mid-game never leaves processes suspended or
                // down-prioritized.
                self.engine.exit_game_mode();
                Ok(())
            }
            ServiceMsg::ToggleOverlay => {
                launch_overlay();
                Ok(())
            }
        };
        // SaveConfig already refreshed above (before its reply); every other
        // message refreshes on the way out.
        if !matches!(msg, ServiceMsg::SaveConfig { .. }) {
            self.maybe_refresh_state(msg);
        }
        res
    }

    /// Rebuild the shared snapshot after a message, throttled to
    /// [`SNAPSHOT_REFRESH_INTERVAL`] so the O(N) name-cloning rebuild in
    /// [`Service::current_state`] does not run on every event under churn.
    /// `Reload` / `Stop` always force a rebuild so a reload outcome and the
    /// post-stop restore are never stale; `SaveConfig` also forces one so a
    /// just-saved config is immediately visible to `GetConfig` readers. The
    /// timer is also advanced on a forced rebuild so a subsequent throttled
    /// message does not immediately re-trigger.
    fn maybe_refresh_state(&mut self, msg: &ServiceMsg) {
        let force = matches!(
            msg,
            ServiceMsg::Reload | ServiceMsg::Stop | ServiceMsg::SaveConfig { .. }
        );
        if force {
            self.refresh_state();
            self.last_refresh = Some(std::time::Instant::now());
            return;
        }
        let now = std::time::Instant::now();
        let due = self
            .last_refresh
            .map(|t| now.duration_since(t) >= SNAPSHOT_REFRESH_INTERVAL)
            .unwrap_or(true);
        if due {
            self.last_refresh = Some(now);
            self.refresh_state();
        }
    }

    /// Re-read the config file and swap it in (`set_config` exits GameBoost
    /// cleanly). `last_reload` is recorded in the shared snapshot so IPC readers
    /// can see the most recent reload outcome.
    fn reload(&mut self) -> Result<(), String> {
        match Config::load(&self.cfg_path) {
            Ok(cfg) => {
                self.engine.set_config(cfg);
                self.set_last_reload(None);
                Ok(())
            }
            Err(e) => {
                let msg = e.to_string();
                self.set_last_reload(Some(msg.clone()));
                Err(msg)
            }
        }
    }

    fn set_last_reload(&self, val: Option<String>) {
        if let Ok(mut s) = self.state.write() {
            s.last_reload = val;
        }
    }

    /// Validate `cfg`, write it atomically over `cfg_path` (temp file in the
    /// same directory + rename), and reload it into the engine.
    ///
    /// Invalid configs are rejected *before* any write, so a bad config never
    /// touches the file. The temp name embeds the pid so concurrent saves (or a
    /// leftover from a crashed process) never collide.
    fn persist_config(&mut self, cfg: &Config) -> Result<String, String> {
        cfg.validate().map_err(|e| e.to_string())?;
        let dir = self.cfg_path.parent().unwrap_or(std::path::Path::new("."));
        let tmp = dir.join(format!(".aetheris.toml.tmp{}", std::process::id()));
        std::fs::write(&tmp, toml::to_string(cfg).map_err(|e| e.to_string())?)
            .map_err(|e| e.to_string())?;
        std::fs::rename(&tmp, &self.cfg_path).map_err(|e| e.to_string())?;
        self.engine.set_config(cfg.clone());
        Ok("saved".into())
    }

    /// Rebuild the shared snapshot from the engine's current state. Reads
    /// `last_reload` back out of the snapshot so `current_state` callers (and a
    /// subsequent refresh) do not clobber the recorded reload outcome.
    fn refresh_state(&self) {
        let snap = self.current_state();
        if let Ok(mut s) = self.state.write() {
            *s = snap;
        }
    }

    pub fn current_state(&self) -> StateSnapshot {
        let mode = match self.engine.mode() {
            Mode::Normal => "Normal".to_string(),
            Mode::GameBoost => "GameBoost".to_string(),
        };
        let boosted = self
            .engine
            .boosted()
            .iter()
            .map(|(&pid, _)| ProcessInfo {
                pid,
                name: self.engine.pid_name(pid).unwrap_or_default(),
                is_game: false,
            })
            .collect();
        let processes = self
            .engine
            .iter_processes()
            .map(|(pid, name, is_game)| ProcessInfo {
                pid,
                name: name.to_string(),
                is_game,
            })
            .collect();
        let last_reload = self.state.read().ok().and_then(|s| s.last_reload.clone());
        StateSnapshot { mode, boosted, processes, last_reload, config: self.engine.cfg().clone() }
    }

    /// Spawn the ETW / foreground / IPC threads, each feeding the shared event
    /// channel, and run the main loop until a [`ServiceMsg::Stop`] arrives.
    pub fn run(mut self) -> Result<(), String> {
        let (tx, rx): (Sender<ServiceMsg>, Receiver<ServiceMsg>) = channel();

        // Relay the stop channel into the event channel.
        if let Some(stop_rx) = self.stop_rx.take() {
            let t = tx.clone();
            std::thread::spawn(move || {
                if let Ok(m) = stop_rx.recv() {
                    let _ = t.send(m);
                }
            });
        }

        // ETW process monitor (fail-safe: any setup error exits the loop).
        let etw = crate::etw::EtwMonitor::start()?;
        let etw_tx = tx.clone();
        std::thread::spawn(move || {
            while let Some(ev) = etw.recv() {
                if etw_tx.send(ServiceMsg::Proc(ev)).is_err() {
                    break;
                }
            }
        });

        // Foreground window watcher.
        let fg = crate::foreground::ForegroundWatcher::start()?;
        let fg_tx = tx.clone();
        std::thread::spawn(move || {
            while let Some(ev) = fg.recv() {
                if fg_tx.send(ServiceMsg::Foreground(ev)).is_err() {
                    break;
                }
            }
        });

        // Overlay hotkey: a configurable global hotkey launches the overlay on
        // demand. The watcher runs on its own thread + message-only window, so
        // it stays independent of the foreground watcher's pump. A hotkey that
        // fails to parse is silently disabled (config default); one that fails
        // to register is logged and the service keeps running without it.
        if let Some(hk) = self
            .engine
            .cfg()
            .overlay
            .hotkey
            .as_deref()
            .and_then(crate::hotkey::parse_hotkey)
        {
            let hotkey_tx = tx.clone();
            match crate::hotkey::HotkeyWatcher::start(hk) {
                Ok(watcher) => {
                    std::thread::spawn(move || {
                        while watcher.recv().is_some() {
                            if hotkey_tx.send(ServiceMsg::ToggleOverlay).is_err() {
                                break;
                            }
                        }
                    });
                }
                Err(e) => log::warn(format!("overlay hotkey could not start: {e}")),
            }
        }

        // IPC server: answers GetState/QueryProcess/GetConfig from the shared
        // snapshot (refreshed by the main loop, throttled to
        // SNAPSHOT_REFRESH_INTERVAL), forwards reloads to the main loop, and
        // routes SaveConfig through the main loop, blocking for the outcome.
        let state = self.state.clone();
        let ipc_tx = tx.clone();
        // Interactive Users DACL so a non-elevated aetheris-cli can reach the
        // elevated service; SYSTEM retains full access. The DACL grants
        // transport-level read+write only; the file-rewriting SaveConfig is
        // separately gated below on the connected client's elevation.
        let ipc_server = IpcServer::new_with_dacl(DEFAULT_PIPE, DEFAULT_PIPE_DACL);
        std::thread::spawn(move || {
            // The handler receives the connected pipe HANDLE (IpcServer::run
            // passes it through) so privileged requests can check the client's
            // token before the connection is torn down.
            let mut handle_req = |pipe: HANDLE, req: &Request| -> Response {
                match req {
                    Request::GetState => {
                        let s = state.read().unwrap();
                        Response::State(s.clone())
                    }
                    Request::GetConfig => {
                        let cfg = state.read().unwrap().config.clone();
                        Response::Config(cfg)
                    }
                    Request::QueryProcess(name) => {
                        let s = state.read().unwrap();
                        let found = s
                            .processes
                            .iter()
                            .find(|p| {
                                p.name
                                    .to_ascii_lowercase()
                                    .contains(&name.to_ascii_lowercase())
                            })
                            .cloned();
                        Response::Process(found)
                    }
                    Request::ReloadConfig => {
                        let _ = ipc_tx.send(ServiceMsg::Reload);
                        Response::Reload("queued".into())
                    }
                    Request::SaveConfig(cfg) => {
                        // SaveConfig rewrites the admin-owned config file, so it
                        // requires an elevated client. Fail closed: an Err from
                        // the elevation check counts as not elevated, and the
                        // file is never touched in that case.
                        if !crate::ipc::is_client_elevated(pipe).unwrap_or(false) {
                            return Response::SaveConfig(Err("requires elevation".into()));
                        }
                        let (tx, rx) = channel();
                        let _ = ipc_tx.send(ServiceMsg::SaveConfig {
                            cfg: cfg.clone(),
                            reply: tx,
                        });
                        match rx.recv() {
                            Ok(res) => Response::SaveConfig(res),
                            Err(_) => {
                                Response::SaveConfig(Err("service unavailable".into()))
                            }
                        }
                    }
                }
            };
            let _ = ipc_server.run(&mut handle_req);
        });

        // Graceful degradation: on high system load, defer optimization actions.
        let mut last_degrade_warn = std::time::Instant::now();
        while let Ok(msg) = rx.recv() {
            match msg {
                ServiceMsg::Stop => {
                    // Restore GameBoost state (resume suspended, restore
                    // priority/affinity/QoS) before breaking out of the loop.
                    let _ = self.handle_message(&ServiceMsg::Stop);
                    break;
                }
                ServiceMsg::Reload => {
                    // A malformed config keeps the previous config active; log
                    // it so the failure is at least visible (the IPC handler
                    // still answers "queued" — the warning is the feedback).
                    if let Err(e) = self.handle_message(&ServiceMsg::Reload) {
                        log::warn(format!("reload failed (keeping previous config): {e}"));
                    }
                }
                ServiceMsg::SaveConfig { cfg, reply } => {
                    // The reply payload carries the persist outcome back to the
                    // blocked IPC thread; handle_message returns Err here only
                    // when the reply channel itself could not be delivered.
                    if let Err(e) =
                        self.handle_message(&ServiceMsg::SaveConfig { cfg, reply })
                    {
                        log::warn(format!("save config reply failed: {e}"));
                    }
                }
                ServiceMsg::Proc(ev) => {
                    if system_load_percent() > 85 {
                        if last_degrade_warn.elapsed() > std::time::Duration::from_secs(1) {
                            log::warn("high system load: deferring optimization actions");
                            last_degrade_warn = std::time::Instant::now();
                        }
                    } else {
                        let _ = self.handle_message(&ServiceMsg::Proc(ev));
                    }
                }
                ServiceMsg::Foreground(ev) => {
                    let _ = self.handle_message(&ServiceMsg::Foreground(ev));
                }
                ServiceMsg::ToggleOverlay => {
                    let _ = self.handle_message(&ServiceMsg::ToggleOverlay);
                }
            }
        }
        Ok(())
    }
}

/// Toggle the overlay: close the running overlay window if one exists,
/// otherwise launch `aetheris-overlay.exe` next to the service binary.
///
/// This is the overlay toggle: the hotkey watcher sends [`ServiceMsg::ToggleOverlay`]
/// and the main loop calls this. Pressing the hotkey again finds the running
/// overlay window (class `aetheris_overlay`) and posts `WM_CLOSE` to it — the
/// overlay exits on `WM_CLOSE` (`WM_DESTROY` → `PostQuitMessage` → exit 0), so
/// a second press can never stack a duplicate panel. If no window exists the
/// overlay is launched. A missing overlay binary or a failed `CreateProcessW`
/// is non-fatal — the failure is logged and the service keeps running. The
/// process handles are closed immediately after spawn: the overlay is a
/// standalone window process expected to detach and outlive the service.
fn launch_overlay() {
    // Close the running overlay window (class `aetheris_overlay`) instead of
    // launching a duplicate. FindWindowW returns Err when no such window
    // exists, so `Ok` here means the overlay is up and we toggle it off.
    let existing = unsafe {
        windows::Win32::UI::WindowsAndMessaging::FindWindowW(
            windows::core::w!("aetheris_overlay"),
            None,
        )
    };
    if let Ok(hwnd) = existing {
        let _ = unsafe {
            windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                Some(hwnd),
                windows::Win32::UI::WindowsAndMessaging::WM_CLOSE,
                windows::Win32::Foundation::WPARAM(0),
                windows::Win32::Foundation::LPARAM(0),
            )
        };
        return;
    }
    let overlay = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|d| d.to_path_buf()))
        .map(|dir| dir.join("aetheris-overlay.exe"));
    match overlay {
        Some(p) if p.exists() => {
            let w: Vec<u16> = p
                .to_string_lossy()
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect();
            let si = windows::Win32::System::Threading::STARTUPINFOW {
                cb: std::mem::size_of::<windows::Win32::System::Threading::STARTUPINFOW>() as u32,
                ..Default::default()
            };
            let mut pi = windows::Win32::System::Threading::PROCESS_INFORMATION::default();
            let ok = unsafe {
                windows::Win32::System::Threading::CreateProcessW(
                    windows::core::PCWSTR(w.as_ptr()),
                    None,
                    None,
                    None,
                    false,
                    windows::Win32::System::Threading::CREATE_NO_WINDOW,
                    None,
                    None,
                    &si,
                    &mut pi,
                )
            };
            match ok {
                Ok(()) => {
                    let _ = unsafe { windows::Win32::Foundation::CloseHandle(pi.hProcess) };
                    let _ = unsafe { windows::Win32::Foundation::CloseHandle(pi.hThread) };
                }
                Err(e) => log::warn(format!("overlay launch failed: {e}")),
            }
        }
        _ => log::warn("aetheris-overlay.exe not found next to the service"),
    }
}

/// Previous aggregate CPU sample, used to compute the delta between two calls
/// to [`system_load_percent`].
///
/// `idle` is the summed [`IdleTime`] across all processors; `total` is the
/// summed [`KernelTime`] + [`UserTime`]. [`KernelTime`] already includes
/// [`IdleTime`], so `total` must NOT add idle in again — the busy delta is
/// `Δtotal − Δidle`.
///
/// [`IdleTime`]: ntapi::ntexapi::SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION
/// [`KernelTime`]: ntapi::ntexapi::SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION
/// [`UserTime`]: ntapi::ntexapi::SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION
#[derive(Clone, Copy)]
struct LoadSample {
    idle: u64,
    total: u64,
}

static LOAD_STATE: Mutex<Option<LoadSample>> = Mutex::new(None);
static LOAD_FAILED_WARNED: AtomicBool = AtomicBool::new(false);

/// Current system busy percentage, 0..=100, from two
/// `NtQuerySystemInformation(SystemProcessorPerformanceInformation)` samples.
///
/// Idle and total processor time are summed across all processors; the ratio of
/// the per-call deltas (`Δtotal − Δidle`, where `total = KernelTime + UserTime`
/// and `KernelTime` already includes `IdleTime`) is the busy percentage. The
/// first call only seeds the previous sample and returns 0 (not enough data). On
/// any query failure the call returns 0 — safe for graceful degradation, which
/// must never self-throttle on a broken sampler — and logs a warning once.
pub fn system_load_percent() -> u32 {
    let mut cur = LoadSample { idle: 0, total: 0 };
    let ok = unsafe {
        // Size the buffer to the active logical-CPU count so
        // NtQuerySystemInformation does not fail with
        // STATUS_INFO_LENGTH_MISMATCH on >64-logical-CPU hosts (a fixed
        // 64-entry array returned 0 forever there). Clamp to a sane cap: at
        // least 64 (so a failed `logical_cpu_count()` of 0 keeps the previous
        // behavior) and at most 256 (a larger buffer than the real count is
        // still accepted, but no host needs more).
        let count = crate::actions::logical_cpu_count().clamp(64, 256) as usize;
        type CpuPerf = ntapi::ntexapi::SYSTEM_PROCESSOR_PERFORMANCE_INFORMATION;
        let mut info: Vec<CpuPerf> = vec![std::mem::zeroed(); count];
        let size = (count * std::mem::size_of::<CpuPerf>()) as u32;
        let status = ntapi::ntexapi::NtQuerySystemInformation(
            ntapi::ntexapi::SystemProcessorPerformanceInformation,
            info.as_mut_ptr() as *mut _,
            size,
            std::ptr::null_mut(),
        );
        if status == 0 {
            let mut idle = 0u64;
            let mut total = 0u64;
            for p in info.iter() {
                idle = idle.saturating_add(*p.IdleTime.QuadPart() as u64);
                // KernelTime already INCLUDES IdleTime, so `total` must not add
                // it again or idle systems would report ~50% busy.
                total = total
                    .saturating_add(*p.KernelTime.QuadPart() as u64)
                    .saturating_add(*p.UserTime.QuadPart() as u64);
            }
            cur = LoadSample { idle, total };
            true
        } else {
            false
        }
    };

    if !ok {
        if !LOAD_FAILED_WARNED.swap(true, Ordering::Relaxed) {
            log::warn("system load sampling failed (NtQuerySystemInformation)");
        }
        return 0;
    }

    let mut guard = LOAD_STATE.lock().unwrap();
    let prev = match &*guard {
        Some(p) => *p,
        None => {
            *guard = Some(cur);
            return 0; // first sample: not enough data
        }
    };
    *guard = Some(cur);

    let d_total = cur.total.saturating_sub(prev.total);
    let d_idle = cur.idle.saturating_sub(prev.idle);
    if d_total == 0 {
        return 0;
    }
    let busy = d_total.saturating_sub(d_idle);
    let pct = (busy as f64 / d_total as f64 * 100.0).round();
    (pct as u32).min(100)
}

#[cfg(test)]
mod load_tests {
    use super::system_load_percent;

    #[test]
    fn load_in_range() {
        let v = system_load_percent();
        assert!(v <= 100, "load {v} out of range");
    }

    #[test]
    fn load_changes_slowly() {
        // Two samples close together must both be 0..=100 (stability, not monotonicity).
        let a = system_load_percent();
        std::thread::sleep(std::time::Duration::from_millis(120));
        let b = system_load_percent();
        assert!(a <= 100 && b <= 100);
    }
}
