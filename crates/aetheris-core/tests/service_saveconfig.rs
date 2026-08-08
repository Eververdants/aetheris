//! SaveConfig end-to-end through the real service message path.
//!
//! Deviation from the brief: `Config::from_str` runs `validate()` (a qos quota
//! of 150 is rejected there too), so an invalid config cannot be produced by
//! parsing. The invalid config is built by struct literal instead, which
//! bypasses parse-time validation and exercises the `persist_config` guard that
//! the SaveConfig path exists for.

use std::path::PathBuf;
use std::sync::mpsc::channel;

use aetheris_core::config::{BackgroundRule, Config, GameConfig};
use aetheris_core::service::{Service, ServiceMsg};

const CFG_INITIAL: &str = "[game]\nboost_on_start=true\nprocesses=[\"game.exe\"]\n";

fn tmp_cfg_path() -> PathBuf {
    std::env::temp_dir().join(format!("aetheris_savecfg_{}.toml", std::process::id()))
}

#[test]
fn save_config_validates_then_persists() {
    let path = tmp_cfg_path();
    std::fs::write(&path, CFG_INITIAL).unwrap();

    let (mut svc, _state) = Service::new(&path, Config::load(&path).unwrap());

    // Valid new config: persists + reloads.
    let good = Config::from_str(
        "[game]\nboost_on_start=true\nprocesses=[\"g.exe\"]\n\
         [[background]]\nname=\"b.exe\"\nsuspend=true\n",
    )
    .unwrap();
    let (tx, rx) = channel();
    svc.handle_message(&ServiceMsg::SaveConfig {
        cfg: good.clone(),
        reply: tx,
    })
    .unwrap();
    let saved = rx.recv().expect("persist reply");
    assert_eq!(saved, Ok("saved".to_string()), "valid config must report saved");
    assert_eq!(
        Config::load(&path).unwrap().game.processes,
        vec!["g.exe".to_string()],
        "valid config must be persisted to disk"
    );
    assert_eq!(svc.current_state().mode, "Normal");

    // Invalid config (qos 150): rejected, file unchanged.
    let bad = Config {
        game: GameConfig {
            processes: vec![],
            ..Default::default()
        },
        background: vec![BackgroundRule {
            name: "x.exe".to_string(),
            qos_cpu_quota: Some(150),
            ..Default::default()
        }],
        ..Default::default()
    };
    let (tx, rx) = channel();
    svc.handle_message(&ServiceMsg::SaveConfig {
        cfg: bad,
        reply: tx,
    })
    .unwrap();
    let res = rx.recv().expect("reject reply");
    assert!(res.is_err(), "invalid config must be rejected");
    assert_eq!(
        Config::load(&path).unwrap().game.processes,
        vec!["g.exe".to_string()],
        "rejected config must not touch the file"
    );

    let _ = std::fs::remove_file(&path);
}
