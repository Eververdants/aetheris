//! Server on a test-only pipe; client roundtrips GetState/QueryProcess.
use std::thread;
use std::time::Duration;

use aetheris_core::config::{Config, GameConfig};
use aetheris_core::ipc::{client_call, IpcServer, ProcessInfo, Request, Response, StateSnapshot};

const TEST_PIPE: &str = r"\\.\pipe\aetheris_test";
const TEST_PIPE_DACL: &str = r"\\.\pipe\aetheris_test_dacl";

/// The one-shot server tears each connection down before recreating the next
/// pipe instance, so a client call can fail transiently even though a previous
/// call succeeded: a write landing inside the close-then-recreate window gets
/// ERROR_PIPE_NOT_CONNECTED, and a call racing the very first `CreateNamedPipeW`
/// finds no pipe instance at all ("pipe unavailable after retries"). Both are
/// scheduling races, not protocol failures. Poll until the call succeeds or the
/// deadline passes, retrying every error; a genuinely broken server still fails
/// (slowly) after the deadline. Protocol mismatches surface as panics at the
/// call sites' `match`, which this helper does not retry.
fn call_with_retry(pipe: &str, req: &Request) -> Result<Response, String> {
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        match client_call(pipe, req) {
            Ok(r) => return Ok(r),
            Err(e) => {
                if std::time::Instant::now() >= deadline {
                    return Err(e);
                }
                thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

#[test]
fn roundtrip_get_state_and_query() {
    let server = IpcServer::new(TEST_PIPE);
    let t = thread::spawn(move || {
        let mut handler = |req: &Request| -> Response {
            match req {
                Request::GetState => Response::State(StateSnapshot {
                    mode: "Normal".into(),
                    boosted: vec![],
                    ..StateSnapshot::default()
                }),
                Request::QueryProcess(name) => Response::Process(Some(ProcessInfo {
                    pid: 42,
                    name: name.clone(),
                    is_game: false,
                })),
                Request::ReloadConfig => Response::Reload("ok".into()),
                Request::GetConfig => Response::Config(Config {
                    game: GameConfig {
                        processes: vec!["game.exe".into()],
                        ..Default::default()
                    },
                    ..Default::default()
                }),
                Request::SaveConfig(_cfg) => Response::SaveConfig(Ok("saved".into())),
            }
        };
        let _ = server.run(&mut handler);
    });

    let state = match call_with_retry(TEST_PIPE, &Request::GetState).expect("call") {
        Response::State(s) => s,
        other => panic!("unexpected response: {other:?}"),
    };
    assert_eq!(state.mode, "Normal");

    let proc = match call_with_retry(TEST_PIPE, &Request::QueryProcess("browser.exe".into()))
        .expect("call")
    {
        Response::Process(p) => p,
        other => panic!("unexpected response: {other:?}"),
    };
    assert_eq!(proc.unwrap().pid, 42);

    let reload = call_with_retry(TEST_PIPE, &Request::ReloadConfig).expect("call");
    assert!(matches!(reload, Response::Reload(_)));

    let cfg = match call_with_retry(TEST_PIPE, &Request::GetConfig).expect("call") {
        Response::Config(c) => c,
        other => panic!("unexpected response: {other:?}"),
    };
    assert_eq!(cfg.game.processes, vec!["game.exe".to_string()]);

    let save = call_with_retry(TEST_PIPE, &Request::SaveConfig(cfg)).expect("call");
    assert!(matches!(save, Response::SaveConfig(Ok(_))));

    drop(t);
}

#[test]
fn pipe_with_interactive_dacl_connectable_from_same_token() {
    let server = IpcServer::new_with_dacl(TEST_PIPE_DACL, aetheris_core::ipc::DEFAULT_PIPE_DACL);
    let t = thread::spawn(move || {
        let mut h = |_req: &Request| Response::State(StateSnapshot::default());
        let _ = server.run(&mut h);
    });

    // Connect as the current (likely non-elevated) token; must succeed with the
    // IU DACL.
    let resp = call_with_retry(TEST_PIPE_DACL, &Request::GetState)
        .expect("connect with interactive DACL");
    assert!(matches!(resp, Response::State(_)));

    drop(t);
}
