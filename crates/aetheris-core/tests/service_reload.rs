//! Service message handling end-to-end using synthetic process events and a
//! real snapshotable helper process (no ETW needed).
//!
//! Deviation from the brief (documented in task-12-report.md): the brief's test
//! fed fake pids (100/200) to the real [`OsBackend`], whose `snapshot()` /
//! `apply()` cannot target nonexistent processes (`OpenProcess` fails), so the
//! "boosted is non-empty" assertion could never hold with the real backend.
//! We spawn the `dummy_proc` helper (the repo's established integration-test
//! pattern, cf. `actions_priority.rs`) and target it with the background rule so
//! the whole boost -> restore cycle runs against a real process.

use std::path::PathBuf;
use std::process::Command;

use aetheris_core::config::Config;
use aetheris_core::events::{ProcessEvent, ProcessKind};
use aetheris_core::service::{Service, ServiceMsg};

const CFG_GAME: &str = r#"
[game]
boost_on_start = true
processes = ["game.exe"]

[[background]]
name = "dummy_proc.exe"
priority = "below_normal"
"#;

const CFG_AFTER_RELOAD: &str = r#"
[game]
boost_on_start = true
processes = ["other.exe"]
"#;

/// Per-process temp config path so parallel test binaries never collide.
fn tmp_cfg_path() -> PathBuf {
    std::env::temp_dir().join(format!("aetheris_service_reload_{}.toml", std::process::id()))
}

fn spawn_dummy() -> std::process::Child {
    let exe = env!("CARGO_BIN_EXE_dummy_proc");
    Command::new(exe).spawn().expect("spawn dummy")
}

#[test]
fn service_processes_messages_and_reload() {
    let tmp = tmp_cfg_path();
    std::fs::write(&tmp, CFG_GAME).unwrap();

    let mut child = spawn_dummy();
    let bg_pid = child.id();

    let mut svc = Service::new(&tmp, Config::load(&tmp).expect("cfg"));

    // Background helper starts first.
    svc.handle_message(&ServiceMsg::Proc(ProcessEvent {
        pid: bg_pid,
        name: "dummy_proc.exe".into(),
        parent_pid: 0,
        kind: ProcessKind::Start,
    }))
    .unwrap();

    // Game process starts (synthetic; the engine needs no real handle for it).
    svc.handle_message(&ServiceMsg::Proc(ProcessEvent {
        pid: 9000,
        name: "game.exe".into(),
        parent_pid: 0,
        kind: ProcessKind::Start,
    }))
    .unwrap();

    let state = svc.current_state();
    assert_eq!(state.mode, "GameBoost");
    assert!(!state.boosted.is_empty(), "background helper must be boosted");
    assert!(state.boosted.iter().any(|p| p.pid == bg_pid));

    // Reload re-reads the (rewritten) config file and exits GameBoost cleanly.
    std::fs::write(&tmp, CFG_AFTER_RELOAD).unwrap();
    svc.handle_message(&ServiceMsg::Reload).unwrap();
    let state = svc.current_state();
    assert_eq!(state.mode, "Normal");
    assert!(state.boosted.is_empty());

    // The reloaded config is live: "game.exe" is no longer a game.
    svc.handle_message(&ServiceMsg::Proc(ProcessEvent {
        pid: 9001,
        name: "game.exe".into(),
        parent_pid: 0,
        kind: ProcessKind::Start,
    }))
    .unwrap();
    assert_eq!(
        svc.current_state().mode,
        "Normal",
        "reloaded config must be in effect"
    );

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&tmp);
}
