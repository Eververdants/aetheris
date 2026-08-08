//! Smoke test for the realtime ETW kernel-process consumer.
//!
//! Enabling `Microsoft-Windows-Kernel-Process` requires elevation. When the
//! test process is not elevated the live-session checks are skipped and the
//! fail-safe test asserts that `EtwMonitor::start()` returns `Err` (the service
//! fails closed rather than falling back to polling).
use std::process::Command;
use std::time::{Duration, Instant};

use aetheris_core::etw::EtwMonitor;
use aetheris_core::events::ProcessKind;

fn is_elevated() -> bool {
    // Cheap check: query the process token's elevation level.
    unsafe {
        // HANDLE wraps *mut c_void in windows 0.62 (brief wrote HANDLE(0), which
        // no longer compiles); a null pointer is the same as INVALID_HANDLE_VALUE here.
        let mut token: windows::Win32::Foundation::HANDLE =
            windows::Win32::Foundation::HANDLE(std::ptr::null_mut());
        windows::Win32::System::Threading::OpenProcessToken(
            windows::Win32::System::Threading::GetCurrentProcess(),
            windows::Win32::Security::TOKEN_QUERY,
            &mut token,
        )
        .is_ok()
            && {
                let mut sz = 0u32;
                let mut h: windows::Win32::Security::TOKEN_ELEVATION = Default::default();
                windows::Win32::Security::GetTokenInformation(
                    token,
                    windows::Win32::Security::TokenElevation,
                    Some(&mut h as *mut _ as *mut std::ffi::c_void),
                    std::mem::size_of::<windows::Win32::Security::TOKEN_ELEVATION>() as u32,
                    &mut sz,
                )
                .is_ok()
                    && h.TokenIsElevated != 0
            }
    }
}

#[test]
fn etw_sees_process_start() {
    if !is_elevated() {
        eprintln!("SKIP: not elevated");
        return;
    }
    let mon = EtwMonitor::start().expect("start etw session");
    let exe = env!("CARGO_BIN_EXE_dummy_proc");
    let mut child = Command::new(exe).spawn().expect("spawn dummy");
    let pid = child.id();

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut seen = false;
    while Instant::now() < deadline {
        let remaining = deadline.saturating_duration_since(Instant::now());
        // Deviation from brief: use recv_timeout so an idle system cannot hang
        // the loop past the deadline (the brief's blocking recv would).
        if let Some(ev) = mon.recv_timeout(remaining) {
            if ev.pid == pid && ev.kind == ProcessKind::Start {
                assert_eq!(ev.name.to_ascii_lowercase(), "dummy_proc.exe");
                seen = true;
                break;
            }
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    mon.stop();
    assert!(seen, "no Start event for dummy_proc within 10s");
}

/// Fail-safe: a non-elevated process must get `Err` from `EtwMonitor::start()`
/// (StartTraceW/EnableTraceEx2 fail with access denied). When elevated this is
/// skipped because the real start path is exercised by the main test.
#[test]
fn etw_start_fails_closed_without_elevation() {
    if is_elevated() {
        eprintln!("SKIP: elevated");
        return;
    }
    match EtwMonitor::start() {
        Err(e) => eprintln!("fail-safe confirmed, start() error: {e}"),
        Ok(_mon) => panic!("non-elevated start must fail closed, got Ok instead"),
    }
}
