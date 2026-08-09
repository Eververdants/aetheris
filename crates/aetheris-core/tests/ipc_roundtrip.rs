//! Server on a test-only pipe; client roundtrips GetState/QueryProcess.
use std::thread;
use std::time::Duration;

use aetheris_core::config::{Config, GameConfig};
use aetheris_core::ipc::{client_call, IpcServer, ProcessInfo, Request, Response, StateSnapshot};
use windows::Win32::Foundation::HANDLE;

const TEST_PIPE: &str = r"\\.\pipe\aetheris_test";
const TEST_PIPE_DACL: &str = r"\\.\pipe\aetheris_test_dacl";
const TEST_PIPE_STOP: &str = r"\\.\pipe\aetheris_test_stop";
const TEST_PIPE_TOGGLE: &str = r"\\.\pipe\aetheris_test_toggle";

/// `call_with_retry` deadline: how long to keep polling the pipe for a
/// transient one-shot-server race before giving up.
const RETRY_DEADLINE: Duration = Duration::from_secs(5);
/// `call_with_retry` pause between polling attempts.
const RETRY_INTERVAL: Duration = Duration::from_millis(50);

/// Unit: the DACL grants IU read+write but not WRITE_DAC/WRITE_OWNER.
#[test]
fn dacl_least_privilege_for_interactive_users() {
    let d = aetheris_core::ipc::DEFAULT_PIPE_DACL;
    assert!(d.contains("(A;;GA;;;SY)"), "SYSTEM must retain full access");
    assert!(d.contains("(A;;GR;;;IU)"), "IU must get read");
    assert!(d.contains("(A;;GW;;;IU)"), "IU must get write");
    assert!(!d.contains("(A;;GA;;;IU)"), "IU must NOT get generic-all (WRITE_DAC/WRITE_OWNER)");
}

#[test]
fn is_client_elevated_fails_closed_on_invalid_handle() {
    // A non-pipe handle (or null) must return Err — the fail-closed contract
    // that denies SaveConfig when elevation can't be established.
    let r =
        aetheris_core::ipc::is_client_elevated(windows::Win32::Foundation::HANDLE(std::ptr::null_mut()));
    assert!(r.is_err(), "invalid handle must fail closed, got {:?}", r);
}

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
    let deadline = std::time::Instant::now() + RETRY_DEADLINE;
    loop {
        match client_call(pipe, req) {
            Ok(r) => return Ok(r),
            Err(e) => {
                if std::time::Instant::now() >= deadline {
                    return Err(e);
                }
                thread::sleep(RETRY_INTERVAL);
            }
        }
    }
}

#[test]
fn roundtrip_get_state_and_query() {
    let server = IpcServer::new(TEST_PIPE);
    let t = thread::spawn(move || {
        let mut handler = |_pipe: HANDLE, req: &Request| -> Response {
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
                // Mirror the real service's IPC handler: the two control
                // requests answer `Reload` with the relay message name.
                Request::StopService => Response::Reload("stopping".into()),
                Request::ToggleOverlay => Response::Reload("toggled".into()),
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

/// v2.2: `Request::StopService` roundtrips to the `Reload("stopping")` reply the
/// real service returns after relaying `ServiceMsg::Stop` to its main loop.
#[test]
fn roundtrip_stop_service() {
    let server = IpcServer::new(TEST_PIPE_STOP);
    let t = thread::spawn(move || {
        let mut handler = |_pipe: HANDLE, req: &Request| -> Response {
            match req {
                // Mirror the real service's IPC thread arm.
                Request::StopService => Response::Reload("stopping".into()),
                // The other requests are not exercised on this pipe; answer
                // safely so the match stays exhaustive.
                _ => Response::Reload("unhandled".into()),
            }
        };
        let _ = server.run(&mut handler);
    });

    let stop = call_with_retry(TEST_PIPE_STOP, &Request::StopService).expect("call");
    match stop {
        Response::Reload(m) => assert_eq!(m, "stopping"),
        other => panic!("StopService roundtrip: got {other:?}"),
    }

    drop(t);
}

/// v2.2: `Request::ToggleOverlay` roundtrips to `Reload("toggled")`, the reply
/// the real service returns after relaying `ServiceMsg::ToggleOverlay`.
#[test]
fn roundtrip_toggle_overlay() {
    let server = IpcServer::new(TEST_PIPE_TOGGLE);
    let t = thread::spawn(move || {
        let mut handler = |_pipe: HANDLE, req: &Request| -> Response {
            match req {
                Request::ToggleOverlay => Response::Reload("toggled".into()),
                _ => Response::Reload("unhandled".into()),
            }
        };
        let _ = server.run(&mut handler);
    });

    let toggle = call_with_retry(TEST_PIPE_TOGGLE, &Request::ToggleOverlay).expect("call");
    match toggle {
        Response::Reload(m) => assert_eq!(m, "toggled"),
        other => panic!("ToggleOverlay roundtrip: got {other:?}"),
    }

    drop(t);
}

#[test]
fn pipe_with_interactive_dacl_connectable_from_same_token() {
    let server = IpcServer::new_with_dacl(TEST_PIPE_DACL, aetheris_core::ipc::DEFAULT_PIPE_DACL);
    let t = thread::spawn(move || {
        let mut h = |_pipe: HANDLE, _req: &Request| Response::State(StateSnapshot::default());
        let _ = server.run(&mut h);
    });

    // Connect as the current (likely non-elevated) token; must succeed with the
    // IU DACL.
    let resp = call_with_retry(TEST_PIPE_DACL, &Request::GetState)
        .expect("connect with interactive DACL");
    assert!(matches!(resp, Response::State(_)));

    drop(t);
}
