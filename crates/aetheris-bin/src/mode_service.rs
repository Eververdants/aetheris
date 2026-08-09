//! aetheris `service` subcommand: the headless engine.
//!
//! Moved verbatim from the old `aetheris-service` binary. Parses its own args
//! from the passed slice (the `service` subcommand word is already consumed by
//! the dispatcher), keeps stderr, and returns the process exit code.

use std::path::PathBuf;
use std::sync::mpsc::Sender;

use aetheris_core::config::Config;
use aetheris_core::log;
use aetheris_core::service::{Service, ServiceMsg};

pub fn main(args: Vec<String>) -> i32 {
    let mut cfg_path = PathBuf::from("aetheris.toml");
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--config" => {
                i += 1;
                if i < args.len() {
                    cfg_path = PathBuf::from(&args[i]);
                }
            }
            _ => {}
        }
        i += 1;
    }

    log::init(1024);

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
