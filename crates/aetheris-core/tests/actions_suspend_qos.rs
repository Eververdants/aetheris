//! Suspend freezes a process (CPU time stops advancing); resume restarts it.
//! QoS throttling is a real cross-process CPU cap via a Job Object: the process
//! is assigned to a job and the job's CPU rate control is read back / cleared.
use std::process::Command;
use std::time::Duration;

use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::JobObjects::{
    QueryInformationJobObject, JobObjectCpuRateControlInformation,
    JOBOBJECT_CPU_RATE_CONTROL_INFORMATION, JOB_OBJECT_CPU_RATE_CONTROL_ENABLE,
};
use windows::Win32::System::Threading::{GetThreadTimes, OpenProcess, OpenThread, THREAD_ALL_ACCESS};

use aetheris_core::actions::{OsBackend, ProcessBackend, TargetAction, PROCESS_QUERY};

fn spawn_dummy() -> std::process::Child {
    let exe = env!("CARGO_BIN_EXE_dummy_proc");
    Command::new(exe).spawn().expect("spawn dummy")
}

fn first_thread_id(pid: u32) -> Option<u32> {
    // Toolhelp snapshot to find one thread id of the process.
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

fn busy_time(pid: u32) -> u128 {
    let tid = first_thread_id(pid).expect("thread");
    let h = unsafe { OpenThread(THREAD_ALL_ACCESS, false, tid) }.expect("open thread");
    let mut creation = windows::Win32::Foundation::FILETIME::default();
    let mut exit = windows::Win32::Foundation::FILETIME::default();
    let mut kernel = windows::Win32::Foundation::FILETIME::default();
    let mut user = windows::Win32::Foundation::FILETIME::default();
    // NOTE (deviation from brief): GetThreadTimes orders its outputs as
    // (creation, exit, kernel, user). The brief summed the first two (creation +
    // exit), which are constant timestamps — that always yields delta 0 and made
    // the resume assert fail. Sum kernel + user time instead.
    unsafe { GetThreadTimes(h, &mut creation, &mut exit, &mut kernel, &mut user) }
        .expect("thread times");
    let _ = unsafe { CloseHandle(h) };
    let kt = ((kernel.dwHighDateTime as u128) << 32) | kernel.dwLowDateTime as u128;
    let ut = ((user.dwHighDateTime as u128) << 32) | user.dwLowDateTime as u128;
    kt + ut
}

#[test]
fn suspend_freezes_and_resume_restarts() {
    let mut child = spawn_dummy();
    let pid = child.id();
    let backend = OsBackend::new();

    // Warm up so busy-time is already ticking.
    std::thread::sleep(Duration::from_millis(200));
    let t1 = busy_time(pid);
    backend.apply(pid, &TargetAction::Suspend).expect("suspend");
    std::thread::sleep(Duration::from_millis(300));
    let t2 = busy_time(pid);
    // Allow a generous ceiling: NtSuspendProcess charges a thread that is
    // currently running the remainder of its time slice (a full ~15.6ms quantum
    // was observed under parallel load), which is a one-time accounting artifact
    // of suspension. A genuinely running process accrues ~300ms (3,000,000
    // units) in this same window, so < 500_000 still cleanly separates the two.
    assert!(
        t2 - t1 < 500_000,
        "suspended process must not accrue CPU time (t2-t1={})",
        t2 - t1
    );
    backend.apply(pid, &TargetAction::Resume).expect("resume");
    std::thread::sleep(Duration::from_millis(300));
    let t3 = busy_time(pid);
    assert!(
        t3 - t2 > 50_000,
        "resumed process must accrue CPU time (t3-t2={})",
        t3 - t2
    );
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn qos_job_assigns_and_caps() {
    // Spawn dummy; create job; assign; read back the CPU rate control.
    let mut child = spawn_dummy();
    let pid = child.id();
    let backend = OsBackend::new();
    backend
        .apply(pid, &TargetAction::QosCpuQuota { percent: 50 })
        .expect("assign qos");
    // Read back: rate control should be enabled with hard cap 5000 (0.01% units).
    let jobs = backend.jobs.lock().unwrap();
    let entry = jobs.get(&pid).expect("job entry exists");
    let mut info = JOBOBJECT_CPU_RATE_CONTROL_INFORMATION::default();
    unsafe {
        QueryInformationJobObject(
            Some(entry.job),
            JobObjectCpuRateControlInformation,
            (&mut info as *mut JOBOBJECT_CPU_RATE_CONTROL_INFORMATION).cast(),
            std::mem::size_of::<JOBOBJECT_CPU_RATE_CONTROL_INFORMATION>() as u32,
            None,
        )
    }
    .expect("query job");
    assert!(
        info.ControlFlags.0 & JOB_OBJECT_CPU_RATE_CONTROL_ENABLE.0 != 0,
        "rate control must be enabled"
    );
    let cpu_rate = unsafe { info.Anonymous.CpuRate };
    assert_eq!(cpu_rate, 5000, "hard cap must be 50% in 0.01% units");
    drop(jobs);
    backend
        .apply(pid, &TargetAction::QosCpuQuota { percent: 0 })
        .expect("clear qos");
    backend.on_process_exit(pid);
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn qos_clear_then_drop_does_not_kill() {
    let mut child = spawn_dummy();
    let pid = child.id();
    let backend = OsBackend::new();
    backend
        .apply(pid, &TargetAction::QosCpuQuota { percent: 30 })
        .expect("assign");
    backend
        .apply(pid, &TargetAction::QosCpuQuota { percent: 0 })
        .expect("clear");
    drop(backend); // Drop must not terminate the still-running dummy
    let alive = unsafe { OpenProcess(PROCESS_QUERY, false, pid) }.is_ok();
    assert!(alive, "closing the backend must not kill the capped process");
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn qos_reams_after_missed_stop_pid_reuse() {
    // Reproduce the ETW-drop bug: a capped process dies and its Stop event is
    // missed, leaving `jobs[pid] = { job, assigned: true }` behind. When a new
    // process reuses that pid, `apply_qos` previously saw `assigned == true`
    // and skipped assignment — the cap silently did nothing. Fix: probe the
    // job's active-process count; an empty job (0) means the prior process is
    // gone, so re-arm the entry and re-assign the new process.
    let mut child = spawn_dummy();
    let pid = child.id();
    let backend = OsBackend::new();

    backend
        .apply(pid, &TargetAction::QosCpuQuota { percent: 50 })
        .expect("assign qos");
    assert!(
        backend.jobs.lock().unwrap().get(&pid).unwrap().assigned,
        "first assignment must arm the job"
    );

    // Kill WITHOUT on_process_exit (missed Stop): the entry survives armed but
    // the job is now empty.
    child.kill().expect("kill");
    child.wait().expect("wait");
    // wait() guarantees termination, but the job's active count can lag a
    // scheduler tick; poll briefly for the empty-job signal.
    let deadline = std::time::Instant::now() + Duration::from_millis(2000);
    while backend.job_active_processes(pid) != Some(0) {
        assert!(
            std::time::Instant::now() < deadline,
            "job never reported empty after the process died"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(
        backend.jobs.lock().unwrap().get(&pid).unwrap().assigned,
        "missed Stop must leave assigned == true (bug precondition)"
    );

    // Simulate pid reuse: a brand-new process takes over the stale empty job.
    // Windows pid reuse cannot be forced deterministically, so seed the
    // surviving entry under the new process's pid — the same occupied +
    // assigned + empty state the real bug leaves behind.
    let mut new_child = spawn_dummy();
    let new_pid = new_child.id();
    {
        let mut jobs = backend.jobs.lock().unwrap();
        let entry = jobs.remove(&pid).expect("stale entry present");
        assert!(entry.assigned, "stale entry must be armed");
        jobs.insert(new_pid, entry);
    }

    // Re-apply the cap: the fix must re-arm the occupied entry and RE-ASSIGN
    // the new process, so the cap actually applies.
    backend
        .apply(new_pid, &TargetAction::QosCpuQuota { percent: 50 })
        .expect("re-apply qos after pid reuse");

    // THE regression guard: with the fix the job is no longer empty — the new
    // process was genuinely assigned into it. Without the fix the re-apply only
    // re-configures the job's rate control (via SetInformationJobObject) and
    // never assigns the process, so the active-process count stays 0 and this
    // assertion fails. `entry.assigned` and the rate-control read-back below are
    // both true either way — they prove the JOB is armed/configured, not that
    // the new process is in it.
    assert_eq!(
        backend.job_active_processes(new_pid),
        Some(1),
        "new process must be re-assigned to the job after a missed-Stop PID reuse"
    );

    let jobs = backend.jobs.lock().unwrap();
    let entry = jobs.get(&new_pid).expect("entry present after re-apply");
    assert!(entry.assigned, "re-arm must re-assign the new process");

    // Read back the cap to prove the new process is genuinely capped.
    let mut info = JOBOBJECT_CPU_RATE_CONTROL_INFORMATION::default();
    unsafe {
        QueryInformationJobObject(
            Some(entry.job),
            JobObjectCpuRateControlInformation,
            (&mut info as *mut JOBOBJECT_CPU_RATE_CONTROL_INFORMATION).cast(),
            std::mem::size_of::<JOBOBJECT_CPU_RATE_CONTROL_INFORMATION>() as u32,
            None,
        )
    }
    .expect("query job");
    assert!(
        info.ControlFlags.0 & JOB_OBJECT_CPU_RATE_CONTROL_ENABLE.0 != 0,
        "new process must be under the CPU cap"
    );
    assert_eq!(unsafe { info.Anonymous.CpuRate }, 5000, "cap is 50%");

    // Cleanup. Kill + wait the child FIRST, then on_process_exit — closing the
    // job handle is only safe after the process has exited (the documented
    // contract); there is no KILL_ON_JOB_CLOSE, so order matters only for the
    // handle-lifecycle contract, never for process termination.
    drop(jobs);
    backend
        .apply(new_pid, &TargetAction::QosCpuQuota { percent: 0 })
        .expect("clear qos");
    let _ = new_child.kill();
    let _ = new_child.wait();
    backend.on_process_exit(new_pid);
}
