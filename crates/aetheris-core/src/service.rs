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
//! action is deferred with a warn (throttled to once per second). The v1 sampler
//! stub returns 0, so the hook never self-throttles incorrectly — the real
//! `NtQuerySystemInformation(SystemProcessorPerformanceInformation)` sampling
//! lands in v1.1.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use std::sync::mpsc::{Receiver, Sender, channel};

use crate::actions::OsBackend;
use crate::config::Config;
use crate::events::{ForegroundEvent, ProcessEvent};
use crate::ipc::{IpcServer, ProcessInfo, Request, Response, StateSnapshot, DEFAULT_PIPE};
use crate::log;
use crate::policy::{Mode, PolicyEngine};

/// Messages consumed by the service main loop.
#[derive(Debug)]
pub enum ServiceMsg {
    Proc(ProcessEvent),
    Foreground(ForegroundEvent),
    Reload,
    Stop,
}

pub struct Service {
    cfg_path: PathBuf,
    engine: PolicyEngine<OsBackend>,
    stop_tx: Sender<ServiceMsg>,
    stop_rx: Option<Receiver<ServiceMsg>>,
    /// Shared IPC snapshot: refreshed by the main loop after every message and
    /// read by the IPC thread to answer `GetState` / `QueryProcess` without
    /// touching the engine.
    state: Arc<RwLock<StateSnapshot>>,
}

impl Service {
    /// Build the service and the shared state snapshot `Arc`. The caller hands
    /// the `Arc` to whichever thread must answer live-state queries (in v1 the
    /// IPC thread spawned by [`Service::run`]).
    pub fn new(cfg_path: &Path, cfg: Config) -> (Self, Arc<RwLock<StateSnapshot>>) {
        let backend = OsBackend::new();
        if let Err(e) = backend.enable_privileges() {
            log::warn(format!("privilege bootstrap failed: {e}"));
        }
        let (stop_tx, stop_rx) = channel::<ServiceMsg>();
        let state = Arc::new(RwLock::new(StateSnapshot::default()));
        (
            Self {
                cfg_path: cfg_path.to_path_buf(),
                engine: PolicyEngine::new(cfg, backend),
                stop_tx,
                stop_rx: Some(stop_rx),
                state: state.clone(),
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
    /// snapshot is refreshed once after every message, so `GetState` /
    /// `QueryProcess` readers always see the latest engine state.
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
            ServiceMsg::Stop => {
                // Restore every boosted process before the loop breaks, so a
                // Ctrl-C mid-game never leaves processes suspended or
                // down-prioritized.
                self.engine.exit_game_mode();
                Ok(())
            }
        };
        self.refresh_state();
        res
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
        StateSnapshot { mode, boosted, processes, last_reload }
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

        // IPC server: answers GetState/QueryProcess from the shared snapshot
        // (refreshed by the main loop after every message) and forwards reloads
        // to the main loop.
        let state = self.state.clone();
        let ipc_tx = tx.clone();
        let ipc_server = IpcServer::new(DEFAULT_PIPE);
        std::thread::spawn(move || {
            let mut handle_req = |req: &Request| -> Response {
                match req {
                    Request::GetState => {
                        let s = state.read().unwrap();
                        Response::State(s.clone())
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
            }
        }
        Ok(())
    }
}

/// v1 stub: returns 0 so graceful degradation never self-throttles incorrectly.
/// Real `NtQuerySystemInformation(SystemProcessorPerformanceInformation)`
/// sampling lands in v1.1.
pub fn system_load_percent() -> u32 {
    0
}
