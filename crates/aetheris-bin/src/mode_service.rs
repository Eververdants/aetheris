//! aetheris `service` subcommand: the headless engine.
//!
//! Moved verbatim from the old `aetheris-service` binary. Parses its own args
//! from the passed slice (the `service` subcommand word is already consumed by
//! the dispatcher), keeps stderr, and returns the process exit code.

use std::path::PathBuf;
use std::sync::mpsc::Sender;

use aetheris_core::config::{default_config_path, default_config_str, Config};
use aetheris_core::log;
use aetheris_core::service::{Service, ServiceMsg};

pub fn main(args: Vec<String>) -> i32 {
    // No `--config` → the machine-wide default (%PROGRAMDATA%\aetheris). The
    // service is elevated, so it can create the admin-owned config where the
    // non-elevated UI process cannot.
    let mut cfg_arg: Option<PathBuf> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--config" => {
                i += 1;
                if i < args.len() {
                    cfg_arg = Some(PathBuf::from(&args[i]));
                }
            }
            _ => {}
        }
        i += 1;
    }
    let explicit = cfg_arg.is_some();
    let cfg_path = cfg_arg.unwrap_or_else(default_config_path);

    log::init(1024);

    // First run (only when no explicit --config was given): write the
    // commented-example template before Config::load so the elevated service
    // auto-generates the machine-wide config. An explicit --config path is left
    // alone — a missing file there is reported by Config::load. A write failure
    // is logged, not fatal: Config::load reports the missing/unreadable file
    // definitively (and a genuinely unelevated service will fail both).
    if !explicit && !cfg_path.exists() {
        if let Some(parent) = cfg_path.parent() {
            if let Err(e) = std::fs::create_dir_all(parent) {
                eprintln!(
                    "warning: could not create config dir {}: {e}",
                    parent.display()
                );
            }
        }
        match std::fs::write(&cfg_path, default_config_str()) {
            Ok(()) => println!("created default config: {}", cfg_path.display()),
            Err(e) => eprintln!(
                "warning: could not write default config {}: {e}",
                cfg_path.display()
            ),
        }
    }

    let cfg = match Config::load(&cfg_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("config error: {e}");
            return 1;
        }
    };

    let (service, _state) = Service::new(&cfg_path, cfg);
    let stop_tx: Sender<ServiceMsg> = service.stop_sender();

    if let Err(e) = ctrlc::set_handler(move || {
        let _ = stop_tx.send(ServiceMsg::Stop);
    }) {
        eprintln!("ctrlc handler error: {e}");
    }

    println!("aetheris-service running (config: {})", cfg_path.display());
    if let Err(e) = service.run() {
        eprintln!("service error: {e}");
        return 1;
    }
    0
}
