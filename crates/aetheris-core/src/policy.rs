//! Policy engine: a small state machine (Normal / GameBoost) that turns process
//! and foreground events into actions on the OS via a [`ProcessBackend`].
//!
//! - Always-rules are applied whenever a matching process starts.
//! - Entering GameBoost snapshots and applies the background rule for every
//!   running background-matched process; processes that start mid-boost get the
//!   same treatment. Exiting GameBoost (or a config reload while boosting)
//!   restores every snapshot.
//! - Protected processes are never acted on.

use std::collections::{BTreeSet, HashMap};

use crate::actions::{ProcessBackend, ProcState, TargetAction};
use crate::config::{AffinitySpec, Config};
use crate::events::{ForegroundEvent, ProcessEvent, ProcessKind};
use crate::proc_table::{name_hash, ProcessTable};
use crate::rules::PatternMatcher;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    GameBoost,
}

pub struct PolicyEngine<B: ProcessBackend> {
    cfg: Config,
    matcher: PatternMatcher,
    protected: BTreeSet<String>,
    backend: B,
    table: ProcessTable,
    mode: Mode,
    boosted: HashMap<u32, ProcState>,
    game_pids: Vec<u32>,
    foreground_pid: Option<u32>,
}

impl<B: ProcessBackend> PolicyEngine<B> {
    pub fn new(cfg: Config, backend: B) -> Self {
        let protected = cfg.protected_set();
        let matcher = PatternMatcher::new(cfg.game.processes.clone());
        Self {
            cfg,
            matcher,
            protected,
            backend,
            table: ProcessTable::new(),
            mode: Mode::Normal,
            boosted: HashMap::new(),
            game_pids: Vec::new(),
            foreground_pid: None,
        }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn boosted(&self) -> &HashMap<u32, ProcState> {
        &self.boosted
    }

    /// Process name recorded for `pid`, if known (used by
    /// `Service::current_state` to render a state snapshot).
    pub fn pid_name(&self, pid: u32) -> Option<String> {
        self.table.name(pid).map(|s| s.to_string())
    }

    fn is_protected(&self, name: &str) -> bool {
        self.protected.contains(&name.to_ascii_lowercase())
    }

    fn is_game(&self, name: &str) -> bool {
        self.matcher.matches(name)
    }

    fn find_background_rule(&self, name: &str) -> Option<&crate::config::BackgroundRule> {
        let name = name.to_ascii_lowercase();
        self.cfg
            .background
            .iter()
            .find(|r| PatternMatcher::new(vec![r.name.clone()]).matches(&name))
    }

    fn actions_for(rule: &crate::config::BackgroundRule) -> Vec<TargetAction> {
        let mut v = Vec::new();
        if let Some(p) = rule.priority {
            v.push(TargetAction::Priority(p));
        }
        if let Some(a) = &rule.affinity {
            v.push(TargetAction::Affinity { core_mask: mask_from_affinity(a) });
        }
        if let Some(q) = rule.qos_cpu_quota {
            v.push(TargetAction::QosCpuQuota { percent: q });
        }
        if rule.suspend {
            v.push(TargetAction::Suspend);
        }
        if rule.trim_memory {
            v.push(TargetAction::TrimMemory);
        }
        v
    }

    fn always_actions_for(&self, name: &str) -> Vec<TargetAction> {
        let name_l = name.to_ascii_lowercase();
        if let Some(r) = self.cfg.rule.iter().find(|r| {
            PatternMatcher::new(vec![r.name.clone()]).matches(&name_l)
        }) {
            let mut v = Vec::new();
            if let Some(p) = r.priority {
                v.push(TargetAction::Priority(p));
            }
            if let Some(a) = &r.affinity {
                v.push(TargetAction::Affinity { core_mask: mask_from_affinity(a) });
            }
            return v;
        }
        Vec::new()
    }

    pub fn on_process_event(&mut self, ev: &ProcessEvent) {
        match ev.kind {
            ProcessKind::Start => {
                self.table.upsert(ev.pid, &ev.name, false);
                if self.is_protected(&ev.name) {
                    return;
                }
                // Always-rules apply in any mode.
                for a in self.always_actions_for(&ev.name) {
                    let _ = self.backend.apply(ev.pid, &a);
                }
                // Game entry on Start only when configured to boost on start;
                // otherwise game entry waits for a foreground event (see
                // `on_foreground`). A game process is never treated as
                // background, so when `boost_on_start` is off we do nothing here.
                if self.is_game(&ev.name) {
                    if self.cfg.game.boost_on_start {
                        self.enter_game_mode(ev.pid, &ev.name);
                    }
                } else if self.mode == Mode::GameBoost && !self.game_pids.contains(&ev.pid) {
                    self.apply_background_to(ev.pid, &ev.name);
                }
            }
            ProcessKind::Stop => {
                self.table.remove(ev.pid);
                self.foreground_pid = if self.foreground_pid == Some(ev.pid) {
                    None
                } else {
                    self.foreground_pid
                };
                if self.game_pids.contains(&ev.pid) {
                    self.exit_game_mode();
                } else if let Some(state) = self.boosted.remove(&ev.pid) {
                    let _ = self.backend.restore(ev.pid, &state);
                }
            }
        }
    }

    pub fn on_foreground(&mut self, ev: &ForegroundEvent) {
        self.foreground_pid = Some(ev.pid);
        let name = self
            .table
            .name(ev.pid)
            .map(|s| s.to_string())
            .unwrap_or_default();
        if self.cfg.game.boost_on_start {
            return;
        }
        if self.is_game(&name) {
            if self.mode != Mode::GameBoost {
                self.enter_game_mode(ev.pid, &name);
            }
        } else if self.mode == Mode::GameBoost
            && !self.game_pids.contains(&ev.pid)
            && !self.is_game(&name)
        {
            // Foreground left the game.
            self.exit_game_mode();
        }
    }

    fn enter_game_mode(&mut self, game_pid: u32, _game_name: &str) {
        if self.mode == Mode::GameBoost {
            if !self.game_pids.contains(&game_pid) {
                self.game_pids.push(game_pid);
            }
            return;
        }
        self.mode = Mode::GameBoost;
        self.game_pids.clear();
        self.game_pids.push(game_pid);
        let mut table: Vec<(u32, String)> = self
            .table
            .iter()
            .filter(|(pid, name, _)| *pid != game_pid && !self.is_protected(name))
            .map(|(pid, name, _)| (pid, name.to_string()))
            .collect();
        table.sort_by_key(|(_, name)| name_hash(name));
        for (pid, name) in table {
            self.apply_background_to(pid, &name);
        }
    }

    /// Exit GameBoost: restore every boosted process (resume suspended, restore
    /// priority/affinity/QoS) and return to `Mode::Normal`. Public so the
    /// service can restore state on shutdown (Ctrl-C / `ServiceMsg::Stop`) —
    /// a suspended or down-prioritized process must never survive the service.
    pub fn exit_game_mode(&mut self) {
        if self.mode != Mode::GameBoost {
            return;
        }
        self.mode = Mode::Normal;
        self.game_pids.clear();
        for (pid, state) in std::mem::take(&mut self.boosted) {
            let _ = self.backend.restore(pid, &state);
        }
    }

    fn apply_background_to(&mut self, pid: u32, name: &str) {
        if self.is_protected(name) {
            return;
        }
        let rule = match self.find_background_rule(name) {
            Some(r) => r,
            None => return,
        };
        if self.boosted.contains_key(&pid) {
            return;
        }
        let mut state = match self.backend.snapshot(pid) {
            Ok(s) => s,
            Err(_) => return,
        };
        for a in Self::actions_for(rule) {
            match self.backend.apply(pid, &a) {
                Ok(()) => {
                    // Record what was actually applied so `restore` can reverse
                    // it (Critical 1): the pre-action snapshot always reports
                    // `suspended: false` / `qos_percent: None`, so without this
                    // a suspended or QoS-capped process was never resumed or
                    // un-capped on game exit.
                    match a {
                        TargetAction::Suspend => state.suspended = true,
                        TargetAction::QosCpuQuota { percent } => state.qos_percent = Some(percent),
                        _ => {}
                    }
                }
                Err(e) => crate::log::warn(format!("apply {pid} {:?}: {e}", a)),
            }
        }
        self.boosted.insert(pid, state);
    }

    pub fn set_config(&mut self, cfg: Config) {
        self.exit_game_mode();
        self.cfg = cfg;
        self.matcher = PatternMatcher::new(self.cfg.game.processes.clone());
        self.protected = self.cfg.protected_set();
    }
}

fn mask_from_affinity(a: &AffinitySpec) -> u64 {
    crate::actions::mask_from_cores(&a.cores)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::ActionError;
    use crate::config::{AlwaysRule, BackgroundRule, GameConfig, PriorityClass};
    use std::sync::{Arc, Mutex};

    #[derive(Default, Clone, Debug)]
    struct Call {
        pid: u32,
        action: Option<TargetAction>,
        restore: Option<ProcState>,
    }

    #[derive(Default, Clone)]
    struct RecordingBackend {
        calls: Arc<Mutex<Vec<Call>>>,
    }

    impl RecordingBackend {
        fn calls(&self) -> Vec<Call> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl ProcessBackend for RecordingBackend {
        fn snapshot(&self, _pid: u32) -> Result<ProcState, ActionError> {
            Ok(ProcState { priority: 0, affinity: 1, suspended: false, qos_percent: None })
        }
        fn apply(&self, pid: u32, action: &TargetAction) -> Result<(), ActionError> {
            self.calls.lock().unwrap().push(Call { pid, action: Some(*action), restore: None });
            Ok(())
        }
        fn restore(&self, pid: u32, state: &ProcState) -> Result<(), ActionError> {
            self.calls.lock().unwrap().push(Call { pid, action: None, restore: Some(*state) });
            Ok(())
        }
    }

    fn cfg() -> Config {
        Config {
            game: GameConfig { boost_on_start: true, processes: vec!["game.exe".into()] },
            background: vec![BackgroundRule {
                name: "browser.exe".into(),
                suspend: true,
                priority: Some(PriorityClass::BelowNormal),
                affinity: None,
                qos_cpu_quota: None,
                trim_memory: false,
            }],
            rule: vec![],
            protected_extra: vec![],
        }
    }

    fn start(pid: u32, name: &str) -> ProcessEvent {
        ProcessEvent { pid, name: name.into(), parent_pid: 0, kind: ProcessKind::Start }
    }
    fn stop(pid: u32, name: &str) -> ProcessEvent {
        ProcessEvent { pid, name: name.into(), parent_pid: 0, kind: ProcessKind::Stop }
    }

    #[test]
    fn game_start_boosts_background() {
        let backend = RecordingBackend::default();
        let mut eng = PolicyEngine::new(cfg(), backend.clone());
        eng.on_process_event(&start(100, "browser.exe"));
        eng.on_process_event(&start(200, "game.exe"));
        assert_eq!(eng.mode(), Mode::GameBoost);
        let calls = backend.calls();
        let actions: Vec<&TargetAction> = calls
            .iter()
            .filter(|c| c.pid == 100 && c.action.is_some())
            .map(|c| c.action.as_ref().unwrap())
            .collect();
        assert!(actions.contains(&&TargetAction::Priority(PriorityClass::BelowNormal)));
        assert!(actions.contains(&&TargetAction::Suspend));
        assert!(eng.boosted().contains_key(&100));
    }

    #[test]
    fn game_exit_restores_background() {
        let backend = RecordingBackend::default();
        let mut eng = PolicyEngine::new(cfg(), backend.clone());
        eng.on_process_event(&start(100, "browser.exe"));
        eng.on_process_event(&start(200, "game.exe"));
        assert_eq!(eng.mode(), Mode::GameBoost);
        eng.on_process_event(&stop(200, "game.exe"));
        assert_eq!(eng.mode(), Mode::Normal);
        assert!(eng.boosted().is_empty());
        let calls = backend.calls();
        assert!(calls.iter().any(|c| c.pid == 100 && c.restore.is_some()));
    }

    #[test]
    fn background_start_during_boost_is_boosted() {
        let backend = RecordingBackend::default();
        let mut eng = PolicyEngine::new(cfg(), backend.clone());
        eng.on_process_event(&start(200, "game.exe"));
        assert_eq!(eng.mode(), Mode::GameBoost);
        eng.on_process_event(&start(100, "browser.exe"));
        assert!(eng.boosted().contains_key(&100));
        let calls = backend.calls();
        assert!(calls.iter().any(|c| c.pid == 100 && c.action == Some(TargetAction::Suspend)));
    }

    #[test]
    fn foreground_trigger_with_boost_on_start_false() {
        let mut c = cfg();
        c.game.boost_on_start = false;
        let backend = RecordingBackend::default();
        let mut eng = PolicyEngine::new(c, backend.clone());
        eng.on_process_event(&start(300, "browser.exe"));
        eng.on_process_event(&start(200, "game.exe"));
        eng.on_foreground(&ForegroundEvent { pid: 200 });
        assert_eq!(eng.mode(), Mode::GameBoost);
        assert!(eng.boosted().contains_key(&300));
    }

    #[test]
    fn protected_process_never_boosted() {
        let backend = RecordingBackend::default();
        let mut eng = PolicyEngine::new(cfg(), backend.clone());
        eng.on_process_event(&start(200, "game.exe"));
        eng.on_process_event(&start(100, "csrss.exe"));
        assert!(!eng.boosted().contains_key(&100));
    }

    #[test]
    fn config_reload_exits_boost_cleanly() {
        let backend = RecordingBackend::default();
        let mut eng = PolicyEngine::new(cfg(), backend.clone());
        eng.on_process_event(&start(100, "browser.exe"));
        eng.on_process_event(&start(200, "game.exe"));
        assert_eq!(eng.mode(), Mode::GameBoost);
        eng.set_config(cfg());
        assert_eq!(eng.mode(), Mode::Normal);
        assert!(eng.boosted().is_empty());
        let calls = backend.calls();
        assert!(calls.iter().any(|c| c.pid == 100 && c.restore.is_some()));
    }

    #[test]
    fn always_rule_applies_in_normal_mode() {
        let mut c = cfg();
        c.rule = vec![AlwaysRule {
            name: "updater.exe".into(),
            priority: Some(PriorityClass::Idle),
            affinity: None,
        }];
        let backend = RecordingBackend::default();
        let mut eng = PolicyEngine::new(c, backend.clone());
        eng.on_process_event(&start(100, "updater.exe"));
        assert_eq!(eng.mode(), Mode::Normal);
        let calls = backend.calls();
        assert!(calls
            .iter()
            .any(|c| c.pid == 100 && c.action == Some(TargetAction::Priority(PriorityClass::Idle))));
    }
}
