//! Named-pipe IPC for the aetheris control surface.
//!
//! A small synchronous (blocking) request/response protocol on Windows named
//! pipes. One pipe instance is created per connection, so the server is driven
//! one-shot at a time: accept a client, read a length-prefixed bincode
//! [`Request`], run the handler, write back a length-prefixed bincode
//! [`Response`], then disconnect and loop for the next client.
//!
//! Frame format: 4-byte little-endian length prefix followed by the bincode
//! payload. Messages are capped at 1 MiB on both sides.

use std::io::{Read, Write};
use std::os::windows::io::FromRawHandle;

use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::{CloseHandle, GetLastError, GENERIC_READ, GENERIC_WRITE, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FlushFileBuffers, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, WaitNamedPipeW, PIPE_READMODE_BYTE,
    PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};

/// Default pipe used by the service.
pub const DEFAULT_PIPE: &str = r"\\.\pipe\aetheris";

/// Largest message accepted in either direction.
const MAX_MSG: usize = 1 << 20;

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum Request {
    GetState,
    ReloadConfig,
    QueryProcess(String),
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum Response {
    State(StateSnapshot),
    Reload(String),
    Process(Option<ProcessInfo>),
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct StateSnapshot {
    pub mode: String,
    pub boosted: Vec<ProcessInfo>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ProcessInfo {
    pub pid: u32,
    pub name: String,
    pub is_game: bool,
}

pub struct IpcServer {
    pipe_name: String,
}

impl IpcServer {
    pub fn new(pipe_name: &str) -> Self {
        Self {
            pipe_name: pipe_name.to_string(),
        }
    }

    /// Blocking accept/serve loop. Runs forever; returns `Err` on a hard pipe
    /// error and `Ok(())` on graceful shutdown (v1 never exits except on error).
    pub fn run<F: FnMut(&Request) -> Response>(&self, handler: &mut F) -> Result<(), String> {
        let name: Vec<u16> = self
            .pipe_name
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();

        loop {
            // CreateNamedPipeW returns a raw HANDLE (not a Result) in the
            // windows crate; a null/INVALID_HANDLE_VALUE means failure.
            let pipe = unsafe {
                CreateNamedPipeW(
                    windows::core::PCWSTR(name.as_ptr()),
                    PIPE_ACCESS_DUPLEX,
                    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                    PIPE_UNLIMITED_INSTANCES,
                    4096,
                    4096,
                    0,
                    None,
                )
            };
            if pipe.is_invalid() {
                return Err(format!("CreateNamedPipeW failed (last error {})", unsafe {
                    GetLastError().0
                }));
            }

            // Block until a client opens the instance. The wrapper maps
            // ERROR_PIPE_CONNECTED to Ok, so a client that won the race between
            // CreateNamedPipeW and ConnectNamedPipe is still accepted.
            if unsafe { ConnectNamedPipe(pipe, None) }.is_err() {
                cleanup(pipe);
                continue;
            }

            // Read the length-prefixed request.
            let mut len_buf = [0u8; 4];
            if read_exact_handle(pipe, &mut len_buf).is_err() {
                cleanup(pipe);
                continue;
            }
            let len = u32::from_le_bytes(len_buf) as usize;
            if len == 0 || len > MAX_MSG {
                cleanup(pipe);
                continue;
            }
            let mut req_buf = vec![0u8; len];
            if read_exact_handle(pipe, &mut req_buf).is_err() {
                cleanup(pipe);
                continue;
            }

            let req: Request = match bincode::deserialize(&req_buf) {
                Ok(r) => r,
                Err(_) => {
                    cleanup(pipe);
                    continue;
                }
            };

            let resp = handler(&req);
            let resp_buf = match bincode::serialize(&resp) {
                Ok(b) => b,
                Err(e) => {
                    // Cannot build a response at all; tear down this connection.
                    cleanup(pipe);
                    return Err(format!("serialize: {e}"));
                }
            };
            let _ = write_all_handle(pipe, &(resp_buf.len() as u32).to_le_bytes());
            let _ = write_all_handle(pipe, &resp_buf);

            // Ensure every response byte reaches the client before
            // DisconnectNamedPipe, which would otherwise discard server-side
            // buffered data the client has not read yet.
            let _ = unsafe { FlushFileBuffers(pipe) };
            cleanup(pipe);
        }
    }
}

fn cleanup(h: HANDLE) {
    unsafe {
        let _ = DisconnectNamedPipe(h);
        let _ = CloseHandle(h);
    }
}

/// `Read`/`Write` adapter over a raw pipe `HANDLE`. The wrapper *owns* the
/// handle (via `from_raw_handle`), so callers must `mem::forget` the value and
/// close the handle themselves to avoid a double close.
struct FileHandle(std::fs::File);

impl FileHandle {
    unsafe fn new(h: HANDLE) -> Self {
        Self(std::fs::File::from_raw_handle(h.0))
    }
}

impl Read for FileHandle {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
}

impl Write for FileHandle {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}

fn read_exact_handle(h: HANDLE, buf: &mut [u8]) -> std::io::Result<()> {
    let mut file = unsafe { FileHandle::new(h) };
    let res = file.read_exact(buf);
    std::mem::forget(file); // we do not own the handle
    res
}

fn write_all_handle(h: HANDLE, buf: &[u8]) -> std::io::Result<()> {
    let mut file = unsafe { FileHandle::new(h) };
    let res = file.write_all(buf);
    std::mem::forget(file);
    res
}

/// Connect to a named pipe and perform one request/response cycle.
///
/// Opens the pipe in blocking (non-overlapped) mode, so plain synchronous
/// `Read`/`Write` calls are used for the frame.
pub fn client_call(pipe_name: &str, req: &Request) -> Result<Response, String> {
    let name: Vec<u16> = pipe_name.encode_utf16().chain(std::iter::once(0)).collect();

    // Wait for an available pipe instance before opening, retrying with a
    // bounded timeout so a missing server fails fast instead of hanging.
    let mut waited = 0;
    loop {
        let ok = unsafe { WaitNamedPipeW(windows::core::PCWSTR(name.as_ptr()), 2000) };
        if ok.as_bool() {
            break;
        }
        waited += 1;
        if waited > 5 {
            return Err(format!("WaitNamedPipeW timeout for {pipe_name}"));
        }
    }

    // Blocking client pipe: no FILE_FLAG_OVERLAPPED.
    let h = unsafe {
        CreateFileW(
            windows::core::PCWSTR(name.as_ptr()),
            (GENERIC_READ | GENERIC_WRITE).0,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAGS_AND_ATTRIBUTES::default(),
            None,
        )
    }
    .map_err(|e| format!("CreateFileW: {e}"))?;

    let req_buf = bincode::serialize(req).map_err(|e| format!("serialize: {e}"))?;
    let mut file = unsafe { FileHandle::new(h) };
    let res = (|| -> std::io::Result<Response> {
        file.write_all(&(req_buf.len() as u32).to_le_bytes())?;
        file.write_all(&req_buf)?;

        let mut len_buf = [0u8; 4];
        file.read_exact(&mut len_buf)?;
        let len = u32::from_le_bytes(len_buf) as usize;
        if len == 0 || len > MAX_MSG {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("response length {len} out of range"),
            ));
        }
        let mut buf = vec![0u8; len];
        file.read_exact(&mut buf)?;
        bincode::deserialize(&buf)
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    })();
    std::mem::forget(file); // we do not own the handle
    let _ = unsafe { CloseHandle(h) };
    res.map_err(|e| format!("io: {e}"))
}
