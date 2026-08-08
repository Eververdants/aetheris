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
