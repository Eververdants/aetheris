//! Policy engine: a small state machine (Normal / GameBoost) that turns process
//! and foreground events into actions on the OS via a [`ProcessBackend`].
//!
//! - Always-rules are applied whenever a matching process starts.
//! - Entering GameBoost snapshots and applies the background rule for every
//!   running background-matched process; processes that start mid-boost get the
//!   same treatment. Exiting GameBoost restores every snapshot.
//! - A config reload/save exits GameBoost to apply the new config, then
//!   re-enters it if a game is still running (see [`reenter_if_game_running`]):
//!   a running game emits no new Start/foreground event, so without re-entry the
//!   optimization would stay OFF until the game restarts.
//! - Protected processes are never acted on.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::PathBuf;

use crate::actions::{ProcessBackend, ProcState, QosLifecycle, TargetAction};
use crate::config::{AffinitySpec, Config};
use crate::events::{ForegroundEvent, ProcessEvent, ProcessKind};
use crate::proc_table::{name_hash, ProcessTable};
use crate::rules::PatternMatcher;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    GameBoost,
}

pub struct PolicyEngine<B: ProcessBackend + QosLifecycle> {
    cfg: Config,
    matcher: PatternMatcher,
    protected: BTreeSet<String>,
    backend: B,
    table: ProcessTable,
    mode: Mode,
    boosted: HashMap<u32, ProcState>,
    game_pids: Vec<u32>,
    // "Smart suspend": boosted-and-suspended pids that the user brought to the
    // foreground get resumed (so the app is never frozen while in use) and are
    // tracked here; they are re-suspended when they leave the foreground.
    foreground_resumed: HashSet<u32>,
    last_foreground: Option<u32>,
    // Precompiled matchers, rebuilt once at `new()` / `set_config()` instead of
    // per lookup. Pattern indices align with `cfg.background` / `cfg.rule`.
    background_matcher: PatternMatcher,
    always_matcher: PatternMatcher,
    background_names: Vec<String>, // parallel to cfg.background
    always_names: Vec<String>,     // parallel to cfg.rule
    protected_hashes: HashSet<u64>,
    // Backup of network QoS registry tweaks applied on GameBoost entry, reverted
    // on exit. `Some` only while `Mode::GameBoost` and `cfg.network.enabled`.
    network_backup: Option<Vec<crate::network::BackupEntry>>,
    // Crash-reconciliation marker path: `Service::new` reconciles stale tweaks
    // from a service death mid-boost here at startup; `enter_game_mode` writes
    // it and `exit_game_mode` clears it. Set from a service-provided path
    // (default: PROGRAMDATA, see `network::default_marker_path`).
    network_marker: PathBuf,
}

impl<B: ProcessBackend + QosLifecycle> PolicyEngine<B> {
    pub fn new(cfg: Config, backend: B) -> Self {
        let protected = cfg.protected_set();
        let matcher = PatternMatcher::new(cfg.game.processes.clone());
        let mut engine = Self {
            cfg,
            matcher,
            protected,
            backend,
            table: ProcessTable::new(),
            mode: Mode::Normal,
            boosted: HashMap::new(),
            game_pids: Vec::new(),
            foreground_resumed: HashSet::new(),
            last_foreground: None,
            background_matcher: PatternMatcher::new(Vec::new()),
            always_matcher: PatternMatcher::new(Vec::new()),
            background_names: Vec::new(),
            always_names: Vec::new(),
            protected_hashes: HashSet::new(),
            network_backup: None,
            network_marker: crate::network::default_marker_path(),
        };
        engine.rebuild_matchers();
        engine
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    /// Read access to the active config, exposed so `Service::current_state`
    /// can clone it into the shared IPC snapshot for `GetConfig`.
    pub fn cfg(&self) -> &Config {
        &self.cfg
    }

    /// Override the crash-reconciliation marker path. The service calls this
    /// with the same path it reconciled at startup, so `enter_game_mode` writes
    /// and `exit_game_mode` clears the marker that the next startup consumes.
    pub fn set_network_marker(&mut self, path: PathBuf) {
        self.network_marker = path;
    }

    /// Read access to the backend, used by tests to inspect OS-level QoS state
    /// (e.g. whether a Job Object exists for a pid) and by the service for
    /// backend-level operations.
    pub fn backend(&self) -> &B {
        &self.backend
    }

    pub fn boosted(&self) -> &HashMap<u32, ProcState> {
        &self.boosted
    }

    /// Process name recorded for `pid`, if known (used by
    /// `Service::current_state` to render a state snapshot).
    pub fn pid_name(&self, pid: u32) -> Option<String> {
        self.table.name(pid).map(|s| s.to_string())
    }

    /// Iterate every tracked process as `(pid, name, is_game)`. Exposed so
    /// `Service::current_state` can render the live process list into the shared
    /// IPC snapshot (`QueryProcess` matches against it).
    pub fn iter_processes(&self) -> impl Iterator<Item = (u32, &str, bool)> {
        self.table.iter()
    }

    /// Rebuild the precompiled matchers and the protected-hash set. Called from
    /// `new()` and `set_config()`; never from the per-event hot path.
    fn rebuild_matchers(&mut self) {
        self.background_names = self.cfg.background.iter().map(|r| r.name.to_ascii_lowercase()).collect();
        self.always_names = self.cfg.rule.iter().map(|r| r.name.to_ascii_lowercase()).collect();
        self.background_matcher = PatternMatcher::new(self.background_names.clone());
        self.always_matcher = PatternMatcher::new(self.always_names.clone());
        self.protected_hashes = self
            .protected
            .iter()
            .map(|p| crate::proc_table::name_hash(p))
            .collect();
    }

    fn is_protected(&self, name: &str) -> bool {
        // Allocation-free lookup: hash the name with the same case-insensitive
        // fold used to build `protected_hashes`, so arbitrary-case names match
        // already-lowercase protected entries.
        self.protected_hashes.contains(&crate::proc_table::name_hash(name))
    }

    /// Index of the earliest background rule matching `name`, in config order.
    fn first_matching_background(&self, name: &str) -> Option<usize> {
        // The matcher is `ascii_case_insensitive(true)`; scan the raw bytes with
        // no lowercase alloc. Take the MINIMUM pattern index over all (overlapping)
        // matches so the earliest rule in config order wins — aho-corasick reports
        // matches in leftmost scan order, not pattern order, so `.next()` alone
        // would not preserve config-order precedence for overlapping patterns.
        self.background_matcher
            .find_overlapping_iter(name.as_bytes())
            .map(|m| m.pattern().as_usize())
            .min()
    }

    /// Index of the earliest always-rule matching `name`, in config order.
    fn first_matching_always(&self, name: &str) -> Option<usize> {
        self.always_matcher
            .find_overlapping_iter(name.as_bytes())
            .map(|m| m.pattern().as_usize())
            .min()
    }

    fn background_rule_at(&self, i: usize) -> Option<&crate::config::BackgroundRule> {
        self.cfg.background.get(i)
    }

    #[cfg(test)]
    pub(crate) fn is_protected_for_test(&self, name: &str) -> bool {
        self.is_protected(name)
    }

    fn is_game(&self, name: &str) -> bool {
        self.matcher.matches(name)
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
        let Some(idx) = self.first_matching_always(name) else {
            return Vec::new();
        };
        let Some(r) = self.cfg.rule.get(idx) else {
            return Vec::new();
        };
        let mut v = Vec::new();
        if let Some(p) = r.priority {
            v.push(TargetAction::Priority(p));
        }
        if let Some(a) = &r.affinity {
            v.push(TargetAction::Affinity { core_mask: mask_from_affinity(a) });
        }
        v
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
                if self.game_pids.contains(&ev.pid) {
                    self.exit_game_mode();
                } else if let Some(state) = self.boosted.remove(&ev.pid) {
                    let _ = self.backend.restore(ev.pid, &state);
                }
                // The process is gone: release any Job Object held for it. Safe
                // because there is no longer a live process to strand.
                self.backend.on_process_exit(ev.pid);
            }
        }
    }

    pub fn on_foreground(&mut self, ev: &ForegroundEvent) {
        // Smart suspend: a boosted-and-suspended process the user brings to the
        // foreground is resumed immediately (never frozen while in use), and
        // re-suspended when it leaves the foreground. Runs in every mode.
        if let Some(prev) = self.last_foreground.take() {
            if prev != ev.pid && self.foreground_resumed.remove(&prev) {
                if self.boosted.get(&prev).map(|s| s.suspended).unwrap_or(false) {
                    // Left the foreground: re-suspend it (back to zero usage).
                    let _ = self.backend.apply(prev, &TargetAction::Suspend);
                }
            }
        }
        self.last_foreground = Some(ev.pid);
        if !self.foreground_resumed.contains(&ev.pid)
            && self.boosted.get(&ev.pid).map(|s| s.suspended).unwrap_or(false)
        {
            let _ = self.backend.apply(ev.pid, &TargetAction::Resume);
            self.foreground_resumed.insert(ev.pid);
        }

        // boost_on_start=false: foreground matching a game enters GameBoost;
        // leaving a game exits it.
        if self.cfg.game.boost_on_start {
            return;
        }
        let name = self
            .table
            .name(ev.pid)
            .map(|s| s.to_string())
            .unwrap_or_default();
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
        // Network QoS tweaks are opt-in (`cfg.network.enabled`) and applied only
        // on the Normal -> GameBoost transition. Failures are logged, never fatal
        // to the game flow. Stored so `exit_game_mode` can revert them.
        if self.cfg.network.enabled {
            match crate::network::apply(self.cfg.network.nagle, self.cfg.network.netbios) {
                Ok(backup) => {
                    // Persist the backup as a crash marker so a service death
                    // mid-GameBoost can be reconciled (reverted) on the next
                    // startup. Guard: never write an empty marker — an apply
                    // that modified nothing has nothing to reconcile. A
                    // marker-write failure is logged, never fatal to the game
                    // flow (a missing marker just means no auto-revert).
                    if !backup.is_empty() {
                        if let Err(e) =
                            crate::network::write_marker(&backup, &self.network_marker)
                        {
                            crate::log::warn(format!("network marker write failed: {e}"));
                        }
                    }
                    self.network_backup = Some(backup);
                }
                Err(e) => {
                    crate::log::warn(format!("network QoS apply failed: {e}"));
                    self.network_backup = None;
                }
            }
        }
        // Standby memory purge is opt-in (`[game] purge_standby_on_boost`) and
        // fires once on the Normal -> GameBoost transition. Not reversible, but
        // harmless (the OS rebuilds its standby list). Failures are logged and
        // never fatal to the game flow.
        if self.cfg.game.purge_standby_on_boost {
            if let Err(e) = crate::actions::purge_standby_list() {
                crate::log::warn(format!("standby purge failed: {e}"));
            }
        }
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
        self.foreground_resumed.clear();
        self.last_foreground = None;
        // Revert any network QoS registry tweaks applied on entry. `revert`
        // logs and swallows per-entry failures — never fail the game flow —
        // and returns how many entries were actually reverted.
        if let Some(backup) = self.network_backup.take() {
            let reverted = crate::network::revert(&backup);
            // The marker is only stale once every tweak is fully reverted. On a
            // partial revert, leave it in place so the next startup reconcile
            // retries the remaining values — clearing it would strand them with
            // no reconcile path, the exact failure mode the marker exists to
            // recover.
            if reverted == backup.len() {
                crate::network::remove_marker(&self.network_marker);
            }
        }
        for (pid, state) in std::mem::take(&mut self.boosted) {
            let _ = self.backend.restore(pid, &state);
        }
        // Un-cap and release every Job Object after restoring (restore clears
        // each process's quota via QosCpuQuota{percent:0}; this is the global
        // teardown). No KILL_ON_JOB_CLOSE, so nothing is terminated.
        self.backend.clear_all_qos();
    }

    fn apply_background_to(&mut self, pid: u32, name: &str) {
        if self.is_protected(name) {
            return;
        }
        let rule_idx = match self.first_matching_background(name) {
            Some(i) => i,
            None => return,
        };
        let rule = match self.background_rule_at(rule_idx) {
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

    /// Re-enter GameBoost for every still-running game after a config reload or
    /// save. `set_config` exits GameBoost (restore + revert) to swap in the new
    /// config, but a game already running produces no new Start/foreground
    /// event, so without re-entry the optimization stays OFF until the game
    /// restarts. Scans the process table for every process matching the CURRENT
    /// game matcher; `enter_game_mode` boosts the first (and applies the network
    /// QoS tweaks / standby purge once on the Normal -> GameBoost transition),
    /// then the remaining matching pids are appended to `game_pids` so any of
    /// them exiting later ends GameBoost. No-op when already GameBoost.
    pub fn reenter_if_game_running(&mut self) {
        if self.mode == Mode::GameBoost {
            return;
        }
        // Collect every matching game (owned) so the table borrow is released
        // before `enter_game_mode` takes `&mut self`.
        let games: Vec<(u32, String)> = self
            .table
            .iter()
            .filter(|(_, name, _)| self.is_game(name))
            .map(|(pid, name, _)| (pid, name.to_string()))
            .collect();
        let Some((first_pid, first_name)) = games.first().cloned() else {
            return;
        };
        self.enter_game_mode(first_pid, &first_name);
        for (pid, _) in games.into_iter().skip(1) {
            if !self.game_pids.contains(&pid) {
                self.game_pids.push(pid);
            }
        }
    }

    pub fn set_config(&mut self, cfg: Config) {
        self.exit_game_mode();
        self.cfg = cfg;
        self.matcher = PatternMatcher::new(self.cfg.game.processes.clone());
        self.protected = self.cfg.protected_set();
        self.rebuild_matchers();
        self.reenter_if_game_running();
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

    impl QosLifecycle for RecordingBackend {}

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
            game: GameConfig {
                boost_on_start: true,
                processes: vec!["game.exe".into()],
                purge_standby_on_boost: false,
            },
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
            network: crate::config::NetworkConfig::default(),
            overlay: crate::config::OverlayConfig::default(),
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
    fn smart_suspend_resumes_on_foreground_and_reesuspends_on_leave() {
        let backend = RecordingBackend::default();
        let mut eng = PolicyEngine::new(cfg(), backend.clone());
        eng.on_process_event(&start(100, "browser.exe"));
        eng.on_process_event(&start(200, "game.exe")); // GameBoost; browser suspended
        assert!(eng.boosted().get(&100).map(|s| s.suspended).unwrap_or(false));

        // User brings the suspended browser to the foreground -> resumed, not frozen.
        eng.on_foreground(&ForegroundEvent { pid: 100 });
        let calls = backend.calls();
        assert!(
            calls.iter().any(|c| c.pid == 100 && c.action == Some(TargetAction::Resume)),
            "foreground process must be resumed"
        );

        // Foreground moves elsewhere -> the browser is re-suspended (back to zero).
        let mark = backend.calls().len();
        eng.on_foreground(&ForegroundEvent { pid: 300 });
        let calls = backend.calls();
        assert!(
            calls.iter().skip(mark).any(|c| c.pid == 100 && c.action == Some(TargetAction::Suspend)),
            "leaving foreground must re-suspend"
        );

        // Game exits -> everything restored.
        eng.on_process_event(&stop(200, "game.exe"));
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
    fn boost_on_start_false_defers_to_foreground() {
        let mut c = cfg();
        c.game.boost_on_start = false;
        let backend = RecordingBackend::default();
        let mut eng = PolicyEngine::new(c, backend.clone());
        eng.on_process_event(&start(200, "game.exe"));
        assert_eq!(eng.mode(), Mode::Normal, "start event must NOT enter GameBoost when boost_on_start=false");
        eng.on_foreground(&ForegroundEvent { pid: 200 });
        assert_eq!(eng.mode(), Mode::GameBoost, "foreground event enters GameBoost");
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
        // The game exits before the reload, so there is no running process to
        // re-enter GameBoost for — the reload must leave the engine Normal with
        // every snapshot restored (re-enter must NOT fire).
        eng.on_process_event(&stop(200, "game.exe"));
        assert_eq!(eng.mode(), Mode::Normal);
        eng.set_config(cfg());
        assert_eq!(eng.mode(), Mode::Normal);
        assert!(eng.boosted().is_empty());
        let calls = backend.calls();
        assert!(calls.iter().any(|c| c.pid == 100 && c.restore.is_some()));
    }

    #[test]
    fn set_config_reenters_gameboost_for_running_game() {
        // A config reload/save swaps the matcher and exits GameBoost, but a
        // game that is still running emits no new Start/foreground event, so the
        // engine must scan the process table and re-enter GameBoost for it.
        let backend = RecordingBackend::default();
        let mut eng = PolicyEngine::new(cfg(), backend.clone());
        eng.on_process_event(&start(100, "browser.exe"));
        eng.on_process_event(&start(200, "game.exe"));
        assert_eq!(eng.mode(), Mode::GameBoost);
        eng.set_config(cfg());
        assert_eq!(eng.mode(), Mode::GameBoost, "running game must re-enter after reload");
        assert!(eng.boosted().contains_key(&100), "browser re-boosted on re-entry");
        let calls = backend.calls();
        assert!(calls
            .iter()
            .any(|c| c.pid == 100 && c.action == Some(TargetAction::Suspend)));
    }

    #[test]
    fn set_config_reenters_gameboost_for_all_running_games() {
        // Two games running simultaneously: a config reload exits GameBoost and
        // must re-enter for BOTH, so stopping either one (here the second)
        // ends GameBoost cleanly rather than leaving it wedged on because the
        // second pid was never tracked.
        let mut c = cfg();
        c.game.processes = vec!["game.exe".into(), "game2.exe".into()];
        let backend = RecordingBackend::default();
        let mut eng = PolicyEngine::new(c.clone(), backend.clone());
        eng.on_process_event(&start(100, "browser.exe"));
        eng.on_process_event(&start(200, "game.exe"));
        eng.on_process_event(&start(201, "game2.exe"));
        assert_eq!(eng.mode(), Mode::GameBoost);

        eng.set_config(c.clone()); // both games still match the new matcher
        assert_eq!(eng.mode(), Mode::GameBoost, "running games must re-enter after reload");

        // Stopping the SECOND tracked game exits GameBoost (both pids tracked).
        eng.on_process_event(&stop(201, "game2.exe"));
        assert_eq!(eng.mode(), Mode::Normal, "stopping a tracked game ends GameBoost");
        assert!(eng.boosted().is_empty(), "boosted map cleared when GameBoost ends");
    }

    #[test]
    fn combined_matcher_first_match_order() {
        // background rule order matters: first matching rule wins. Overwrite the
        // shared `cfg()` background (which already carries a suspend=true
        // "browser.exe" rule) so the scenario is exactly: "browser" (idx 0,
        // suspend=false) is the earliest match for "browser.exe".
        let mut c = cfg();
        c.background = vec![
            BackgroundRule { name: "browser".into(), suspend: false, ..Default::default() },
            BackgroundRule { name: "browser.exe".into(), suspend: true, ..Default::default() },
        ];
        let backend = RecordingBackend::default();
        let mut eng = PolicyEngine::new(c, backend.clone());
        eng.on_process_event(&start(200, "game.exe"));
        eng.on_process_event(&start(100, "browser.exe"));
        let calls = backend.calls();
        // First rule ("browser") matches "browser.exe"; suspend is false there,
        // so no Suspend applied.
        assert!(!calls.iter().any(|c| c.pid == 100 && c.action == Some(TargetAction::Suspend)));
    }

    #[test]
    fn overlapping_rules_preserve_config_order_precedence() {
        // A later-index pattern that matches earlier in the name must not win:
        // config order (earliest pattern index) decides, matching the pre-cache
        // `find_background_rule` behavior. Here "updater" (idx 1) is a prefix of
        // "updater.exe" but rule 0 ("updater.exe") is the earliest match.
        let mut c = cfg();
        c.background = vec![
            BackgroundRule { name: "updater.exe".into(), suspend: true, ..Default::default() },
            BackgroundRule { name: "updater".into(), suspend: false, ..Default::default() },
        ];
        let backend = RecordingBackend::default();
        let mut eng = PolicyEngine::new(c, backend.clone());
        eng.on_process_event(&start(200, "game.exe"));
        eng.on_process_event(&start(100, "updater.exe"));
        let calls = backend.calls();
        // Rule 0 ("updater.exe", suspend) is the earliest match, so Suspend IS applied.
        assert!(calls.iter().any(|c| c.pid == 100 && c.action == Some(TargetAction::Suspend)));
    }

    #[test]
    fn is_protected_is_allocation_free_on_hit() {
        // Sanity: protected membership works case-insensitively via hash, no panic.
        let c = cfg();
        let eng = PolicyEngine::new(c, RecordingBackend::default());
        assert!(eng.is_protected_for_test("CSRSS.EXE"));
        assert!(!eng.is_protected_for_test("browser.exe"));
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
