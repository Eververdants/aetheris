//! Engine-level boost/restore regression test (Critical 1).
//!
//! Drives a real `PolicyEngine<OsBackend>`: a real `dummy_proc` helper is
//! suspended + down-prioritized (+ QoS attempted) when the game starts, and
//! must be resumed, re-prioritized, and QoS-cleared when the game exits.
//! Without the Critical 1 fix the stored `ProcState` was the pre-action
//! snapshot (`suspended: false`, `qos_percent: None`), so `restore()` never
//! issued a Resume or a QoS clear — a suspended process stayed frozen forever.

use std::process::{Child, Command};
use std::time::Duration;

use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::JobObjects::{
    QueryInformationJobObject, JobObjectCpuRateControlInformation,
    JOBOBJECT_CPU_RATE_CONTROL_INFORMATION,
};
use windows::Win32::System::Threading::{
    GetPriorityClass, GetThreadTimes, OpenProcess, OpenThread, NORMAL_PRIORITY_CLASS,
    PROCESS_QUERY_INFORMATION, THREAD_ALL_ACCESS,
};

use aetheris_core::actions::{OsBackend, ProcessBackend, TargetAction};
use aetheris_core::config::{BackgroundRule, Config, GameConfig, PriorityClass};
use aetheris_core::events::{ProcessEvent, ProcessKind};
use aetheris_core::policy::{Mode, PolicyEngine};

/// Kills and reaps the child even if the test panics mid-way, keeping the test
/// hermetic (no orphaned dummy_proc).
struct ChildGuard(Child);

impl Drop for ChildGuard {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
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
            name: "dummy_proc.exe".into(),
            suspend: true,
            priority: Some(PriorityClass::BelowNormal),
            affinity: None,
            qos_cpu_quota: Some(50),
            trim_memory: false,
        }],
        rule: vec![],
        protected_extra: vec![],
        network: aetheris_core::config::NetworkConfig::default(),
    }
}

fn start(pid: u32, name: &str) -> ProcessEvent {
    ProcessEvent { pid, name: name.into(), parent_pid: 0, kind: ProcessKind::Start }
}

fn stop(pid: u32, name: &str) -> ProcessEvent {
    ProcessEvent { pid, name: name.into(), parent_pid: 0, kind: ProcessKind::Stop }
}

fn first_thread_id(pid: u32) -> Option<u32> {
    // Toolhelp snapshot to find one thread id of the process (same technique as
    // tests/actions_suspend_qos.rs).
    unsafe {
        let snapshot = windows::Win32::System::Diagnostics::ToolHelp::CreateToolhelp32Snapshot(
            windows::Win32::System::Diagnostics::ToolHelp::TH32CS_SNAPTHREAD,
            0,
        )
        .expect("snap");
        let mut entry = windows::Win32::System::Diagnostics::ToolHelp::THREADENTRY32 {
            dwSize: std::mem::size_of::<windows::Win32::System::Diagnostics::ToolHelp::THREADENTRY32>()
                as u32,
            ..Default::default()
        };
        let mut result = None;
        if windows::Win32::System::Diagnostics::ToolHelp::Thread32First(snapshot, &mut entry)
            .is_ok()
        {
            loop {
                if entry.th32OwnerProcessID == pid {
                    result = Some(entry.th32ThreadID);
                    break;
                }
                if windows::Win32::System::Diagnostics::ToolHelp::Thread32Next(snapshot, &mut entry)
                    .is_err()
                {
                    break;
                }
            }
        }
        let _ = CloseHandle(snapshot);
        result
    }
}

/// Kernel + user CPU time for the process's first thread (100ns units).
fn busy_time(pid: u32) -> u128 {
    let tid = first_thread_id(pid).expect("thread");
    let h = unsafe { OpenThread(THREAD_ALL_ACCESS, false, tid) }.expect("open thread");
    let mut creation = windows::Win32::Foundation::FILETIME::default();
    let mut exit = windows::Win32::Foundation::FILETIME::default();
    let mut kernel = windows::Win32::Foundation::FILETIME::default();
    let mut user = windows::Win32::Foundation::FILETIME::default();
    unsafe { GetThreadTimes(h, &mut creation, &mut exit, &mut kernel, &mut user) }
        .expect("thread times");
    let _ = unsafe { CloseHandle(h) };
    let kt = ((kernel.dwHighDateTime as u128) << 32) | kernel.dwLowDateTime as u128;
    let ut = ((user.dwHighDateTime as u128) << 32) | user.dwLowDateTime as u128;
    kt + ut
}

fn priority_class(pid: u32) -> u32 {
    let h = unsafe { OpenProcess(PROCESS_QUERY_INFORMATION, false, pid) }.expect("open process");
    let c = unsafe { GetPriorityClass(h) };
    let _ = unsafe { CloseHandle(h) };
    c
}

#[test]
fn boost_suspends_then_exit_resumes_and_restores() {
    let guard = ChildGuard(
        Command::new(env!("CARGO_BIN_EXE_dummy_proc")).spawn().expect("spawn dummy"),
    );
    let bg_pid = guard.0.id();

    let mut engine = PolicyEngine::new(cfg(), OsBackend::new());

    // 1. Background helper starts while still in Normal mode.
    engine.on_process_event(&start(bg_pid, "dummy_proc.exe"));
    assert_eq!(engine.mode(), Mode::Normal);

    // Warm up so busy-time is already ticking before we freeze it.
    std::thread::sleep(Duration::from_millis(200));

    // 2. Game starts -> GameBoost: dummy is suspended + down-prioritized.
    engine.on_process_event(&start(9000, "game.exe"));
    assert_eq!(engine.mode(), Mode::GameBoost);
    assert!(engine.boosted().contains_key(&bg_pid));

    // 3. Suspended: CPU time must stop advancing.
    let t1 = busy_time(bg_pid);
    std::thread::sleep(Duration::from_millis(300));
    let t2 = busy_time(bg_pid);
    // Generous ceiling: NtSuspendProcess charges a running thread the remainder
    // of its time slice (one ~15.6ms quantum was observed under load) as a
    // one-time accounting artifact. A running process accrues ~300ms
    // (3,000,000 units) in this window, so < 500_000 still separates the two.
    assert!(
        t2 - t1 < 500_000,
        "boosted background process must be frozen (t2-t1={})",
        t2 - t1
    );

    // 4. Game exits -> Normal: the engine must restore every boosted process.
    engine.on_process_event(&stop(9000, "game.exe"));
    assert_eq!(engine.mode(), Mode::Normal);
    assert!(engine.boosted().is_empty(), "boosted map must be cleared on game exit");

    // 5. Resumed: CPU time advances again.
    std::thread::sleep(Duration::from_millis(300));
    let t3 = busy_time(bg_pid);
    assert!(
        t3 - t2 > 50_000,
        "restored process must accrue CPU time again (t3-t2={})",
        t3 - t2
    );

    // 5b. Priority restored to NORMAL (BelowNormal was applied during boost).
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(
        priority_class(bg_pid),
        NORMAL_PRIORITY_CLASS.0,
        "priority must be restored to NORMAL after game exit"
    );

    // 5c. QoS: the engine's restore reverses whatever QoS it applied via
    // QosCpuQuota{percent:0} and exit_game_mode clears all jobs. The job entry
    // for the dummy must exist during boost and be gone after exit (asserted in
    // `qos_job_lifecycle_across_game_exit`). Prove the clear side is sound with
    // a fresh backend: applying then clearing must not error, and clearing an
    // un-applied pid must be a no-op.
    let b = OsBackend::new();
    let _ = b.apply(bg_pid, &TargetAction::QosCpuQuota { percent: 50 });
    b.apply(bg_pid, &TargetAction::QosCpuQuota { percent: 0 })
        .expect("qos clear must never error (no-op or reversal)");

    // 6. Child is killed by the ChildGuard on drop (also on panic).
    drop(guard);
}

/// Engine-level Job Object QoS lifecycle: entering GameBoost must attach the
/// background process to a real Job Object (a cross-process CPU cap, v2-A
/// Task 1), and exiting GameBoost must clear the cap and release the job.
#[test]
fn qos_job_lifecycle_across_game_exit() {
    let guard = ChildGuard(
        Command::new(env!("CARGO_BIN_EXE_dummy_proc")).spawn().expect("spawn dummy"),
    );
    let bg_pid = guard.0.id();

    let mut engine = PolicyEngine::new(cfg(), OsBackend::new());

    // 1. Background helper starts while in Normal mode; then the game starts ->
    //    GameBoost: the engine applies QosCpuQuota{50}, creating a Job Object
    //    and assigning the dummy to it.
    engine.on_process_event(&start(bg_pid, "dummy_proc.exe"));
    engine.on_process_event(&start(9000, "game.exe"));
    assert_eq!(engine.mode(), Mode::GameBoost);
    assert!(engine.boosted().contains_key(&bg_pid));

    // A job entry must exist for the dummy. (If the host forbids the attach
    // because the test harness already runs inside a job, the entry still
    // exists with assigned=false — either way a job was created.)
    let jobs = engine.backend().jobs.lock().unwrap();
    let entry = jobs.get(&bg_pid).expect("job entry must exist while boosting");
    let rate = unsafe {
        let mut info = JOBOBJECT_CPU_RATE_CONTROL_INFORMATION::default();
        QueryInformationJobObject(
            Some(entry.job),
            JobObjectCpuRateControlInformation,
            (&mut info as *mut JOBOBJECT_CPU_RATE_CONTROL_INFORMATION).cast(),
            std::mem::size_of::<JOBOBJECT_CPU_RATE_CONTROL_INFORMATION>() as u32,
            None,
        )
        .expect("query job while boosting");
        info.Anonymous.CpuRate
    };
    assert_eq!(rate, 5000, "engine must apply the configured 50% hard cap");
    drop(jobs);

    // 2. Game exits -> Normal: the engine restores every boosted process and
    //    clears all QoS (rate control disabled, job handles released).
    engine.on_process_event(&stop(9000, "game.exe"));
    assert_eq!(engine.mode(), Mode::Normal);
    assert!(engine.boosted().is_empty(), "boosted map must be cleared on game exit");
    assert!(
        engine.backend().jobs.lock().unwrap().is_empty(),
        "all QoS job entries must be released on game exit"
    );

    drop(guard);
}

/// Regression (Ctrl-C / `ServiceMsg::Stop`): the service must restore every
/// boosted process when it shuts down, not only when the game exits. The stop
/// path calls `PolicyEngine::exit_game_mode()` directly; without the fix the
/// main loop broke on `ServiceMsg::Stop` without ever exiting game mode, so a
/// suspended dummy_proc stayed frozen (and down-prioritized) after Ctrl-C.
#[test]
fn stop_path_restores_boosted_processes() {
    let guard = ChildGuard(
        Command::new(env!("CARGO_BIN_EXE_dummy_proc")).spawn().expect("spawn dummy"),
    );
    let bg_pid = guard.0.id();

    let mut engine = PolicyEngine::new(cfg(), OsBackend::new());

    // 1. Background helper starts, then the game starts -> GameBoost: dummy is
    //    suspended + down-prioritized.
    engine.on_process_event(&start(bg_pid, "dummy_proc.exe"));
    std::thread::sleep(Duration::from_millis(200)); // warm up busy-time
    engine.on_process_event(&start(9000, "game.exe"));
    assert_eq!(engine.mode(), Mode::GameBoost);
    assert!(engine.boosted().contains_key(&bg_pid));

    // 2. Suspended: CPU time must stop advancing.
    let t1 = busy_time(bg_pid);
    std::thread::sleep(Duration::from_millis(300));
    let t2 = busy_time(bg_pid);
    assert!(
        t2 - t1 < 500_000,
        "boosted background process must be frozen (t2-t1={})",
        t2 - t1
    );

    // 3. Simulate the stop path: the service calls `exit_game_mode()` directly
    //    (what `ServiceMsg::Stop` now routes to) before breaking the loop.
    engine.exit_game_mode();
    assert_eq!(engine.mode(), Mode::Normal);
    assert!(engine.boosted().is_empty(), "boosted map must be cleared on stop");

    // 4. Resumed: CPU time advances again.
    std::thread::sleep(Duration::from_millis(300));
    let t3 = busy_time(bg_pid);
    assert!(
        t3 - t2 > 50_000,
        "restored process must accrue CPU time again (t3-t2={})",
        t3 - t2
    );

    // 5. Priority restored to NORMAL (BelowNormal was applied during boost).
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(
        priority_class(bg_pid),
        NORMAL_PRIORITY_CLASS.0,
        "priority must be restored to NORMAL after stop"
    );

    // 6. Child is killed by the ChildGuard on drop (also on panic).
    drop(guard);
}
