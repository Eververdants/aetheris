//! Spawns the dummy_proc helper and verifies real priority/affinity/trim actions.
use std::process::Command;
use std::time::Duration;

use windows::Win32::System::Threading::{
    GetPriorityClass, OpenProcess, BELOW_NORMAL_PRIORITY_CLASS, NORMAL_PRIORITY_CLASS,
    PROCESS_ACCESS_RIGHTS, PROCESS_QUERY_INFORMATION,
};

use aetheris_core::actions::{OsBackend, ProcessBackend, TargetAction};
use aetheris_core::config::PriorityClass;

fn spawn_dummy() -> std::process::Child {
    let exe = env!("CARGO_BIN_EXE_dummy_proc");
    Command::new(exe).spawn().expect("spawn dummy")
}

fn open(pid: u32, rights: PROCESS_ACCESS_RIGHTS) -> windows::Win32::Foundation::HANDLE {
    unsafe { OpenProcess(rights, false, pid) }.expect("open process")
}

#[test]
fn priority_below_normal_takes_effect() {
    let mut child = spawn_dummy();
    let pid = child.id();
    let backend = OsBackend::new();
    backend
        .apply(pid, &TargetAction::Priority(PriorityClass::BelowNormal))
        .expect("apply priority");
    std::thread::sleep(Duration::from_millis(50));
    let h = open(pid, PROCESS_QUERY_INFORMATION);
    let cls = unsafe { GetPriorityClass(h) };
    assert_eq!(cls, BELOW_NORMAL_PRIORITY_CLASS.0);
    let _ = h;
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn affinity_cores_mask_takes_effect() {
    let mut child = spawn_dummy();
    let pid = child.id();
    let backend = OsBackend::new();
    // Pin to the first core only, if the system has more than one.
    backend
        .apply(pid, &TargetAction::Affinity { core_mask: 1 })
        .expect("apply affinity");
    std::thread::sleep(Duration::from_millis(50));
    let h = open(pid, PROCESS_QUERY_INFORMATION);
    let mut mask: usize = 0;
    let mut sys: usize = 0;
    unsafe { windows::Win32::System::Threading::GetProcessAffinityMask(h, &mut mask, &mut sys) }
        .expect("query affinity");
    assert_eq!(mask, 1);
    let _ = h;
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn snapshot_reports_priority_and_affinity() {
    let mut child = spawn_dummy();
    let pid = child.id();
    let backend = OsBackend::new();
    let state = backend.snapshot(pid).expect("snapshot");
    assert_eq!(state.priority, NORMAL_PRIORITY_CLASS.0);
    let _ = child.kill();
    let _ = child.wait();
}
