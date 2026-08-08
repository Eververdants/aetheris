//! Live GetState/QueryProcess over the shared snapshot.
//!
//! Deviation from the brief (documented in task-1-report.md): the brief fed fake
//! pids (9001/9002) to the real [`OsBackend`], whose `snapshot()` cannot target
//! a nonexistent process (`OpenProcess` fails), so its `boosted`-is-non-empty
//! assertion could never hold. We spawn the `dummy_proc` helper (the repo's
//! established integration-test pattern, cf. `service_reload.rs`) and target it
//! with the background rule so the shared snapshot lists it under `boosted` for
//! real.

use std::path::PathBuf;
use std::process::Command;

use aetheris_core::config::Config;
use aetheris_core::events::{ProcessEvent, ProcessKind};
use aetheris_core::service::{Service, ServiceMsg};

const CFG_TOML: &str = r#"
[game]
boost_on_start = true
processes = ["game.exe"]
[[background]]
name = "dummy_proc.exe"
suspend = true
"#;

fn cfg() -> Config {
    Config::from_str(CFG_TOML).unwrap()
}

fn tmp_cfg_path() -> PathBuf {
    std::env::temp_dir().join(format!("aetheris_state_cfg_{}.toml", std::process::id()))
}

fn spawn_dummy() -> std::process::Child {
    let exe = env!("CARGO_BIN_EXE_dummy_proc");
    Command::new(exe).spawn().expect("spawn dummy")
}

#[test]
fn state_snapshot_is_live_and_queryable() {
    let tmp = tmp_cfg_path();
    std::fs::write(&tmp, CFG_TOML).unwrap();

    let mut child = spawn_dummy();
    let dummy_pid = child.id();

    let (mut svc, state) = Service::new(&tmp, cfg());

    // Game process starts (synthetic; the engine needs no real handle for it).
    svc.handle_message(&ServiceMsg::Proc(ProcessEvent {
        pid: 9001,
        name: "game.exe".into(),
        parent_pid: 0,
        kind: ProcessKind::Start,
    }))
    .unwrap();

    // Real background helper: boosted for real so the snapshot lists it.
    svc.handle_message(&ServiceMsg::Proc(ProcessEvent {
        pid: dummy_pid,
        name: "dummy_proc.exe".into(),
        parent_pid: 0,
        kind: ProcessKind::Start,
    }))
    .unwrap();

    // The shared snapshot reflects the live engine state after each message.
    let snap = state.read().unwrap();
    assert_eq!(snap.mode, "GameBoost");
    assert!(
        snap.boosted.iter().any(|p| p.name == "dummy_proc.exe"),
        "background helper must be listed as boosted"
    );
    assert!(
        snap.processes.iter().any(|p| p.pid == 9001 && p.name == "game.exe"),
        "process table must be reflected in the snapshot"
    );

    let _ = child.kill();
    let _ = child.wait();
    let _ = std::fs::remove_file(&tmp);
}
