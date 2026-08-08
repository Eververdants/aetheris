//! Action executor: privilege enablement, priority, affinity, and memory trim.
//!
//! This is the OS-facing layer that turns a [`TargetAction`] into a real Windows
//! API call against a target process. [`OsBackend`] is the production backend;
//! the [`ProcessBackend`] trait is what the policy engine will drive.

use std::fmt;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, GetLastError, HANDLE};
use windows::Win32::Security::{
    AdjustTokenPrivileges, LookupPrivilegeValueW, LUID_AND_ATTRIBUTES, SE_PRIVILEGE_ENABLED,
    TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
};
use windows::Win32::System::SystemInformation::GROUP_AFFINITY;
use windows::Win32::System::Threading::{
    ALL_PROCESSOR_GROUPS, GetActiveProcessorCount, GetCurrentProcess, GetPriorityClass,
    GetProcessAffinityMask, OpenProcess, OpenProcessToken, SetPriorityClass,
    SetProcessAffinityMask, SetProcessDefaultCpuSetMasks, SetProcessWorkingSetSize,
    ABOVE_NORMAL_PRIORITY_CLASS, BELOW_NORMAL_PRIORITY_CLASS, HIGH_PRIORITY_CLASS,
    IDLE_PRIORITY_CLASS, NORMAL_PRIORITY_CLASS, PROCESS_ACCESS_RIGHTS, PROCESS_CREATION_FLAGS,
    PROCESS_MODE_BACKGROUND_BEGIN, PROCESS_MODE_BACKGROUND_END, PROCESS_QUERY_INFORMATION,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_INFORMATION, PROCESS_SET_QUOTA,
    PROCESS_SUSPEND_RESUME, PROCESS_TERMINATE, REALTIME_PRIORITY_CLASS,
};

use crate::config::PriorityClass;

/// An action that can be applied to a target process.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetAction {
    Priority(PriorityClass),
    Affinity { core_mask: u64 },
    Suspend,
    Resume,
    TrimMemory,
    QosCpuQuota { percent: u32 },
}

/// Snapshot of a process's tunable state, for later restore.
#[derive(Debug, Clone, Copy, Default)]
pub struct ProcState {
    pub priority: u32,
    pub affinity: u64,
    pub suspended: bool,
    pub qos_percent: Option<u32>,
}

/// Errors returned by the action executor.
#[derive(Debug)]
pub enum ActionError {
    Open(u32),
    Api(String),
}

impl fmt::Display for ActionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ActionError::Open(code) => write!(f, "open process failed: code {code}"),
            ActionError::Api(m) => write!(f, "api: {m}"),
        }
    }
}

impl std::error::Error for ActionError {}

/// Query-only access rights for observing a process.
///
/// NOTE (deviation from brief): the brief built this with
/// `PROCESS_QUERY_INFORMATION | PROCESS_QUERY_LIMITED_INFORMATION` in a `const`
/// context, but windows 0.62's `BitOr` for these flag types is not `const fn`.
/// We compose from the raw `.0` u32 values instead (identical mask).
pub const PROCESS_QUERY: PROCESS_ACCESS_RIGHTS = PROCESS_ACCESS_RIGHTS(
    PROCESS_QUERY_INFORMATION.0 | PROCESS_QUERY_LIMITED_INFORMATION.0,
);

/// Full access rights needed to apply every action (query, set, suspend, terminate).
///
/// `PROCESS_SET_QUOTA` is required by `SetProcessWorkingSetSize` (memory trim),
/// and `PROCESS_SUSPEND_RESUME` by `NtSuspendProcess`/`NtResumeProcess`.
const PROCESS_RIGHTS: PROCESS_ACCESS_RIGHTS = PROCESS_ACCESS_RIGHTS(
    PROCESS_QUERY.0
        | PROCESS_SET_INFORMATION.0
        | PROCESS_SET_QUOTA.0
        | PROCESS_SUSPEND_RESUME.0
        | PROCESS_TERMINATE.0,
);

/// Build a core bitmask from a list of zero-based core indices.
pub fn mask_from_cores(cores: &[u8]) -> u64 {
    cores.iter().fold(0u64, |m, &c| m | (1u64 << c))
}

/// Total number of logical processors across every processor group.
///
/// `GetActiveProcessorCount(ALL_PROCESSOR_GROUPS)` returns the grand total,
/// which exceeds 64 only on dual-group (or larger) hosts — the signal that
/// [`OsBackend::apply`] must take the group-aware CPU-Sets affinity path.
pub fn logical_cpu_count() -> u32 {
    unsafe { GetActiveProcessorCount(ALL_PROCESSOR_GROUPS) }
}

/// Builds the [`GROUP_AFFINITY`] entries for `SetProcessDefaultCpuSetMasks` from
/// a flat list of zero-based core indices. Each index `c` in `0..64` maps to bit
/// `c` of processor group 0 — the same group the flat u64 `core_mask` already
/// spans on a single-group host.
///
/// Returns `None` when the list is empty, contains an index `>= 64` (not
/// expressible as a group-0 mask), or holds more than 64 entries; the caller
/// should skip the pin rather than pin nothing.
///
/// DEV-from-brief: the brief specified a raw
/// `PROCESS_DEFAULT_CPU_SET_INFORMATION` byte buffer, but that struct does not
/// exist in windows 0.62.2 (verified in the crate source), and 0.62.2's
/// `SetProcessDefaultCpuSetMasks` takes a slice of typed [`GROUP_AFFINITY`]
/// structs. We return the typed slice instead of a raw `Vec<u8>`; alignment is
/// therefore guaranteed by the type system (the brief's alignment check existed
/// only because it fed a raw-pointer API).
pub fn build_cpu_set_mask(cores: &[u8]) -> Option<Vec<GROUP_AFFINITY>> {
    if cores.is_empty() || cores.len() > 64 || cores.iter().any(|&c| c >= 64) {
        return None;
    }
    Some(
        cores
            .iter()
            .map(|&c| GROUP_AFFINITY {
                Mask: 1usize << c,
                Group: 0,
                Reserved: [0; 3],
            })
            .collect(),
    )
}

fn to_windows_priority(p: PriorityClass) -> PROCESS_CREATION_FLAGS {
    match p {
        PriorityClass::Idle => IDLE_PRIORITY_CLASS,
        PriorityClass::BelowNormal => BELOW_NORMAL_PRIORITY_CLASS,
        PriorityClass::Normal => NORMAL_PRIORITY_CLASS,
        PriorityClass::AboveNormal => ABOVE_NORMAL_PRIORITY_CLASS,
        PriorityClass::High => HIGH_PRIORITY_CLASS,
        PriorityClass::Realtime => REALTIME_PRIORITY_CLASS,
    }
}

/// Backend contract for applying actions to a process.
pub trait ProcessBackend {
    fn snapshot(&self, pid: u32) -> Result<ProcState, ActionError>;
    fn apply(&self, pid: u32, action: &TargetAction) -> Result<(), ActionError>;
    fn restore(&self, pid: u32, state: &ProcState) -> Result<(), ActionError>;
}

pub(crate) fn open_process(pid: u32) -> Result<HANDLE, ActionError> {
    let h = unsafe { OpenProcess(PROCESS_RIGHTS, false, pid) }
        .map_err(|e| ActionError::Open(e.code().0 as u32))?;
    Ok(h)
}

/// Production backend backed by the Windows process APIs.
///
/// Tracks which pids are currently in Background Processing Mode so a later
/// clear can reverse exactly what was applied. v1 creates no Job Objects:
/// QoS is Background Processing Mode (see [`OsBackend::apply_qos`] for why).
pub struct OsBackend {
    background_mode: std::sync::Mutex<std::collections::HashSet<u32>>,
}

impl OsBackend {
    pub fn new() -> Self {
        Self {
            background_mode: std::sync::Mutex::new(std::collections::HashSet::new()),
        }
    }

    /// Enable the privileges required to touch other processes:
    /// `SeDebugPrivilege` (open + query protected/system processes) and
    /// `SeIncreaseBasePriorityPrivilege` (raise base priority).
    pub fn enable_privileges(&self) -> Result<(), ActionError> {
        // NOTE (deviation from brief): windows 0.62 moved the string literal macro
        // from `windows::s!` to `windows::core::w!` (which produces a wide PCWSTR).
        self.enable_privilege(windows::core::w!("SeDebugPrivilege"))?;
        self.enable_privilege(windows::core::w!("SeIncreaseBasePriorityPrivilege"))
    }

    fn enable_privilege(&self, name: PCWSTR) -> Result<(), ActionError> {
        unsafe {
            // NOTE (deviation from brief): `HANDLE(0)` does not type-check against the
            // `*mut c_void` field in current rustc; build the null handle explicitly.
            let mut token: HANDLE = HANDLE(std::ptr::null_mut());
            let open_result = OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
                &mut token,
            )
            .map_err(|e| ActionError::Api(format!("OpenProcessToken: {e}")));

            let lookup_result = open_result.and_then(|()| {
                let mut luid = windows::Win32::Foundation::LUID::default();
                LookupPrivilegeValueW(None, name, &mut luid)
                    .map_err(|e| ActionError::Api(format!("LookupPrivilegeValueW: {e}")))
                    .map(|()| luid)
            });

            let adjust_result = lookup_result.and_then(|luid| {
                let tp = TOKEN_PRIVILEGES {
                    PrivilegeCount: 1,
                    Privileges: [LUID_AND_ATTRIBUTES {
                        Luid: luid,
                        Attributes: SE_PRIVILEGE_ENABLED,
                    }],
                };
                AdjustTokenPrivileges(token, false, Some(&tp), 0, None, None)
                    .map_err(|e| ActionError::Api(format!("AdjustTokenPrivileges: {e}")))
            });

            let _ = CloseHandle(token);
            adjust_result
        }
    }

    /// Apply or clear CPU throttling for `pid` via Background Processing Mode.
    ///
    /// `percent > 0` enters Background Processing Mode (`SetPriorityClass(...,
    /// PROCESS_MODE_BACKGROUND_BEGIN)`), which lowers the process's resource
    /// scheduling priorities (spec §5.4); `percent == 0` leaves it again.
    ///
    /// Why Background Processing Mode and not a Job Object (v1 decision):
    /// Job Object QoS is deferred to v2, and NOT because closing the last job
    /// handle kills processes — a job only terminates its assigned processes on
    /// handle close when `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` is set, which was
    /// never the case here. The real reason is that Background Processing Mode,
    /// the v1 mechanism, is current-process-only (see the documented OS
    /// limitation below), so `qos_cpu_quota` is a documented no-op for external
    /// processes in v1. A real cross-process CPU cap needs Job Objects with
    /// clear-on-stop semantics (the job and its caps are cleaned up when the
    /// service stops), which is deferred to v2.
    ///
    /// Documented OS limitation: MSDN states `PROCESS_MODE_BACKGROUND_BEGIN` /
    /// `END` "can be specified only if hProcess is a handle to the current
    /// process". aetheris manages *other* processes, so on most hosts this call
    /// fails with ERROR_INVALID_PARAMETER and the engine logs the apply as a
    /// warning (priority/affinity/suspend still apply). That is acceptable for
    /// v1: the mechanism is safe and reversible by construction, and a real
    /// cross-process CPU cap (via `NtSetInformationProcess` or Job Objects with
    /// clear-on-stop semantics) is deferred to v2.
    fn apply_qos(&self, pid: u32, percent: u32) -> Result<(), ActionError> {
        if percent == 0 {
            // Clear: reverse Background Processing Mode only if we applied it.
            // Calling it when the pid was never put into background mode is a
            // no-op (the engine only issues a clear after a successful apply).
            let mut bg = self.background_mode.lock().unwrap();
            if bg.remove(&pid) {
                let h = open_process(pid)?;
                let r = unsafe { SetPriorityClass(h, PROCESS_MODE_BACKGROUND_END) };
                let _ = unsafe { CloseHandle(h) };
                r.map_err(|e| ActionError::Api(format!("PROCESS_MODE_BACKGROUND_END: {e}")))?;
            }
            return Ok(());
        }

        let mut bg = self.background_mode.lock().unwrap();
        if bg.contains(&pid) {
            // Already in background mode (idempotent re-apply); nothing to do.
            return Ok(());
        }
        let h = open_process(pid)?;
        let r = unsafe { SetPriorityClass(h, PROCESS_MODE_BACKGROUND_BEGIN) };
        let _ = unsafe { CloseHandle(h) };
        r.map_err(|e| ActionError::Api(format!("PROCESS_MODE_BACKGROUND_BEGIN: {e}")))?;
        bg.insert(pid);
        Ok(())
    }
}

impl Default for OsBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for OsBackend {
    fn drop(&mut self) {
        // Intentionally do NOT close any Job Object handle here. Note that
        // closing the last job handle does NOT kill assigned processes — a job
        // only terminates its processes when `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`
        // is set (it never was). The real v1 story: QoS is Background Processing
        // Mode, tracked in `background_mode`, which is current-process-only, so
        // the flag on the (about to exit) service is left as-is and is safe —
        // there are no Job Objects to clean up. When Job Object QoS lands in v2
        // it must be cleaned up with clear-on-stop semantics, not by relying on
        // handle-close behavior.
    }
}

impl ProcessBackend for OsBackend {
    fn snapshot(&self, pid: u32) -> Result<ProcState, ActionError> {
        let h = open_process(pid)?;
        let priority = unsafe { GetPriorityClass(h) };
        let mut mask: usize = 0;
        let mut sys: usize = 0;
        let r = unsafe { GetProcessAffinityMask(h, &mut mask, &mut sys) }
            .map_err(|e| ActionError::Api(format!("GetProcessAffinityMask: {e}")));
        let _ = unsafe { CloseHandle(h) };
        r?;
        // `suspended`/`qos_percent` are intentionally left at their defaults:
        // this is a PRE-action snapshot and aetheris never restores a
        // suspension or QoS cap it did not apply. The policy engine records
        // what IT applied into the stored `ProcState` (`apply_background_to`),
        // which is what drives `restore()` to Resume / clear a cap.
        Ok(ProcState {
            priority,
            affinity: mask as u64,
            suspended: false,
            qos_percent: None,
        })
    }

    fn apply(&self, pid: u32, action: &TargetAction) -> Result<(), ActionError> {
        let h = open_process(pid)?;
        let result = (|| {
            match action {
                TargetAction::Priority(p) => unsafe {
                    SetPriorityClass(h, to_windows_priority(*p))
                        .map_err(|e| ActionError::Api(format!("SetPriorityClass: {e}")))
                },
                TargetAction::Affinity { core_mask } => {
                    if *core_mask == 0 {
                        return Err(ActionError::Api("affinity mask is zero".into()));
                    }
                    if logical_cpu_count() > 64 {
                        // Group-aware path: the flat u64 mask only spans processor
                        // group 0 (cores 0..63); rebuild the requested core indices
                        // and express each as a GROUP_AFFINITY entry so
                        // SetProcessDefaultCpuSetMasks can pin them. Reuses `h`
                        // (PROCESS_RIGHTS includes PROCESS_SET_INFORMATION).
                        // NOTE: this API sets the process DEFAULT CPU-set mask.
                        // Whether already-running threads are re-pinned by a
                        // default-mask change is unverified (needs a >64-logical-CPU
                        // host); if they are not, new threads are still pinned while
                        // existing ones keep their prior scheduling — verify on a
                        // dual-group host before relying on this path in production.
                        let cores: Vec<u8> =
                            (0..64u8).filter(|i| (*core_mask >> *i) & 1 == 1).collect();
                        match build_cpu_set_mask(&cores) {
                            Some(entries) => unsafe {
                                let r = SetProcessDefaultCpuSetMasks(h, Some(&entries));
                                if !r.as_bool() {
                                    // DEV-from-brief: the brief propagated the API
                                    // error, but the task demands "log::warn + skip
                                    // (never crash, never mis-pin)" — warn and skip,
                                    // mirroring apply_qos's warn-and-continue pattern,
                                    // so one failed pin does not fail the whole rule.
                                    let code = GetLastError();
                                    let e = windows::core::Error::from_hresult(
                                        windows::core::HRESULT::from_win32(code.0),
                                    );
                                    crate::log::warn(format!(
                                        "affinity: SetProcessDefaultCpuSetMasks failed ({e}); skipping"
                                    ));
                                }
                                Ok(())
                            },
                            None => {
                                crate::log::warn(
                                    "affinity: >64 CPUs but CPU-set buffer build failed; skipping",
                                );
                                Ok(())
                            }
                        }
                    } else {
                        unsafe {
                            SetProcessAffinityMask(h, *core_mask as usize).map_err(|e| {
                                ActionError::Api(format!("SetProcessAffinityMask: {e}"))
                            })
                        }
                    }
                }
                TargetAction::TrimMemory => unsafe {
                    SetProcessWorkingSetSize(h, usize::MAX, usize::MAX).map_err(|e| {
                        ActionError::Api(format!("SetProcessWorkingSetSize: {e}"))
                    })
                },
                TargetAction::Suspend => {
                    // NOTE (deviation from brief): ntapi's NtSuspendProcess returns an
                    // NTSTATUS (i32), not a Result, and its HANDLE is a `*mut c_void`
                    // from winapi — distinct from windows::Win32's HANDLE. We cast the
                    // pointer and treat a negative status as failure.
                    let status =
                        unsafe { ntapi::ntpsapi::NtSuspendProcess(h.0 as *mut ntapi::winapi::ctypes::c_void) };
                    if status < 0 {
                        Err(ActionError::Api(format!(
                            "NtSuspendProcess: 0x{:08X}",
                            status as u32
                        )))
                    } else {
                        Ok(())
                    }
                }
                TargetAction::Resume => {
                    let status =
                        unsafe { ntapi::ntpsapi::NtResumeProcess(h.0 as *mut ntapi::winapi::ctypes::c_void) };
                    if status < 0 {
                        Err(ActionError::Api(format!(
                            "NtResumeProcess: 0x{:08X}",
                            status as u32
                        )))
                    } else {
                        Ok(())
                    }
                }
                TargetAction::QosCpuQuota { percent } => self.apply_qos(pid, *percent),
            }
        })();
        let _ = unsafe { CloseHandle(h) };
        result
    }

    fn restore(&self, pid: u32, state: &ProcState) -> Result<(), ActionError> {
        if state.suspended {
            self.apply(pid, &TargetAction::Resume)?;
        }
        if state.qos_percent.is_some() {
            self.apply(pid, &TargetAction::QosCpuQuota { percent: 0 })?;
        }
        self.apply(pid, &TargetAction::Priority(state_priority_to_class(state)))?;
        self.apply(pid, &TargetAction::Affinity { core_mask: state.affinity })?;
        Ok(())
    }
}

/// Map a captured raw Windows priority class constant back to a [`PriorityClass`].
fn state_priority_to_class(state: &ProcState) -> PriorityClass {
    match state.priority {
        p if p == IDLE_PRIORITY_CLASS.0 => PriorityClass::Idle,
        p if p == BELOW_NORMAL_PRIORITY_CLASS.0 => PriorityClass::BelowNormal,
        p if p == NORMAL_PRIORITY_CLASS.0 => PriorityClass::Normal,
        p if p == ABOVE_NORMAL_PRIORITY_CLASS.0 => PriorityClass::AboveNormal,
        p if p == HIGH_PRIORITY_CLASS.0 => PriorityClass::High,
        p if p == REALTIME_PRIORITY_CLASS.0 => PriorityClass::Realtime,
        _ => PriorityClass::Normal,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn logical_cpu_count_sane() {
        let n = logical_cpu_count();
        // DEV-from-brief: brief wrote `n >= 1 && n <= 1024`, which clippy's
        // manual_range_contains flags; use the range form instead.
        assert!(
            (1..=1024).contains(&n),
            "implausible logical CPU count {n}"
        );
    }

    #[test]
    fn build_cpu_set_mask_for_cores() {
        use windows::Win32::System::SystemInformation::GROUP_AFFINITY;

        // Two cores -> one GROUP_AFFINITY entry per core, all in group 0.
        let entries = build_cpu_set_mask(&[0u8, 1u8]).expect("cores map to entries");
        assert_eq!(entries.len(), 2);
        assert_eq!(
            entries[0],
            GROUP_AFFINITY {
                Mask: 1,
                Group: 0,
                Reserved: [0; 3],
            }
        );
        assert_eq!(
            entries[1],
            GROUP_AFFINITY {
                Mask: 2,
                Group: 0,
                Reserved: [0; 3],
            }
        );

        // Highest valid index 63 maps to the top bit of the group-0 mask.
        let top = build_cpu_set_mask(&[63]).expect("core 63 valid");
        assert_eq!(top[0].Mask, 1usize << 63);
        assert_eq!(top[0].Group, 0);

        // Empty core list -> None (caller skips / falls back).
        assert!(build_cpu_set_mask(&[]).is_none());

        // Core index >= 64 cannot be expressed in a group-0 mask -> None.
        assert!(build_cpu_set_mask(&[64]).is_none());

        // More than 64 entries -> None (guard against pathological configs).
        let too_many: Vec<u8> = (0..65).collect();
        assert!(build_cpu_set_mask(&too_many).is_none());

        // Entries are aligned structs; byte size is a multiple of alignment.
        let sz = std::mem::size_of::<GROUP_AFFINITY>();
        let al = std::mem::align_of::<GROUP_AFFINITY>();
        assert_eq!(sz % al, 0, "GROUP_AFFINITY byte size must be alignment-multiple");
    }
}
