//! aetheris `cli` subcommand: one-shot pipe commands.
//!
//! Moved verbatim from the old `aetheris-cli` binary. Parses its own args from
//! the passed slice (the `cli` subcommand word is already consumed by the
//! dispatcher), keeps stderr, and returns the process exit code (0/1/2).

use aetheris_core::ipc::{Response, Request, client_call, DEFAULT_PIPE};

pub fn main(args: Vec<String>) -> i32 {
    let mut pipe = DEFAULT_PIPE.to_string();
    let mut cmd: Option<String> = None;
    let mut arg: Option<String> = None;
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--pipe" => {
                i += 1;
                if i < args.len() {
                    pipe = args[i].clone();
                }
            }
            "get-state" | "reload" | "query" => {
                cmd = Some(args[i].clone());
                if args[i] == "query" && i + 1 < args.len() {
                    arg = Some(args[i + 1].clone());
                }
            }
            _ => {}
        }
        i += 1;
    }

    let req = match cmd.as_deref() {
        Some("get-state") => Request::GetState,
        Some("reload") => Request::ReloadConfig,
        Some("query") => Request::QueryProcess(arg.unwrap_or_default()),
        _ => {
            eprintln!("usage: aetheris cli [--pipe NAME] get-state|reload|query <name>");
            return 2;
        }
    };

    match client_call(&pipe, &req) {
        Ok(Response::State(s)) => {
            println!("mode: {}", s.mode);
            println!("boosted:");
            for p in &s.boosted {
                println!("  {} (pid {})", p.name, p.pid);
            }
            if s.boosted.is_empty() {
                println!("  (none)");
            }
        }
        Ok(Response::Reload(m)) => println!("reload: {m}"),
        Ok(Response::Config(c)) => {
            println!(
                "processes: {}; background rules: {}; always rules: {}",
                c.game.processes.len(),
                c.background.len(),
                c.rule.len()
            );
        }
        Ok(Response::SaveConfig(Ok(m))) => println!("saved: {m}"),
        Ok(Response::SaveConfig(Err(e))) => println!("save failed: {e}"),
        Ok(Response::Process(Some(p))) => {
            println!("{} (pid {}, game={})", p.name, p.pid, p.is_game)
        }
        Ok(Response::Process(None)) => println!("not found"),
        Err(e) => {
            eprintln!("error: {e}");
            return 1;
        }
    }
    0
}
