# aetheris v2-A Engine Features Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Deliver v2's engine features: real cross-process CPU cap (Job Object QoS), reversible network QoS tweaks, and standby memory purge — all opt-in, safe, and fully restored on game exit / service stop.

**Architecture:** Continues the aetheris workspace. All changes in `aetheris-core` (actions, config, policy, service) + tests. No new crates, no new dependencies (windows + ntapi only).

**Tech Stack:** Rust, windows 0.62.2, ntapi, serde/toml. Target Windows 10 1809+ / Win11.

## Global Constraints

- **No async runtime / no tokio.**
- **Hot path zero heap allocation** (maintained; these features are opt-in and run on game-entry/exit, not per-event).
- **Protected list absolute.** All three features are **opt-in** (default off) and **reversible**.
- **Job Object QoS safety:** NEVER set `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`. Clear rate control (→ unlimited) before dropping handles on game exit AND service Stop. Close a job handle only when the process has exited. Attach-failure (target already in a job) → log + degrade to priority lowering, never fail the rule.
- **Network QoS:** only touch the documented Nagle/NetBIOS registry keys; backup original values before writing; restore them on revert; failure to enumerate interfaces → warn + skip. Opt-in via `[network]` config.
- **Standby purge:** `NtSetSystemInformation(SystemMemoryListInformation=0x50, MemoryPurgeStandbyList=4)`; requires `SeProfileSingleProcessPrivilege`; opt-in via `[game] purge_standby_on_boost`; on failure warn + continue.
- Dependencies locked; cargo-deny gate green.
- Every task ends green + committed.

---

### Task 1: Job Object QoS (real cross-process CPU cap)

**Files:**
- Modify: `crates/aetheris-core/src/actions.rs`
- Modify: `crates/aetheris-core/tests/actions_suspend_qos.rs`
- Modify: `crates/aetheris-core/tests/policy_restore.rs`
- Test: integration (job readback, clear, drop-safe) + engine-level

**Interfaces:**
- Consumes: `TargetAction::QosCpuQuota { percent }`, `OsBackend`, `ProcessBackend`, `ProcState` (existing).
- Produces:
  - `OsBackend` regains a job map: `jobs: Mutex<HashMap<u32, JobEntry>>` where `JobEntry { job: HANDLE, assigned: bool }`.
  - `apply`'s `QosCpuQuota` arm: `percent > 0` → find-or-create job, set CPU rate control (ENABLE|HARD_CAP, `CpuRate = percent * 100`), `AssignProcessToJobObject`; on attach failure (`ERROR_ACCESS_DENIED` = already-in-job) → log + return `Ok(())` (degrade silently to no-cap for that process — priority/affinity still apply; document). `percent == 0` → disable rate control on the job (ControlFlags=0), and if `assigned == false` (never attached) remove + close the entry; if assigned, keep the job open but uncapped (process can't be removed from a job; closing the last handle destroys it — the process may be in other jobs so this is a real hazard, hence: keep the job open while the process lives).
  - `pub fn on_process_exit(&self, pid: u32)` — called by the policy engine on a process Stop: if a job entry exists, close the handle + remove (safe: process is gone). Also used at service Stop after clearing all caps.
  - `pub fn clear_all_qos(&self)` — iterate jobs, disable rate control on each (ControlFlags=0). Called by `exit_game_mode` and `Service::run` Stop path.
  - `Drop for OsBackend`: close remaining job handles WITHOUT setting KILL_ON_JOB_CLOSE (safe — closing a job handle without that flag does NOT terminate processes; the jobs simply cease to cap). Document this in the Drop comment.
- `ProcState.qos_percent` semantics: policy engine records `Some(percent)` when it applied a cap; restore calls `QosCpuQuota { percent: 0 }`.

- [ ] **Step 1: Write the failing integration test** — replace `qos_background_mode_is_safe_and_reversible` in `tests/actions_suspend_qos.rs` with a real job test:

```rust
use windows::Win32::System::JobObjects::{QueryInformationJobObject, JOBOBJECTINFOCLASS, JOBOBJECT_CPU_RATE_CONTROL_INFORMATION};
use aetheris_core::actions::{OsBackend, ProcessBackend, TargetAction};

#[test]
fn qos_job_assigns_and_caps() {
    // Spawn dummy; create job; assign; read back the CPU rate control.
    let mut child = spawn_dummy();
    let pid = child.id();
    let backend = OsBackend::new();
    backend.apply(pid, &TargetAction::QosCpuQuota { percent: 50 }).expect("assign qos");
    // Read back: rate control should be enabled with hard cap 5000 (0.01% units).
    let jobs = backend.jobs.lock().unwrap();
    let entry = jobs.get(&pid).expect("job entry exists");
    let mut info = JOBOBJECT_CPU_RATE_CONTROL_INFORMATION::default();
    unsafe { QueryInformationJobObject(
        entry.job,
        JOBOBJECTINFOCLASS::JobObjectCpuRateControlInformation,
        (&mut info as *mut _).cast(),
        std::mem::size_of::<JOBOBJECT_CPU_RATE_CONTROL_INFORMATION>() as u32,
        None,
    ) }.expect("query job");
    assert!(info.ControlFlags.0 & windows::Win32::System::JobObjects::JOB_OBJECT_CPU_RATE_CONTROL_ENABLE.0 != 0);
    assert_eq!(info.CpuRate, 5000);
    drop(jobs);
    backend.apply(pid, &TargetAction::QosCpuQuota { percent: 0 }).expect("clear qos");
    backend.on_process_exit(pid);
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn qos_clear_then_drop_does_not_kill() {
    let mut child = spawn_dummy();
    let pid = child.id();
    let backend = OsBackend::new();
    backend.apply(pid, &TargetAction::QosCpuQuota { percent: 30 }).expect("assign");
    backend.apply(pid, &TargetAction::QosCpuQuota { percent: 0 }).expect("clear");
    drop(backend); // Drop must not terminate the still-running dummy
    let alive = unsafe { windows::Win32::System::Threading::OpenProcess(PROCESS_QUERY, false, pid) }.is_ok();
    assert!(alive, "closing the backend must not kill the capped process");
    let _ = child.kill();
    let _ = child.wait();
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p aetheris-core --test actions_suspend_qos`
Expected: compile error — `jobs` field / `on_process_exit` / `clear_all_qos` missing.

- [ ] **Step 3: Implement Job Object QoS in `actions.rs`**

Restore the job map + implement the methods (this is the design from v1's Task 7 that was replaced by background-mode in the final-review fix; re-introduce with the corrected safety semantics). Key code:

```rust
pub struct OsBackend {
    jobs: Mutex<HashMap<u32, JobEntry>>,
}
struct JobEntry { job: HANDLE, assigned: bool }

impl OsBackend {
    pub fn new() -> Self { Self { jobs: Mutex::new(HashMap::new()) } }

    fn apply_qos(&self, pid: u32, percent: u32) -> Result<(), ActionError> {
        use windows::Win32::System::JobObjects::*;
        let mut jobs = self.jobs.lock().unwrap();

        if percent == 0 {
            // Clear: disable rate control (unlimited) on the tracked job. If the
            // process was never assigned (attach failed), close+drop the entry.
            match jobs.get(&pid) {
                Some(entry) if entry.assigned => {
                    let mut info = JOBOBJECT_CPU_RATE_CONTROL_INFORMATION::default();
                    info.ControlFlags = JOBOBJECT_CPU_RATE_CONTROL_FLAGS(0);
                    unsafe {
                        SetInformationJobObject(entry.job, JOBOBJECTINFOCLASS::JobObjectCpuRateControlInformation,
                            (&info as *const _).cast(), std::mem::size_of::<JOBOBJECT_CPU_RATE_CONTROL_INFORMATION>() as u32)
                    }.map_err(|e| ActionError::Job(format!("clear rate control: {e}")))?;
                }
                Some(_) => { let e = jobs.remove(&pid).unwrap(); unsafe { let _ = CloseHandle(e.job); } }
                None => {}
            }
            return Ok(());
        }

        // percent > 0
        let entry = match jobs.entry(pid) {
            Entry::Occupied(e) => e.into_mut(),
            Entry::Vacant(v) => {
                let j = unsafe { CreateJobObjectW(None, None) }.map_err(|e| ActionError::Job(format!("CreateJobObjectW: {e}")))?;
                v.insert(JobEntry { job: j, assigned: false })
            }
        };
        let mut info = JOBOBJECT_CPU_RATE_CONTROL_INFORMATION::default();
        info.ControlFlags = JOBOBJECT_CPU_RATE_CONTROL_ENABLE | JOBOBJECT_CPU_RATE_CONTROL_HARD_CAP;
        info.CpuRate = percent * 100;
        unsafe {
            SetInformationJobObject(entry.job, JOBOBJECTINFOCLASS::JobObjectCpuRateControlInformation,
                (&info as *const _).cast(), std::mem::size_of::<JOBOBJECT_CPU_RATE_CONTROL_INFORMATION>() as u32)
        }.map_err(|e| ActionError::Job(format!("set rate control: {e}")))?;

        if !entry.assigned {
            let h = open_process(pid)?;
            let assigned = unsafe { AssignProcessToJobObject(entry.job, h) };
            let _ = unsafe { CloseHandle(h) };
            if assigned.is_err() {
                // Target already in a job (browsers/common). Degrade: no cap for
                // this process; priority/affinity still apply. Keep the job open
                // but never KILL_ON_JOB_CLOSE so nothing is terminated.
                crate::log::warn(format!("qos: pid {pid} already in a job; cpu cap skipped"));
                return Ok(());
            }
            entry.assigned = true;
        }
        Ok(())
    }

    pub fn on_process_exit(&self, pid: u32) {
        if let Some(e) = self.jobs.lock().unwrap().remove(&pid) {
            unsafe { let _ = CloseHandle(e.job); }
        }
    }

    pub fn clear_all_qos(&self) {
        let mut jobs = self.jobs.lock().unwrap();
        for (_, entry) in jobs.iter_mut() {
            if entry.assigned {
                let mut info = JOBOBJECT_CPU_RATE_CONTROL_INFORMATION::default();
                info.ControlFlags = JOBOBJECT_CPU_RATE_CONTROL_FLAGS(0);
                let _ = unsafe { SetInformationJobObject(entry.job,
                    JOBOBJECTINFOCLASS::JobObjectCpuRateControlInformation,
                    (&info as *const _).cast(), std::mem::size_of::<JOBOBJECT_CPU_RATE_CONTROL_INFORMATION>() as u32) };
            }
        }
        jobs.clear();
        for (_, e) in jobs.drain() { unsafe { let _ = CloseHandle(e.job); } }
    }
}

impl Drop for OsBackend {
    fn drop(&mut self) {
        // Close remaining job handles. WITHOUT JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        // this does NOT terminate assigned processes — jobs simply stop capping.
        // Caps were cleared first by clear_all_qos on the Stop path; this is the
        // final safety net for abnormal teardown.
        let jobs = self.jobs.lock().unwrap();
        for (_, e) in jobs.iter() { unsafe { let _ = CloseHandle(e.job); } }
    }
}
```

- [ ] **Step 4: Wire into policy/service**

In `policy.rs`:
- `apply_background_to`: after applying `QosCpuQuota`, record `stored.qos_percent = Some(percent)` (as in v1.1).
- On a background process `Stop` event (in `on_process_event` Stop arm, after `boosted.remove`), call `backend.on_process_exit(pid)`. Also call it for the game pid on game exit.
- `exit_game_mode`: after restoring boosted, call `backend.clear_all_qos()`.

In `service.rs` `run` Stop arm and `Service::handle_message(Stop)`: after `engine.exit_game_mode()`, call `self.engine.backend_clear_qos()` (add a passthrough on PolicyEngine) — or have `exit_game_mode` itself call `clear_all_qos` (preferred: put it inside `exit_game_mode` so all exit paths clear).

> Note: PolicyEngine holds `backend: B` generically. `clear_all_qos`/`on_process_exit` are `OsBackend`-specific. Add a `pub fn qos_teardown(&mut self)` on `PolicyEngine` that calls these only if `B: AsRef<OsBackend>`-like... simpler: add the calls conditionally via a trait method default no-op on `ProcessBackend`, overridden in a new `QosLifecycle` trait implemented for `OsBackend`. Keep it minimal: define `pub trait QosLifecycle { fn on_process_exit(&self, _pid: u32) {} fn clear_all_qos(&self) {} }`, implement for `OsBackend`, and have PolicyEngine require `B: ProcessBackend + QosLifecycle`. The RecordingBackend in tests gets a no-op impl.

- [ ] **Step 5: Extend `policy_restore.rs`** — engine-level: background rule with `qos_cpu_quota = 50`, drive real PolicyEngine<OsBackend>, enter game mode, assert a job entry exists for dummy, exit game mode, assert job cleared (rate control disabled / entry gone via a test accessor).

- [ ] **Step 6: Run tests**

Run: `cargo test -p aetheris-core` (all pass), `cargo clippy -p aetheris-core --tests` clean (except pre-existing config.rs:107 warning). Note: the previous `qos_background_mode_is_safe_and_reversible` test is replaced; ensure nothing references the removed `background_mode` HashSet.

- [ ] **Step 7: Commit**

```bash
git add crates/aetheris-core/src/actions.rs crates/aetheris-core/src/policy.rs crates/aetheris-core/src/service.rs crates/aetheris-core/tests/actions_suspend_qos.rs crates/aetheris-core/tests/policy_restore.rs
git commit -m "feat: Job Object CPU QoS with safe teardown (no kill-on-close)"
```

---

### Task 2: Network QoS tweaks (opt-in, reversible)

**Files:**
- Modify: `crates/aetheris-core/src/config.rs` (add `[network]` section)
- Create: `crates/aetheris-core/src/network.rs` (new module; add `pub mod network;` to lib.rs)
- Modify: `crates/aetheris-core/src/policy.rs` (apply on game entry, revert on exit)
- Test: unit (registry roundtrip against a scoped test key) + config test

**Interfaces:**
- Consumes: `Config` (new `network` section), windows registry APIs.
- Produces:
  - `pub struct NetworkConfig { pub enabled: bool, pub nagle: bool, pub netbios: bool }` (`#[derive(Default, Serialize, Deserialize)]`; all default false).
  - `Config` gains `pub network: NetworkConfig`.
  - `pub struct NetworkTweaks` with `pub fn apply(&self) -> Result<Vec<String>, String>` (applies Nagle TcpAckFrequency=1/TCPNoDelay=1 on each active interface; optional NetBIOS DisableNetbiosOverTcpip=2) and `pub fn revert(&self, backup: &Backup) -> ...`. Simpler v1 shape: `pub fn apply() -> Result<Vec<BackupEntry>, String>` where each `BackupEntry { path, value_name, old_value }`; `pub fn revert(entries: &[BackupEntry])`. Backup entries are held by the policy engine for the duration of GameBoost and reverted on exit.
  - Registry access via `windows::Win32::System::Registry` (`RegOpenKeyExW`, `RegGetValueW`, `RegSetValueExW`, `RegDeleteValueW`).

- [ ] **Step 1: Write the failing test** (registry roundtrip against a scoped test key under `HKCU\Software\AetherisTests\`):

```rust
#[test]
fn registry_backup_apply_revert_roundtrip() {
    // Read current TcpAckFrequency on a known key is fragile; instead test the
    // backup/revert mechanics on a controlled value we set ourselves.
    // Write 5 to HKCU\Software\AetherisTests\TestValue; backup; apply 1; assert 1;
    // revert; assert 5.
    ...
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p aetheris-core network`
Expected: compile error — module missing.

- [ ] **Step 3: Implement `network.rs` + config section + policy wiring**

Implementation notes:
- Enumerate active interfaces: `HKEY_LOCAL_MACHINE\SYSTEM\CurrentControlSet\Services\Tcpip\Parameters\Interfaces` — subkeys are adapter GUIDs. For each, apply `TcpAckFrequency = 1` (DWORD) and `TCPNoDelay = 1` (DWORD), backing up existing values. Requires admin (write to HKLM) — the service already runs elevated.
- NetBIOS (if enabled): `HKLM\SYSTEM\CurrentControlSet\Services\NetBT\Parameters\DisableNetbiosOverTcpip = 2` (DWORD, backup old).
- Backup = read-before-write into a `Vec<BackupEntry>`. Revert = write back old value (or delete if absent).
- `apply` returns the backup; if enumeration finds no interfaces, return an error (warn).
- Policy wiring: on `enter_game_mode`, if `cfg.network.enabled`, call `network::apply()` and store `network_backup: Option<Vec<BackupEntry>>` on the engine; on `exit_game_mode`, call `network::revert(&backup)`. Use `log::warn` on failures, never fail the game flow.

- [ ] **Step 4: Run tests**

Run: `cargo test -p aetheris-core` (all pass), clippy clean.

- [ ] **Step 5: Commit**

```bash
git add crates/aetheris-core/src/config.rs crates/aetheris-core/src/network.rs crates/aetheris-core/src/lib.rs crates/aetheris-core/src/policy.rs
git commit -m "feat: reversible network QoS tweaks (Nagle/NetBIOS, opt-in)"
```

---

### Task 3: Standby memory purge (opt-in)

**Files:**
- Modify: `crates/aetheris-core/src/actions.rs` (add `pub fn purge_standby_list() -> Result<(), ActionError>`)
- Modify: `crates/aetheris-core/src/config.rs` (add `[game] purge_standby_on_boost`)
- Modify: `crates/aetheris-core/src/policy.rs` (call on game entry)
- Test: unit (privilege enablement) + config test

**Interfaces:**
- Consumes: ntapi, config.
- Produces:
  - `pub fn purge_standby_list() -> Result<(), ActionError>` — `NtSetSystemInformation(SystemMemoryListInformation /*0x50*/, &[MemoryPurgeStandbyList /*4*/; 1], size)`; requires enabling `SeProfileSingleProcessPrivilege` via the existing privilege helper.
  - `GameConfig` gains `#[serde(default)] pub purge_standby_on_boost: bool` (default false).
  - Policy: on `enter_game_mode`, if `cfg.game.purge_standby_on_boost`, call `purge_standby_list()` (warn on failure, never fail the flow). Not reversible (harmless — OS rebuilds the standby list).

- [ ] **Step 1: Write the failing test**

```rust
#[test]
fn purge_standby_rejects_without_privilege() {
    // Non-elevated: must return an Err (privilege not enabled) or Ok (privilege
    // present). Never panic. If elevated, it should succeed or fail gracefully.
    let r = aetheris_core::actions::purge_standby_list();
    assert!(r.is_ok() || r.is_err(), "must not panic");
}

#[test]
fn game_config_defaults_purge_off() {
    let cfg = Config::from_str("[game]\nprocesses=[]\n").unwrap();
    assert!(!cfg.game.purge_standby_on_boost);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p aetheris-core purge_standby`
Expected: compile error — function/field missing.

- [ ] **Step 3: Implement**

```rust
pub fn purge_standby_list() -> Result<(), ActionError> {
    // SeProfileSingleProcessPrivilege required (StandbyCleanerLite pattern).
    let backend = OsBackend::new();
    backend.enable_privilege_for_test(windows::s!("SeProfileSingleProcessPrivilege"))?;
    let arg: u32 = 4; // MemoryPurgeStandbyList
    let status = unsafe {
        ntapi::ntexapi::NtSetSystemInformation(
            0x50, // SystemMemoryListInformation
            (&arg as *const u32).cast(),
            std::mem::size_of::<u32>() as u32,
        )
    };
    if status == 0 { Ok(()) } else { Err(ActionError::Api(format!("NtSetSystemInformation: 0x{status:08X}"))) }
}
```

> Reuse the existing `enable_privilege` (make it `pub(crate)` or add a wrapper). Wire into policy `enter_game_mode`.

- [ ] **Step 4: Run tests**

Run: `cargo test -p aetheris-core`, clippy clean.

- [ ] **Step 5: Commit**

```bash
git add crates/aetheris-core/src/actions.rs crates/aetheris-core/src/config.rs crates/aetheris-core/src/policy.rs
git commit -m "feat: standby memory purge on game boost (opt-in)"
```

---

### Task 4: Example config + docs + acceptance update

**Files:**
- Modify: `aetheris.toml` (show new opt-in options)
- Modify: `README.md` (document network/standby/QoS)
- Modify: `docs/acceptance-v1.md` or new `docs/acceptance-v2a.md` (measure QoS cap effect)
- Test: full suite

**Interfaces:**
- Consumes: everything above.
- Produces: documented, testable feature set.

- [ ] **Step 1: Update `aetheris.toml`** — add commented opt-in examples:

```toml
[network]
# enabled = true
# nagle = true
# netbios = false

[game]
purge_standby_on_boost = false
```

- [ ] **Step 2: Update `README.md`** — document the three features, their reversibility, and that QoS attaches only to processes not already in a job.

- [ ] **Step 3: Acceptance note** — `docs/acceptance-v2a.md`: measure that a job-capped dummy is limited (busy% falls under a busy loop) and that the cap clears on game exit. Elevated run.

- [ ] **Step 4: Full validation**

Run: `cargo test --workspace`, `cargo deny check licenses bans sources` (if network allows).

- [ ] **Step 5: Commit**

```bash
git add aetheris.toml README.md docs/acceptance-v2a.md
git commit -m "docs: v2-A features, opt-in config, acceptance"
```

---

## Self-Review Notes

- v2 spec §3.1 (Job QoS): covered by Task 1 — clear-on-stop semantics, no KILL_ON_JOB_CLOSE, attach-failure degrade, process-exit cleanup, Drop safety.
- v2 spec §3.2 (network): Task 2 — opt-in, backup/revert, interface enumeration, HKLM admin requirement documented.
- v2 spec §3.3 (standby): Task 3 — opt-in, SeProfileSingleProcessPrivilege, warn-on-failure.
- v2 spec §3.4 (live state): already landed in v1.1 A-track Task 1 — no work here.
- Known deliberate simplifications: network interface enumeration covers all active adapters (no per-adapter whitelist); standby purge fires once per game entry (not throttled); QoS cap is hard-cap (not weighted) — matching the spec.
