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
use windows::Win32::Foundation::{
    CloseHandle, GetLastError, GENERIC_READ, GENERIC_WRITE, HANDLE, HLOCAL, LocalFree,
    ERROR_PIPE_BUSY, WIN32_ERROR,
};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{
    GetTokenInformation, PSECURITY_DESCRIPTOR, RevertToSelf, SECURITY_ATTRIBUTES, TOKEN_ELEVATION,
    TOKEN_QUERY, TokenElevation,
};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FlushFileBuffers, FILE_FLAGS_AND_ATTRIBUTES, FILE_SHARE_READ, FILE_SHARE_WRITE,
    OPEN_EXISTING, PIPE_ACCESS_DUPLEX,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, ImpersonateNamedPipeClient,
    WaitNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};
use windows::Win32::System::Threading::{GetCurrentThread, OpenThreadToken};

use crate::config::Config;

/// Default pipe used by the service.
pub const DEFAULT_PIPE: &str = r"\\.\pipe\aetheris";

/// Default DACL for the service pipe: SYSTEM full access plus Interactive Users
/// (`IU`, S-1-5-4) generic read + generic write.
///
/// Least privilege: IU is deliberately *not* granted `GA` (generic all), which
/// would map to WRITE_DAC / WRITE_OWNER and let any interactive client replace
/// the pipe's security descriptor. `GR|GW` is exactly what the non-elevated
/// `aetheris-cli` requests (`GENERIC_READ|GENERIC_WRITE` in [`client_call`]) and
/// enough for the read-only surface (GetState/GetConfig/QueryProcess, harmless
/// ReloadConfig re-read). Write access to the config file via SaveConfig is
/// separately gated on client elevation ([`is_client_elevated`]), so this DACL
/// grants the transport, not the privilege. SYSTEM retains full access, and no
/// other SID is granted anything.
pub const DEFAULT_PIPE_DACL: &str = "D:P(A;;GA;;;SY)(A;;GR;;;IU)(A;;GW;;;IU)";

/// Largest message accepted in either direction.
const MAX_MSG: usize = 1 << 20;

/// Win32 error 123, `ERROR_NO_PROCESS` — the named constant is not generated
/// in the pinned windows crate's `Win32::Foundation` module, so it is spelled
/// out here. It is the documented race when `CreateFileW` lands inside the
/// server's close-then-recreate window.
const ERROR_NO_PROCESS: WIN32_ERROR = WIN32_ERROR(123);

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum Request {
    GetState,
    GetConfig,
    ReloadConfig,
    SaveConfig(Config),
    QueryProcess(String),
    /// Ask the service to stop: the IPC thread relays `ServiceMsg::Stop`, which
    /// exits GameBoost cleanly (restores every boosted process) and breaks the
    /// service main loop so the elevated process exits. Answers
    /// `Response::Reload("stopping")` before the loop breaks; the response is
    /// best-effort (the process may exit before the client reads it).
    StopService,
    /// Ask the service to toggle the overlay (launch, or close a running
    /// overlay window). Relayed as `ServiceMsg::ToggleOverlay`; answers
    /// `Response::Reload("toggled")`.
    ToggleOverlay,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum Response {
    State(StateSnapshot),
    Config(Config),
    Reload(String),
    SaveConfig(Result<String, String>),
    Process(Option<ProcessInfo>),
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct StateSnapshot {
    pub mode: String,
    pub boosted: Vec<ProcessInfo>,
    pub processes: Vec<ProcessInfo>,
    pub last_reload: Option<String>,
    /// Live config, cloned into the snapshot on each refresh so `GetConfig`
    /// answers from shared state without a separate roundtrip to the engine.
    pub config: Config,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ProcessInfo {
    pub pid: u32,
    pub name: String,
    pub is_game: bool,
}

pub struct IpcServer {
    pipe_name: String,
    /// Optional DACL to apply to created pipe instances, as an SDDL string
    /// (e.g. [`DEFAULT_PIPE_DACL`]). `None` keeps the default (service/owner
    /// only) DACL.
    dacl_sddl: Option<String>,
}

impl IpcServer {
    /// Create a server whose pipe instances use the default DACL (access
    /// restricted to the service account / SYSTEM).
    pub fn new(pipe_name: &str) -> Self {
        Self {
            pipe_name: pipe_name.to_string(),
            dacl_sddl: None,
        }
    }

    /// Create a server whose pipe instances carry `sddl` as their security
    /// descriptor (converted via `ConvertStringSecurityDescriptorToSecurityDescriptorW`
    /// in [`IpcServer::run`]). Used to grant Interactive Users access so a
    /// non-elevated `aetheris-cli` can talk to the elevated service.
    pub fn new_with_dacl(pipe_name: &str, sddl: &str) -> Self {
        Self {
            pipe_name: pipe_name.to_string(),
            dacl_sddl: Some(sddl.to_string()),
        }
    }

    /// Blocking accept/serve loop. Runs forever; returns `Err` on a hard pipe
    /// error and `Ok(())` on graceful shutdown (v1 never exits except on error).
    ///
    /// The handler receives the connected pipe `HANDLE` as its first argument
    /// (added in v2.1) so request handlers can act on the client's identity —
    /// e.g. [`is_client_elevated`] to gate privileged requests on the connected
    /// client's token before any impersonated security context is torn down by
    /// `cleanup`.
    pub fn run<F: FnMut(HANDLE, &Request) -> Response>(&self, handler: &mut F) -> Result<(), String> {
        let name: Vec<u16> = self
            .pipe_name
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();

        // Build the SECURITY_ATTRIBUTES (if a DACL was requested) once and reuse
        // it for every pipe instance this loop creates. The kernel copies the
        // security descriptor into the pipe object at CreateNamedPipeW time, so a
        // single descriptor may be shared by all instances. `_sd_guard` frees it
        // via LocalFree when `run` returns, covering every early-exit path.
        let dacl_attrs: Option<(SECURITY_ATTRIBUTES, SecurityDescriptorGuard)> =
            self.build_security_attributes()?;

        loop {
            // CreateNamedPipeW returns a raw HANDLE (not a Result) in the
            // windows crate; a null/INVALID_HANDLE_VALUE means failure.
            let sa_ptr = dacl_attrs
                .as_ref()
                .map(|(sa, _)| sa as *const SECURITY_ATTRIBUTES)
                .unwrap_or(std::ptr::null());
            let pipe = unsafe {
                CreateNamedPipeW(
                    windows::core::PCWSTR(name.as_ptr()),
                    PIPE_ACCESS_DUPLEX,
                    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                    PIPE_UNLIMITED_INSTANCES,
                    4096,
                    4096,
                    0,
                    if sa_ptr.is_null() { None } else { Some(sa_ptr) },
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

            let resp = handler(pipe, &req);
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

    /// Convert `self.dacl_sddl` (if present) into a `SECURITY_ATTRIBUTES` whose
    /// descriptor is owned by the returned [`SecurityDescriptorGuard`].
    ///
    /// Fail-safe: a failed SDDL conversion returns `Err` so `run` never creates
    /// a pipe with an unintended (empty) security descriptor.
    fn build_security_attributes(
        &self,
    ) -> Result<Option<(SECURITY_ATTRIBUTES, SecurityDescriptorGuard)>, String> {
        let Some(sddl) = &self.dacl_sddl else {
            return Ok(None);
        };
        let sddl_u: Vec<u16> = sddl.encode_utf16().chain(std::iter::once(0)).collect();
        let mut psd = PSECURITY_DESCRIPTOR::default();
        let ok = unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                windows::core::PCWSTR(sddl_u.as_ptr()),
                SDDL_REVISION_1,
                &mut psd,
                None,
            )
        };
        if let Err(e) = ok {
            return Err(format!("ConvertStringSecurityDescriptorToSecurityDescriptorW: {e:?}"));
        }
        let sa = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: psd.0,
            bInheritHandle: false.into(),
        };
        Ok(Some((sa, SecurityDescriptorGuard(psd.0))))
    }
}

/// Determine whether the client connected to `pipe` holds an elevated token.
///
/// The check impersonates the named-pipe client (the calling thread's security
/// context becomes the client's), opens the calling thread's *impersonation*
/// token, and queries its `TokenElevation` level. [`OpenThreadToken`] — not
/// `OpenProcessToken` — is used on purpose: impersonation swaps the calling
/// thread's token while the process token is untouched, so
/// `OpenProcessToken(GetCurrentProcess())` would report the *service's* own
/// elevation, not the client's.
///
/// `RevertToSelf` runs on every path, including a token-open or token-read
/// failure, so the thread is never left impersonating a client after the check
/// (an impersonating thread would otherwise carry the client's identity into
/// the next `CreateNamedPipeW`/handler, and into any accidental cross-thread
/// access).
///
/// Fail closed: any API failure returns `Err`, which callers must treat as
/// "not elevated" (the `SaveConfig` gate does exactly that via `unwrap_or`).
pub fn is_client_elevated(pipe: HANDLE) -> Result<bool, String> {
    unsafe {
        ImpersonateNamedPipeClient(pipe).map_err(|e| format!("ImpersonateNamedPipeClient: {e}"))?;
    }

    // The thread's impersonation token after ImpersonateNamedPipeClient is the
    // connected client's token. `false` for bOpenAsSelf: the impersonation
    // token, not the process token, carries the client's identity.
    let mut token: HANDLE = HANDLE(std::ptr::null_mut());
    let open = unsafe { OpenThreadToken(GetCurrentThread(), TOKEN_QUERY, false, &mut token) };
    // Always revert, regardless of the token-open outcome.
    unsafe { RevertToSelf() }.map_err(|e| format!("RevertToSelf: {e}"))?;
    open.map_err(|e| format!("OpenThreadToken (client token): {e}"))?;

    let mut elev = TOKEN_ELEVATION::default();
    let mut sz = 0u32;
    let r = unsafe {
        GetTokenInformation(
            token,
            TokenElevation,
            Some((&mut elev as *mut TOKEN_ELEVATION).cast()),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut sz,
        )
    };
    let _ = unsafe { CloseHandle(token) };
    r.map_err(|e| format!("GetTokenInformation: {e}"))?;
    Ok(elev.TokenIsElevated != 0)
}

/// Owns a security descriptor allocated by
/// `ConvertStringSecurityDescriptorToSecurityDescriptorW`, freed via `LocalFree`
/// on drop. `run` has several early-`return Err` paths and a loop that runs
/// forever, so an RAII guard keeps the descriptor from leaking without a
/// central `defer`.
struct SecurityDescriptorGuard(*mut std::ffi::c_void);

impl Drop for SecurityDescriptorGuard {
    fn drop(&mut self) {
        if !self.0.is_null() {
            unsafe {
                let _ = LocalFree(Some(HLOCAL(self.0)));
            }
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

    // Serialize before opening the pipe so a serialization error can never
    // leak an opened handle: the early `?` below returns before `CreateFileW`.
    let req_buf = bincode::serialize(req).map_err(|e| format!("serialize: {e}"))?;

    // Wait for a connectable pipe instance, then open it, retrying up to 5
    // times. Only attempt CreateFileW once WaitNamedPipeW reports an instance
    // is ready: attempting it during the server's close-then-recreate window
    // races the previous CloseHandle and fails with ERROR_NO_PROCESS. A
    // successful wait can still race another client for the last instance, so
    // an ERROR_PIPE_BUSY CreateFileW retries too.
    let mut h = None;
    for _ in 0..5 {
        let ready =
            unsafe { WaitNamedPipeW(windows::core::PCWSTR(name.as_ptr()), 2000) }.as_bool();
        if !ready {
            continue;
        }
        // Blocking client pipe: no FILE_FLAG_OVERLAPPED.
        match unsafe {
            CreateFileW(
                windows::core::PCWSTR(name.as_ptr()),
                (GENERIC_READ | GENERIC_WRITE).0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                None,
                OPEN_EXISTING,
                FILE_FLAGS_AND_ATTRIBUTES::default(),
                None,
            )
        } {
            Ok(hh) => {
                h = Some(hh);
                break;
            }
            // ERROR_PIPE_BUSY: another client won the last instance.
            // ERROR_NO_PROCESS: the server's close-then-recreate window — a
            // CreateFileW during it races the previous CloseHandle. Both are
            // transient, so retry rather than fail.
            Err(e)
                if e.code() == ERROR_PIPE_BUSY.into() || e.code() == ERROR_NO_PROCESS.into() =>
            {
                continue;
            }
            Err(e) => return Err(format!("CreateFileW: {e}")),
        }
    }
    let h = h.ok_or_else(|| "CreateFileW: pipe unavailable after retries".to_string())?;

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
