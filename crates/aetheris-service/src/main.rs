use std::path::PathBuf;
use std::sync::mpsc::Sender;

use aetheris_core::config::Config;
use aetheris_core::log;
use aetheris_core::service::{Service, ServiceMsg};

fn main() {
    let mut cfg_path = PathBuf::from("aetheris.toml");
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
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
            std::process::exit(1);
        }
    };

    let service = Service::new(&cfg_path, cfg);
    let stop_tx: Sender<ServiceMsg> = service.stop_sender();

    if let Err(e) = ctrlc::set_handler(move || {
        let _ = stop_tx.send(ServiceMsg::Stop);
    }) {
        eprintln!("ctrlc handler error: {e}");
    }

    println!("aetheris-service running (config: {})", cfg_path.display());
    if let Err(e) = service.run() {
        eprintln!("service error: {e}");
        std::process::exit(1);
    }
}
