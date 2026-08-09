//! aetheris — single binary dispatcher.
//!
//! The four historical binaries (`aetheris-service`, `aetheris-cli`,
//! `aetheris-ui`, `aetheris-overlay`) are now subcommands of one `aetheris.exe`:
//!
//! * no-arg (or `ui`) → the status panel + rule editor
//! * `service`   → the headless engine (elevated, keeps stderr)
//! * `overlay`   → the DirectComposition telemetry panel
//! * `cli`       → one-shot pipe commands (keeps stderr)
//! * `--version` → `aetheris <version>`
//!
//! Subsystem choice: the binary stays **console-subsystem** (no
//! `#![windows_subsystem = "windows"]` at crate level) so that `service` and
//! `cli` can write to stderr. The GUI modes (`ui`, `overlay`) detach from any
//! console on entry by calling `FreeConsole()`, so no console window appears
//! alongside their windows; their errors surface via a message box + log file
//! instead of `eprintln!`.

mod mode_cli;
mod mode_overlay;
mod mode_service;
mod mode_ui;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let sub = args.first().cloned().unwrap_or_else(|| "ui".to_string()); // no-arg = ui
    // The subcommand word is consumed here; each mode parses the remaining args
    // from `rest`. Slicing is done via `into_iter().skip(1)` rather than
    // `args[1..]`, which would panic on the no-arg (`ui`) case where `args` is
    // empty.
    let rest = args.into_iter().skip(1).collect::<Vec<_>>();
    let code = match sub.as_str() {
        "service" => mode_service::main(rest.clone()),
        "ui" => mode_ui::main(rest.clone()),
        "overlay" => mode_overlay::main(rest.clone()),
        "cli" => mode_cli::main(rest.clone()),
        "--version" | "-V" => {
            println!("aetheris {}", env!("CARGO_PKG_VERSION"));
            0
        }
        other => {
            eprintln!("usage: aetheris [service|ui|overlay|cli] ...");
            eprintln!("unknown subcommand: {other}");
            2
        }
    };
    std::process::exit(code);
}
