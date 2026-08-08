//! Server on a test-only pipe; client roundtrips GetState/QueryProcess.
use std::thread;
use std::time::Duration;

use aetheris_core::ipc::{client_call, IpcServer, ProcessInfo, Request, Response, StateSnapshot};

const TEST_PIPE: &str = r"\\.\pipe\aetheris_test";

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
            }
        };
        let _ = server.run(&mut handler);
    });

    // Wait for the server to be listening.
    thread::sleep(Duration::from_millis(300));

    let state = match client_call(TEST_PIPE, &Request::GetState).expect("call") {
        Response::State(s) => s,
        other => panic!("unexpected response: {other:?}"),
    };
    assert_eq!(state.mode, "Normal");

    let proc =
        match client_call(TEST_PIPE, &Request::QueryProcess("browser.exe".into())).expect("call") {
            Response::Process(p) => p,
            other => panic!("unexpected response: {other:?}"),
        };
    assert_eq!(proc.unwrap().pid, 42);

    let reload = client_call(TEST_PIPE, &Request::ReloadConfig).expect("call");
    assert!(matches!(reload, Response::Reload(_)));

    drop(t);
}
