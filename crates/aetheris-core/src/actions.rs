//! Action executor: privilege enablement, priority, affinity, and memory trim.
//!
//! This is the OS-facing layer that turns a [`TargetAction`] into a real Windows
//! API call against a target process. [`OsBackend`] is the production backend;
//! the [`ProcessBackend`] trait is what the policy engine will drive.

use std::fmt;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::Security::{
    AdjustTokenPrivileges, LookupPrivilegeValueW, LUID_AND_ATTRIBUTES, SE_PRIVILEGE_ENABLED,
    TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetPriorityClass, GetProcessAffinityMask, OpenProcess, OpenProcessToken,
    SetPriorityClass, SetProcessAffinityMask, SetProcessWorkingSetSize, ABOVE_NORMAL_PRIORITY_CLASS,
    BELOW_NORMAL_PRIORITY_CLASS, HIGH_PRIORITY_CLASS, IDLE_PRIORITY_CLASS, NORMAL_PRIORITY_CLASS,
    PROCESS_ACCESS_RIGHTS, PROCESS_CREATION_FLAGS, PROCESS_QUERY_INFORMATION,
    PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_INFORMATION, PROCESS_SUSPEND_RESUME,
    PROCESS_TERMINATE, REALTIME_PRIORITY_CLASS,
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
    Job(String),
}

impl fmt::Display for ActionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ActionError::Open(code) => write!(f, "open process failed: code {code}"),
            ActionError::Api(m) => write!(f, "api: {m}"),
            ActionError::Job(m) => write!(f, "job: {m}"),
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
const PROCESS_RIGHTS: PROCESS_ACCESS_RIGHTS = PROCESS_ACCESS_RIGHTS(
    PROCESS_QUERY.0 | PROCESS_SET_INFORMATION.0 | PROCESS_SUSPEND_RESUME.0 | PROCESS_TERMINATE.0,
);

/// Build a core bitmask from a list of zero-based core indices.
pub fn mask_from_cores(cores: &[u8]) -> u64 {
    cores.iter().fold(0u64, |m, &c| m | (1u64 << c))
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

fn open_process(pid: u32) -> Result<HANDLE, ActionError> {
    let h = unsafe { OpenProcess(PROCESS_RIGHTS, false, pid) }
        .map_err(|e| ActionError::Open(e.code().0 as u32))?;
    Ok(h)
}

/// Production backend backed by the Windows process APIs.
pub struct OsBackend;

impl OsBackend {
    pub fn new() -> Self {
        Self
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
}

impl Default for OsBackend {
    fn default() -> Self {
        Self::new()
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
                    unsafe {
                        SetProcessAffinityMask(h, *core_mask as usize).map_err(|e| {
                            ActionError::Api(format!("SetProcessAffinityMask: {e}"))
                        })
                    }
                }
                TargetAction::TrimMemory => unsafe {
                    SetProcessWorkingSetSize(h, usize::MAX, usize::MAX).map_err(|e| {
                        ActionError::Api(format!("SetProcessWorkingSetSize: {e}"))
                    })
                },
                TargetAction::Suspend | TargetAction::Resume | TargetAction::QosCpuQuota { .. } => {
                    // Implemented in Task 7.
                    Err(ActionError::Api("not implemented yet".into()))
                }
            }
        })();
        let _ = unsafe { CloseHandle(h) };
        result
    }

    fn restore(&self, pid: u32, state: &ProcState) -> Result<(), ActionError> {
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
