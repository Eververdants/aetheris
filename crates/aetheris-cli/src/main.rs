use aetheris_core::ipc::{Response, Request, client_call, DEFAULT_PIPE};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut pipe = DEFAULT_PIPE.to_string();
    let mut cmd: Option<String> = None;
    let mut arg: Option<String> = None;
    let mut i = 1;
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
            eprintln!("usage: aetheris-cli [--pipe NAME] get-state|reload|query <name>");
            std::process::exit(2);
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
        Ok(Response::Process(Some(p))) => {
            println!("{} (pid {}, game={})", p.name, p.pid, p.is_game)
        }
        Ok(Response::Process(None)) => println!("not found"),
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}
