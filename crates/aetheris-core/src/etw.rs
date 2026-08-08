//! Realtime ETW consumer for the `Microsoft-Windows-Kernel-Process` provider.
//!
//! Opens a kernel realtime trace session (`AetherisTrace`), enables the
//! kernel-process provider (Start/Stop events) and feeds decoded
//! [`ProcessEvent`]s into a channel for the policy engine. Requires elevation;
//! any setup failure returns `Err` so the service fails closed instead of
//! falling back to polling.
//!
//! Decode strategy: event id 1 = Start, 2 = Stop. The affected pid is decoded
//! from the payload — the TDH `ProcessId` property first, then a manual decode
//! of the kernel-process payload layout (ProcessId-first on modern Windows;
//! the documented UniqueProcessKey-first V1 layout is a fallback), with
//! `EVENT_HEADER.ProcessId` as a last resort (that header field for
//! kernel-process Start events is the *creating* process, not the affected
//! one; for Stop events it *is* the dying process). The image name and parent
//! pid are decoded through TDH (`TdhGetPropertySize`/`TdhGetProperty`) keyed by
//! property name, with a bounded fallback that parses the kernel-process
//! payload layout when TDH cannot resolve the schema.

use std::os::raw::c_void;
use std::sync::mpsc::{sync_channel, Receiver, SyncSender};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use windows::core::{GUID, PCWSTR, PWSTR};
use windows::Win32::Foundation::ERROR_SUCCESS;
use windows::Win32::System::Diagnostics::Etw::{
    CloseTrace, ControlTraceW, EnableTraceEx2, EVENT_CONTROL_CODE_ENABLE_PROVIDER,
    EVENT_RECORD, EVENT_TRACE_CONTROL_STOP, EVENT_TRACE_LOGFILEW, EVENT_TRACE_PROPERTIES,
    EVENT_TRACE_REAL_TIME_MODE, OpenTraceW, PROCESSTRACE_HANDLE, PROCESS_TRACE_MODE_EVENT_RECORD,
    PROCESS_TRACE_MODE_REAL_TIME, PROPERTY_DATA_DESCRIPTOR, ProcessTrace, StartTraceW,
    TdhGetProperty, TdhGetPropertySize, TRACE_LEVEL_INFORMATION, WNODE_FLAG_TRACED_GUID,
    CONTROLTRACE_HANDLE,
};

use crate::events::{ProcessEvent, ProcessKind};

/// Session name used for the realtime trace.
const SESSION_NAME: &str = "AetherisTrace";
/// `Microsoft-Windows-Kernel-Process` provider GUID
/// `{22FB2CD6-0E7B-422B-A0C7-2FAD1FD0E716}`. `GUID::from_u128` maps the u128 in
/// the same order as the crate's own constants (see `windows-core` `guid.rs`:
/// data4 is `to_be_bytes`), so this matches the canonical string form.
const KERNEL_PROCESS_PROVIDER: u128 = 0x22FB2CD6_0E7B_422B_A0C7_2FAD1FD0E716;
/// `WINEVENT_KEYWORD_PROCESS` from `winmeta.h`. The windows crate does not ship
/// it, so it is defined locally.
const WINEVENT_KEYWORD_PROCESS: u64 = 0x10;
/// `OpenTraceW` returns this on failure (the crate exposes no constant for it).
const INVALID_PROCESSTRACE_HANDLE: PROCESSTRACE_HANDLE = PROCESSTRACE_HANDLE { Value: u64::MAX };

/// Property names tried (in order) for the image name / parent pid.
///
/// The provider's manifest names the image-name field `ImageFileName` and the
/// parent `ParentId`, but the brief and several consumers document them as
/// `ProcessName` / `ParentPID`; trying both makes the decode resilient to the
/// naming drift across Windows versions.
const NAME_PROPERTIES: &[&str] = &["ProcessName", "ImageFileName", "UniqueProcessName"];
const PARENT_PROPERTIES: &[&str] = &["ParentPID", "ParentId", "ParentProcessId"];

/// Raw pointer wrapper that is `Send` (raw pointers are not). The pointee (the
/// channel sender) is only dereferenced on the thread that owns it.
struct SendPtr(*mut c_void);
// SAFETY: the wrapped pointer is only used from the single consumer thread that
// receives it; ownership is transferred once and never shared.
unsafe impl Send for SendPtr {}

/// Realtime kernel-process ETW monitor. Produces [`ProcessEvent`]s on its
/// channel; `start()` fails closed (returns `Err`) if the session cannot be
/// created, which is the intended fail-safe.
pub struct EtwMonitor {
    rx: Receiver<ProcessEvent>,
    handle: Option<JoinHandle<()>>,
    session_name: Vec<u16>,
    reg_handle: CONTROLTRACE_HANDLE,
    /// Owning pointer to the callback's channel sender clone; reclaimed once
    /// the consumer thread has stopped.
    ctx: SendPtr,
}

fn wstr(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Decode UTF-16 units (little-endian) up to the first NUL.
fn decode_utf16_units(bytes: &[u8]) -> String {
    let units: Vec<u16> = bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect();
    let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
    String::from_utf16_lossy(&units[..end])
}

/// TDH property value as a string, tolerating either a 4-byte little-endian
/// length prefix (a known quirk of some UnicodeString serializations) or a
/// plain (possibly NUL-terminated) UTF-16 payload.
fn property_string(bytes: &[u8]) -> Option<String> {
    if bytes.is_empty() {
        return None;
    }
    if bytes.len() >= 4 {
        let len = u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]) as usize;
        if len == 0 {
            // Zero-length prefix means the value is an empty string; do not
            // fall through and decode the prefix bytes as content.
            return None;
        }
        if bytes.len() >= 4 + len * 2 {
            let s = decode_utf16_units(&bytes[4..4 + len * 2]);
            if !s.is_empty() {
                return Some(s);
            }
        }
    }
    let s = decode_utf16_units(bytes);
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// File basename of a path/name string (so a full `ImageFileName` path still
/// yields `dummy_proc.exe`).
fn basename(path: &str) -> String {
    path.rsplit(['\\', '/'])
        .next()
        .unwrap_or(path)
        .trim_end_matches('\u{0}')
        .to_string()
}

/// Fetch one named property's raw bytes from an event record via TDH.
fn get_property(record: *const EVENT_RECORD, name: &str) -> Option<Vec<u8>> {
    let name_u = wstr(name);
    let desc = PROPERTY_DATA_DESCRIPTOR {
        PropertyName: name_u.as_ptr() as u64,
        ArrayIndex: u32::MAX,
        Reserved: 0,
    };
    let mut size = 0u32;
    let status = unsafe { TdhGetPropertySize(record, None, std::slice::from_ref(&desc), &mut size) };
    if status != 0 {
        return None;
    }
    let mut buf = vec![0u8; size as usize];
    let status = unsafe { TdhGetProperty(record, None, std::slice::from_ref(&desc), &mut buf) };
    if status != 0 {
        return None;
    }
    Some(buf)
}

/// Decode a little-endian `UInt32` from a TDH property value's raw bytes (the
/// kernel-process `ProcessId`/`ParentId` properties are `UInt32`).
fn decode_u32(bytes: &[u8]) -> Option<u32> {
    if bytes.len() >= 4 {
        Some(u32::from_le_bytes([bytes[0], bytes[1], bytes[2], bytes[3]]))
    } else {
        None
    }
}

/// Read a null-terminated UTF-16 string from raw payload bytes.
fn read_null_terminated_utf16(bytes: &[u8]) -> Option<String> {
    if bytes.len() < 2 {
        return None;
    }
    let mut units: Vec<u16> = Vec::new();
    for c in bytes.chunks_exact(2) {
        let u = u16::from_le_bytes([c[0], c[1]]);
        if u == 0 {
            break;
        }
        units.push(u);
    }
    let s = String::from_utf16_lossy(&units);
    if s.is_empty() {
        None
    } else {
        Some(s)
    }
}

/// Last-resort image-name decode from the kernel-process payload layout,
/// guarded by event version. Only meaningful for Start (id 1) events.
///
/// Layouts:
///   V0  (Process_V0_TypeGroup1): ProcessId u32, ParentId u32, SID, ImageFileName
///   V4  (modern Windows, measured): ProcessId u32, run-id u64, CreateTime
///        FILETIME, ParentId u32, parent run-id u64, SessionId u32, three
///        reserved u32, SID, ImageFileName (null-terminated UTF-16) — the SID
///        starts at offset 48 (0x30).
///   V1  (Process_TypeGroup1, documented): UniqueProcessKey ptr, ProcessId u32,
///        ParentId u32, SessionId u32, ExitStatus i32, DirectoryTableBase ptr,
///        SID, ImageFileName — the SID starts at 2*ptr+16.
fn decode_name_from_payload(record: *const EVENT_RECORD) -> Option<String> {
    let er = unsafe { &*record };
    if er.EventHeader.EventDescriptor.Id != 1 {
        return None;
    }
    let data =
        unsafe { std::slice::from_raw_parts(er.UserData as *const u8, er.UserDataLength as usize) };
    let ptr = std::mem::size_of::<usize>();
    let fixed = match er.EventHeader.EventDescriptor.Version {
        0 => 8,
        4 => 48,
        _ => 2 * ptr + 16,
    };
    if data.len() < fixed + 8 {
        return None;
    }
    // Skip the variable-length SID: byte0 revision, byte1 sub-authority count,
    // 6 authority bytes, then count * 4 bytes of sub-authorities.
    let sid_count = data[fixed + 1] as usize;
    let off = fixed + 8 + sid_count * 4;
    if off >= data.len() {
        return None;
    }
    let name = basename(&read_null_terminated_utf16(&data[off..])?);
    if name.is_empty() {
        None
    } else {
        Some(name)
    }
}

/// Last-resort affected-pid decode from the kernel-process payload layout.
/// The pid sits at offset 0 for `Process_V0_TypeGroup1` (V0) and for the modern
/// ProcessId-first layout (measured: Start ver 4 and Stop ver 2 on Windows 11),
/// and after the pointer-sized `UniqueProcessKey` for the documented V1
/// `Process_TypeGroup1` layout. Used for both Start (id 1) and Stop (id 2).
fn decode_pid_from_payload(data: &[u8], version: u8, ptr: usize) -> Option<u32> {
    let off = if version == 1 { ptr } else { 0 };
    if data.len() >= off + 4 {
        Some(u32::from_le_bytes([
            data[off],
            data[off + 1],
            data[off + 2],
            data[off + 3],
        ]))
    } else {
        None
    }
}

/// Last-resort parent-pid decode from the kernel-process payload layout.
fn decode_parent_pid_from_payload(record: *const EVENT_RECORD) -> Option<u32> {
    let er = unsafe { &*record };
    if er.EventHeader.EventDescriptor.Id != 1 {
        return None;
    }
    let data =
        unsafe { std::slice::from_raw_parts(er.UserData as *const u8, er.UserDataLength as usize) };
    let ptr = std::mem::size_of::<usize>();
    // ParentId: V0 right after ProcessId (offset 4); modern V4 after ProcessId,
    // run-id and CreateTime (offset ptr+12 = 0x14); documented V1 after the
    // pointer-sized UniqueProcessKey + 4-byte ProcessId (offset ptr+4).
    let off = match er.EventHeader.EventDescriptor.Version {
        0 => 4,
        4 => ptr + 12,
        _ => ptr + 4,
    };
    if data.len() >= off + 4 {
        Some(u32::from_le_bytes([
            data[off],
            data[off + 1],
            data[off + 2],
            data[off + 3],
        ]))
    } else {
        None
    }
}

fn decode_name(record: *const EVENT_RECORD) -> String {
    for prop in NAME_PROPERTIES {
        if let Some(bytes) = get_property(record, prop) {
            if let Some(s) = property_string(&bytes) {
                if !s.is_empty() {
                    return basename(&s);
                }
            }
        }
    }
    decode_name_from_payload(record).unwrap_or_default()
}

fn decode_parent_pid(record: *const EVENT_RECORD) -> u32 {
    for prop in PARENT_PROPERTIES {
        if let Some(bytes) = get_property(record, prop) {
            if let Some(pid) = decode_u32(&bytes) {
                return pid;
            }
        }
    }
    decode_parent_pid_from_payload(record).unwrap_or(0)
}

fn decode_event(record: *const EVENT_RECORD) -> Option<ProcessEvent> {
    let er = unsafe { &*record };
    let id = er.EventHeader.EventDescriptor.Id;
    let kind = match id {
        1 => ProcessKind::Start,
        2 => ProcessKind::Stop,
        _ => return None,
    };
    // `EVENT_HEADER.ProcessId` for kernel-process Start events is the
    // *creating* process (or System), not the affected one; the authoritative
    // pid lives in the payload. Prefer the TDH `ProcessId` property, then a
    // manual decode of the documented layout, and only fall back to the header
    // field if neither yields a value.
    let data = unsafe {
        std::slice::from_raw_parts(er.UserData as *const u8, er.UserDataLength as usize)
    };
    let ptr = std::mem::size_of::<usize>();
    let pid = get_property(record, "ProcessId")
        .and_then(|bytes| decode_u32(&bytes))
        .or_else(|| decode_pid_from_payload(data, er.EventHeader.EventDescriptor.Version, ptr))
        .unwrap_or(er.EventHeader.ProcessId);
    // Last-resort sanity check: a zero pid is never a usable event.
    if pid == 0 {
        return None;
    }
    let (name, parent_pid) = match kind {
        // Stop events carry no image name (and no parent); only the pid is
        // meaningful, so skip the name/parent TDH probes entirely.
        ProcessKind::Stop => (String::new(), 0),
        ProcessKind::Start => (decode_name(record), decode_parent_pid(record)),
    };
    Some(ProcessEvent {
        pid,
        name,
        parent_pid,
        kind,
    })
}

impl EtwMonitor {
    /// Create a realtime kernel trace session, enable the kernel-process
    /// provider, and spawn the `ProcessTrace` consumer thread. Any failure
    /// returns `Err(String)` (fail-safe: the service exits, never polls).
    pub fn start() -> Result<Self, String> {
        // Bounded so a stalled consumer cannot grow memory without limit; the
        // callback uses `try_send`, so a full buffer drops the next event
        // instead of blocking the ETW delivery thread.
        let (tx, rx) = sync_channel::<ProcessEvent>(4096);
        let session_name = wstr(SESSION_NAME);
        let name_bytes = session_name.len() * 2;

        // EVENT_TRACE_PROPERTIES with the session name embedded at
        // LoggerNameOffset. The buffer must outlive the StartTraceW call.
        let props_size = std::mem::size_of::<EVENT_TRACE_PROPERTIES>() + name_bytes + 2;
        let mut buf = vec![0u8; props_size];
        let props = buf.as_mut_ptr() as *mut EVENT_TRACE_PROPERTIES;
        unsafe {
            (*props).Wnode.BufferSize = props_size as u32;
            (*props).Wnode.Flags = WNODE_FLAG_TRACED_GUID;
            (*props).Wnode.ClientContext = 1; // QPC time base
            (*props).BufferSize = 128; // 128 KB
            (*props).MinimumBuffers = 5;
            (*props).MaximumBuffers = 25;
            (*props).FlushTimer = 1; // 1 s flush
            (*props).LogFileMode = EVENT_TRACE_REAL_TIME_MODE;
            (*props).LoggerNameOffset = std::mem::size_of::<EVENT_TRACE_PROPERTIES>() as u32;
            let dst = buf
                .as_mut_ptr()
                .add(std::mem::size_of::<EVENT_TRACE_PROPERTIES>())
                as *mut u16;
            std::ptr::copy_nonoverlapping(session_name.as_ptr(), dst, session_name.len());
        }

        let mut reg_handle = CONTROLTRACE_HANDLE::default();
        let status = unsafe { StartTraceW(&mut reg_handle, PCWSTR(session_name.as_ptr()), props) };
        if status != ERROR_SUCCESS {
            // A session with this name may already be running (e.g. a leftover
            // from a previous run). Stop it and retry once.
            let _ = unsafe {
                ControlTraceW(
                    CONTROLTRACE_HANDLE::default(),
                    PCWSTR(session_name.as_ptr()),
                    props,
                    EVENT_TRACE_CONTROL_STOP,
                )
            };
            let status2 = unsafe { StartTraceW(&mut reg_handle, PCWSTR(session_name.as_ptr()), props) };
            if status2 != ERROR_SUCCESS {
                return Err(format!("StartTraceW failed: status 0x{:08X}", status2.0));
            }
        }

        let provider = GUID::from_u128(KERNEL_PROCESS_PROVIDER);
        let enable_status = unsafe {
            EnableTraceEx2(
                reg_handle,
                &provider,
                EVENT_CONTROL_CODE_ENABLE_PROVIDER.0,
                TRACE_LEVEL_INFORMATION as u8,
                WINEVENT_KEYWORD_PROCESS,
                0, // match-all keyword
                0, // timeout
                None,
            )
        };
        if enable_status != ERROR_SUCCESS {
            let status = unsafe {
                ControlTraceW(
                    reg_handle,
                    PCWSTR(std::ptr::null()),
                    props,
                    EVENT_TRACE_CONTROL_STOP,
                )
            };
            if status != ERROR_SUCCESS {
                crate::log::warn(format!(
                    "ControlTraceW cleanup failed after EnableTraceEx2 error: status 0x{:08X}",
                    status.0
                ));
            }
            return Err(format!(
                "EnableTraceEx2 failed: status 0x{:08X}",
                enable_status.0
            ));
        }

        // Open a consumer on the same session, delivering EVENT_RECORDs. The
        // Context pointer is surfaced to the callback as
        // EVENT_RECORD.UserContext and carries a clone of the channel sender.
        let mut logfile: EVENT_TRACE_LOGFILEW = unsafe { std::mem::zeroed() };
        logfile.LoggerName = PWSTR(session_name.as_ptr() as *mut u16);
        logfile.Anonymous1.ProcessTraceMode =
            PROCESS_TRACE_MODE_REAL_TIME | PROCESS_TRACE_MODE_EVENT_RECORD;
        // Keep the boxed sender owned as a local `Box` across the `OpenTraceW`
        // call and `thread::spawn` so a panic on either path drops it instead
        // of leaking the allocation (a raw `SendPtr` has no destructor). It is
        // only intentionally leaked into `Self::ctx` once the consumer thread
        // exists, and reclaimed in `shutdown()`.
        let mut tx_box = Box::new(tx);
        let ctx = SendPtr(tx_box.as_mut() as *mut SyncSender<ProcessEvent> as *mut c_void);
        logfile.Context = ctx.0;
        logfile.Anonymous2.EventRecordCallback = Some(event_callback);

        let trace_handle = unsafe { OpenTraceW(&mut logfile) };
        if trace_handle == INVALID_PROCESSTRACE_HANDLE {
            // `tx_box` still owns the sender; it is dropped when we return.
            let status = unsafe {
                ControlTraceW(
                    reg_handle,
                    PCWSTR(std::ptr::null()),
                    props,
                    EVENT_TRACE_CONTROL_STOP,
                )
            };
            if status != ERROR_SUCCESS {
                crate::log::warn(format!(
                    "ControlTraceW cleanup failed after OpenTraceW error: status 0x{:08X}",
                    status.0
                ));
            }
            return Err("OpenTraceW failed".into());
        }

        // The consumer thread only needs the (Copy) trace handle; the callback
        // sender stays owned by this struct and is reclaimed in `shutdown()`
        // once ProcessTrace has returned (no callbacks can fire after that).
        let handle = thread::spawn(move || {
            let handles = [trace_handle];
            unsafe {
                let _ = ProcessTrace(&handles, None, None);
                let _ = CloseTrace(trace_handle);
            }
        });

        // Success: intentionally leak the Box into `ctx`.
        std::mem::forget(tx_box);

        Ok(Self {
            rx,
            handle: Some(handle),
            session_name,
            reg_handle,
            ctx,
        })
    }

    /// Blocking receive of the next process event.
    pub fn recv(&self) -> Option<ProcessEvent> {
        self.rx.recv().ok()
    }

    /// Receive with a timeout (useful for poll loops and the smoke test).
    pub fn recv_timeout(&self, timeout: Duration) -> Option<ProcessEvent> {
        self.rx.recv_timeout(timeout).ok()
    }

    /// Stop the trace session and join the consumer thread.
    pub fn stop(mut self) {
        self.shutdown();
    }

    fn shutdown(&mut self) {
        // Stop the session, then join the ProcessTrace thread. The properties
        // buffer must embed the session name and stay alive for the call.
        let name_bytes = self.session_name.len() * 2;
        let props_size = std::mem::size_of::<EVENT_TRACE_PROPERTIES>() + name_bytes + 2;
        let mut buf = vec![0u8; props_size];
        let props = buf.as_mut_ptr() as *mut EVENT_TRACE_PROPERTIES;
        unsafe {
            (*props).Wnode.BufferSize = props_size as u32;
            (*props).Wnode.Flags = WNODE_FLAG_TRACED_GUID;
            (*props).LoggerNameOffset = std::mem::size_of::<EVENT_TRACE_PROPERTIES>() as u32;
            let dst = buf
                .as_mut_ptr()
                .add(std::mem::size_of::<EVENT_TRACE_PROPERTIES>())
                as *mut u16;
            std::ptr::copy_nonoverlapping(self.session_name.as_ptr(), dst, self.session_name.len());
            let _ = ControlTraceW(
                self.reg_handle,
                PCWSTR(std::ptr::null()),
                props,
                EVENT_TRACE_CONTROL_STOP,
            );
        }
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
        // Reclaim the callback sender now that the trace has stopped.
        if !self.ctx.0.is_null() {
            unsafe { drop(Box::from_raw(self.ctx.0 as *mut SyncSender<ProcessEvent>)) };
            self.ctx.0 = std::ptr::null_mut();
        }
    }
}

impl Drop for EtwMonitor {
    fn drop(&mut self) {
        // Fail-safe resource cleanup if the caller forgets `stop()`.
        self.shutdown();
    }
}

unsafe extern "system" fn event_callback(record: *mut EVENT_RECORD) {
    if record.is_null() {
        return;
    }
    let tx_ptr = (*record).UserContext as *const SyncSender<ProcessEvent>;
    if tx_ptr.is_null() {
        return;
    }
    let tx = &*tx_ptr;
    if let Some(ev) = decode_event(record) {
        // Non-blocking: a full or disconnected channel is ignored, so the ETW
        // delivery thread is never stalled and the event is simply dropped.
        let _ = tx.try_send(ev);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Synthetic `EVENT_RECORD` with `UserData` pointing at the given payload.
    fn event_record_with_payload(version: u8, id: u16, payload: &mut Vec<u8>) -> EVENT_RECORD {
        let mut rec = EVENT_RECORD::default();
        rec.EventHeader.EventDescriptor.Version = version;
        rec.EventHeader.EventDescriptor.Id = id;
        rec.UserData = payload.as_mut_ptr() as *mut c_void;
        rec.UserDataLength = payload.len() as u16;
        rec
    }

    fn push_utf16(buf: &mut Vec<u8>, s: &str) {
        for u in s.encode_utf16() {
            buf.extend_from_slice(&u.to_le_bytes());
        }
    }

    #[test]
    fn property_string_handles_length_prefix_and_nul_terminated() {
        // 4-byte little-endian length prefix + UTF-16 payload ("dummy" = 5 units).
        let mut prefixed = vec![5u8, 0, 0, 0];
        push_utf16(&mut prefixed, "dummy");
        assert_eq!(property_string(&prefixed).as_deref(), Some("dummy"));

        // Plain NUL-terminated UTF-16, no prefix.
        let mut plain = Vec::new();
        push_utf16(&mut plain, "dummy_proc.exe");
        plain.extend_from_slice(&[0, 0]);
        assert_eq!(property_string(&plain).as_deref(), Some("dummy_proc.exe"));
    }

    #[test]
    fn property_string_zero_length_prefix_is_none() {
        // 4-byte zero length prefix means an empty string.
        assert_eq!(property_string(&[0u8, 0, 0, 0]), None);
        // Zero prefix followed by bytes that would otherwise decode as garbage
        // must not be read as content.
        let mut with_garbage = vec![0u8, 0, 0, 0];
        push_utf16(&mut with_garbage, "junk");
        assert_eq!(property_string(&with_garbage), None);
    }

    #[test]
    fn basename_extracts_file_from_full_path() {
        assert_eq!(basename("dummy_proc.exe"), "dummy_proc.exe");
        assert_eq!(basename(r"C:\Program Files\dummy_proc.exe"), "dummy_proc.exe");
        assert_eq!(basename("/x/y/z.exe"), "z.exe");
    }

    /// The documented `Process_TypeGroup1` (V1) layout: UniqueProcessKey(ptr),
    /// ProcessId, ParentId, SessionId, ExitStatus, DirectoryTableBase(ptr), SID,
    /// ImageFileName (NUL-terminated UTF-16). Kept as the name/parent decode
    /// fallback path for the pre-modern layout.
    #[test]
    fn payload_decode_v1_start() {
        let mut payload = Vec::new();
        payload.extend_from_slice(&[0u8; 8]); // UniqueProcessKey
        payload.extend_from_slice(&100u32.to_le_bytes()); // ProcessId
        payload.extend_from_slice(&50u32.to_le_bytes()); // ParentId
        payload.extend_from_slice(&7u32.to_le_bytes()); // SessionId
        payload.extend_from_slice(&0u32.to_le_bytes()); // ExitStatus
        payload.extend_from_slice(&[0u8; 8]); // DirectoryTableBase
        // SID: rev=1, 1 sub-authority, authority 5, sub-authority 10.
        payload.extend_from_slice(&[1, 1, 0, 0, 0, 0, 0, 5, 10, 0, 0, 0]);
        push_utf16(&mut payload, "dummy_proc.exe\0");
        let rec = event_record_with_payload(1, 1, &mut payload);
        assert_eq!(decode_name(&rec).to_ascii_lowercase(), "dummy_proc.exe");
        assert_eq!(decode_parent_pid(&rec), 50);
    }

    /// The `Process_V0_TypeGroup1` (V0) layout: ProcessId, ParentId, SID,
    /// ImageFileName.
    #[test]
    fn payload_decode_v0_start() {
        let mut payload = Vec::new();
        payload.extend_from_slice(&100u32.to_le_bytes()); // ProcessId
        payload.extend_from_slice(&50u32.to_le_bytes()); // ParentId
        payload.extend_from_slice(&[1, 0, 0, 0, 0, 0, 0, 5]); // SID: rev=1, 0 sub-authorities
        push_utf16(&mut payload, "legacy.exe\0");
        let rec = event_record_with_payload(0, 1, &mut payload);
        assert_eq!(decode_name(&rec).to_ascii_lowercase(), "legacy.exe");
        assert_eq!(decode_parent_pid(&rec), 50);
    }

    /// Stop (id 2) events carry no image name in the payload; decode must not
    /// fabricate one (the event itself is still emitted, keyed by pid).
    #[test]
    fn payload_decode_stop_has_no_name() {
        let mut payload = Vec::new();
        payload.extend_from_slice(&100u32.to_le_bytes()); // ProcessId
        payload.extend_from_slice(&0u32.to_le_bytes()); // ExitStatus
        let rec = event_record_with_payload(1, 2, &mut payload);
        assert!(decode_name_from_payload(&rec).is_none());
        assert!(decode_parent_pid_from_payload(&rec).is_none());
    }

    /// Modern `Process_TypeGroup1` (V4) Start layout as measured on Windows 11:
    /// ProcessId(u32), run-id(u64), CreateTime(FILETIME), ParentId(u32), parent
    /// run-id(u64), SessionId(u32), three reserved u32, SID, ImageFileName
    /// (NUL-terminated UTF-16). The affected pid is the first field; the same
    /// ProcessId-first shape is used by the modern Stop (ver 2) layout.
    fn build_modern_payload(pid: u32, parent: u32) -> Vec<u8> {
        let mut payload = Vec::new();
        payload.extend_from_slice(&pid.to_le_bytes()); // ProcessId @0
        payload.extend_from_slice(&[0u8; 8]); // run-id @4
        payload.extend_from_slice(&[0u8; 8]); // CreateTime @0x0C
        payload.extend_from_slice(&parent.to_le_bytes()); // ParentId @0x14
        payload.extend_from_slice(&[0u8; 8]); // parent run-id @0x18
        payload.extend_from_slice(&1u32.to_le_bytes()); // SessionId @0x20
        payload.extend_from_slice(&[0u8; 12]); // reserved @0x24..0x30
        // SID: rev=1, 1 sub-authority, authority 5, sub-authority 10.
        payload.extend_from_slice(&[1, 1, 0, 0, 0, 0, 0, 5, 10, 0, 0, 0]);
        push_utf16(&mut payload, "dummy_proc.exe\0");
        payload
    }

    #[test]
    fn decode_event_maps_ids_to_kinds() {
        // Modern Start (ver 4) and Stop (ver 2) share the ProcessId-first layout.
        let mut payload = build_modern_payload(42, 7);
        let mut rec = event_record_with_payload(4, 1, &mut payload);
        // The header pid is the *creating* process for Start events; the
        // payload pid must win.
        rec.EventHeader.ProcessId = 999;
        let ev = decode_event(&rec).expect("start event decodes");
        assert_eq!(ev.pid, 42, "pid decoded from payload over the header");
        assert_eq!(ev.name, "dummy_proc.exe");
        assert_eq!(ev.parent_pid, 7);
        assert_eq!(ev.kind, ProcessKind::Start);

        rec.EventHeader.EventDescriptor.Id = 2;
        rec.EventHeader.EventDescriptor.Version = 2; // modern Stop layout
        let ev = decode_event(&rec).expect("stop event decodes");
        assert_eq!(ev.pid, 42);
        assert_eq!(ev.kind, ProcessKind::Stop);
        assert_eq!(ev.name, "", "stop events carry no image name");
        assert_eq!(ev.parent_pid, 0, "stop events carry no parent");

        rec.EventHeader.EventDescriptor.Id = 99;
        assert!(decode_event(&rec).is_none(), "unknown ids are dropped");
    }

    /// V0 Start: pid at payload offset 0, so `decode_event` must pick it up
    /// there rather than from `EVENT_HEADER.ProcessId`.
    #[test]
    fn decode_event_v0_pid_from_payload_offset_zero() {
        let mut payload = Vec::new();
        payload.extend_from_slice(&777u32.to_le_bytes()); // ProcessId
        payload.extend_from_slice(&5u32.to_le_bytes()); // ParentId
        payload.extend_from_slice(&[1, 0, 0, 0, 0, 0, 0, 5]); // SID
        push_utf16(&mut payload, "legacy.exe\0");
        let mut rec = event_record_with_payload(0, 1, &mut payload);
        rec.EventHeader.ProcessId = 999;
        let ev = decode_event(&rec).expect("V0 start decodes");
        assert_eq!(ev.pid, 777, "V0 pid comes from payload offset 0");
        assert_eq!(ev.parent_pid, 5);
    }

    #[test]
    fn decode_u32_reads_little_endian() {
        assert_eq!(decode_u32(&[0x2A, 0x00, 0x00, 0x00]), Some(42));
        assert_eq!(decode_u32(&[0x2A, 0x00]), None);
    }

    #[test]
    fn decode_pid_from_payload_v0_offset_zero() {
        let mut payload = Vec::new();
        payload.extend_from_slice(&123u32.to_le_bytes()); // ProcessId at offset 0
        payload.extend_from_slice(&50u32.to_le_bytes()); // ParentId
        assert_eq!(decode_pid_from_payload(&payload, 0, 8), Some(123));
    }

    #[test]
    fn decode_pid_from_payload_modern_is_offset_zero() {
        // Modern ProcessId-first layout (Start ver 4 / Stop ver 2 measured on
        // Windows 11): the affected pid is the first payload field.
        let mut payload = Vec::new();
        payload.extend_from_slice(&456u32.to_le_bytes()); // ProcessId at offset 0
        assert_eq!(decode_pid_from_payload(&payload, 2, 8), Some(456));
        assert_eq!(decode_pid_from_payload(&payload, 4, 8), Some(456));
    }

    #[test]
    fn decode_pid_from_payload_v1_offset_ptr() {
        // Documented V1 TypeGroup1 layout: UniqueProcessKey first, pid at `ptr`.
        let mut payload = Vec::new();
        payload.extend_from_slice(&[0u8; 8]); // UniqueProcessKey
        payload.extend_from_slice(&456u32.to_le_bytes()); // ProcessId at offset ptr
        assert_eq!(decode_pid_from_payload(&payload, 1, 8), Some(456));
    }

    #[test]
    fn decode_pid_from_payload_short_payload_is_none() {
        let mut payload = Vec::new();
        payload.extend_from_slice(&0x04030201u32.to_le_bytes());
        // V1 needs `ptr + 4` bytes; a 4-byte payload is too short.
        assert_eq!(decode_pid_from_payload(&payload, 1, 8), None);
        // V0 needs only offset 0..4, so it still decodes.
        assert_eq!(decode_pid_from_payload(&payload, 0, 8), Some(0x04030201));
    }

    /// A zero pid anywhere in the chain must drop the event (the last-resort
    /// sanity guard), regardless of the header value.
    #[test]
    fn decode_event_drops_zero_pid() {
        let mut payload = Vec::new();
        payload.extend_from_slice(&0u32.to_le_bytes()); // ProcessId == 0
        payload.extend_from_slice(&50u32.to_le_bytes()); // ParentId
        let mut rec = event_record_with_payload(0, 1, &mut payload);
        rec.EventHeader.ProcessId = 5;
        assert!(decode_event(&rec).is_none(), "pid 0 events are dropped");
    }
}
