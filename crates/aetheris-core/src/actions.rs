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
    PROCESS_ACCESS_RIGHTS, PROCESS_CREATION_FLAGS, PROCESS_MODE_BACKGROUND_BEGIN,
    PROCESS_QUERY_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_INFORMATION,
    PROCESS_SET_QUOTA, PROCESS_SUSPEND_RESUME, PROCESS_TERMINATE, REALTIME_PRIORITY_CLASS,
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
///
/// `PROCESS_SET_QUOTA` is required by `AssignProcessToJobObject` (QoS), and
/// `PROCESS_SUSPEND_RESUME` by `NtSuspendProcess`/`NtResumeProcess`.
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
/// Holds a per-pid map of Job Object handles used for CPU rate control (QoS).
/// A job is created lazily on first QoS assignment and kept so the quota can
/// be changed or cleared later without re-creating it.
pub struct OsBackend {
    jobs: std::sync::Mutex<std::collections::HashMap<u32, HANDLE>>,
}

impl OsBackend {
    pub fn new() -> Self {
        Self {
            jobs: std::sync::Mutex::new(std::collections::HashMap::new()),
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

    /// Apply a CPU rate-control quota to `pid` via a Job Object, or clear it.
    ///
    /// `percent == 0` clears any existing rate control (unlimited CPU).
    /// A non-zero quota is enforced as a per-job hard cap in 0.01% units. If
    /// the target cannot be assigned to a new job (it already lives in another
    /// job, common for browsers), fall back to Background Processing Mode, which
    /// lowers the process's I/O and CPU priority (spec §5.4).
    fn apply_qos(&self, pid: u32, percent: u32) -> Result<(), ActionError> {
        use windows::Win32::System::JobObjects::*;

        if percent == 0 {
            // Clear: disable CPU rate control on the existing job (unlimited).
            let jobs = self.jobs.lock().unwrap();
            if let Some(&job) = jobs.get(&pid) {
                let info = JOBOBJECT_CPU_RATE_CONTROL_INFORMATION {
                    ControlFlags: JOB_OBJECT_CPU_RATE_CONTROL(0),
                    ..Default::default()
                };
                unsafe {
                    SetInformationJobObject(
                        job,
                        JobObjectCpuRateControlInformation,
                        (&info as *const JOBOBJECT_CPU_RATE_CONTROL_INFORMATION).cast(),
                        std::mem::size_of::<JOBOBJECT_CPU_RATE_CONTROL_INFORMATION>() as u32,
                    )
                }
                .map_err(|e| ActionError::Job(format!("clear rate control: {e}")))?;
            }
            return Ok(());
        }

        // Find-or-create a job for this pid.
        let mut jobs = self.jobs.lock().unwrap();
        let job = match jobs.get(&pid) {
            Some(&j) => j,
            None => {
                let j = unsafe { CreateJobObjectW(None, PCWSTR::null()) }
                    .map_err(|e| ActionError::Job(format!("CreateJobObjectW: {e}")))?;
                jobs.insert(pid, j);
                j
            }
        };

        let info = JOBOBJECT_CPU_RATE_CONTROL_INFORMATION {
            ControlFlags: JOB_OBJECT_CPU_RATE_CONTROL_ENABLE | JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP,
            Anonymous: JOBOBJECT_CPU_RATE_CONTROL_INFORMATION_0 {
                CpuRate: percent * 100, // per-job hard cap, in units of 0.01%
            },
        };
        unsafe {
            SetInformationJobObject(
                job,
                JobObjectCpuRateControlInformation,
                (&info as *const JOBOBJECT_CPU_RATE_CONTROL_INFORMATION).cast(),
                std::mem::size_of::<JOBOBJECT_CPU_RATE_CONTROL_INFORMATION>() as u32,
            )
        }
        .map_err(|e| ActionError::Job(format!("set rate control: {e}")))?;

        let h = open_process(pid)?;
        let assigned = unsafe { AssignProcessToJobObject(job, h) };
        if assigned.is_err() {
            // Fallback (spec §5.4): target already in a job (common for browsers).
            // Background Processing Mode lowers the process's I/O and CPU priority.
            let fb = unsafe { SetPriorityClass(h, PROCESS_MODE_BACKGROUND_BEGIN) };
            let _ = unsafe { CloseHandle(h) };
            return match fb {
                Ok(()) => Ok(()),
                Err(e) => Err(ActionError::Job(format!(
                    "assign-to-job and background-mode fallback both failed: {e}"
                ))),
            };
        }
        let _ = unsafe { CloseHandle(h) };
        Ok(())
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
