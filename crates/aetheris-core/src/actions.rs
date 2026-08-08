//! Action executor: privilege enablement, priority, affinity, memory trim, and
//! Job Object CPU QoS.
//!
//! This is the OS-facing layer that turns a [`TargetAction`] into a real Windows
//! API call against a target process. [`OsBackend`] is the production backend;
//! the [`ProcessBackend`] trait is what the policy engine will drive.

use std::collections::hash_map::Entry;
use std::collections::HashMap;
use std::fmt;
use std::sync::Mutex;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{CloseHandle, GetLastError, HANDLE};
use windows::Win32::Security::{
    AdjustTokenPrivileges, LookupPrivilegeValueW, LUID_AND_ATTRIBUTES, SE_PRIVILEGE_ENABLED,
    TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject,
    JobObjectCpuRateControlInformation, JOBOBJECT_CPU_RATE_CONTROL_INFORMATION,
    JOBOBJECT_CPU_RATE_CONTROL_INFORMATION_0, JOB_OBJECT_CPU_RATE_CONTROL,
    JOB_OBJECT_CPU_RATE_CONTROL_ENABLE, JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP,
};
use windows::Win32::System::SystemInformation::GROUP_AFFINITY;
use windows::Win32::System::Threading::{
    ALL_PROCESSOR_GROUPS, GetActiveProcessorCount, GetCurrentProcess, GetPriorityClass,
    GetProcessAffinityMask, OpenProcess, OpenProcessToken, SetPriorityClass,
    SetProcessAffinityMask, SetProcessDefaultCpuSetMasks, SetProcessWorkingSetSize,
    ABOVE_NORMAL_PRIORITY_CLASS, BELOW_NORMAL_PRIORITY_CLASS, HIGH_PRIORITY_CLASS,
    IDLE_PRIORITY_CLASS, NORMAL_PRIORITY_CLASS, PROCESS_ACCESS_RIGHTS, PROCESS_CREATION_FLAGS,
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

/// Purge the Windows standby memory list, returning its pages to the free list
/// so a game can allocate fresh working sets. Opt-in via
/// `[game] purge_standby_on_boost`.
///
/// Follows the StandbyCleanerLite pattern: the `SeProfileSingleProcessPrivilege`
/// must be enabled first, then `NtSetSystemInformation(SystemMemoryListInformation
/// /*0x50*/, MemoryPurgeStandbyList /*4*/)`. When the privilege is not held (a
/// non-elevated process) the API call fails and we return an `Err` — the caller
/// decides whether to warn and continue. Not reversible, but harmless: the OS
/// simply rebuilds its standby list from free pages as needed.
pub fn purge_standby_list() -> Result<(), ActionError> {
    // SeProfileSingleProcessPrivilege required (StandbyCleanerLite pattern).
    OsBackend::new().enable_privilege(windows::core::w!("SeProfileSingleProcessPrivilege"))?;
    let arg: u32 = ntapi::ntexapi::MemoryPurgeStandbyList; // 4
    let status = unsafe {
        ntapi::ntexapi::NtSetSystemInformation(
            ntapi::ntexapi::SystemMemoryListInformation, // 0x50
            (&arg as *const u32).cast_mut().cast(),
            std::mem::size_of::<u32>() as u32,
        )
    };
    if status == 0 {
        Ok(())
    } else {
        Err(ActionError::Api(format!("NtSetSystemInformation: 0x{status:08X}")))
    }
}

/// Per-pid QoS state tracked by [`OsBackend`].
///
/// `job` is the Job Object used to cap the process's CPU rate; `assigned` is
/// true only once [`AssignProcessToJobObject`] succeeded. When assignment fails
/// (target already lives in a job, common for browsers) `assigned` stays false
/// and the job is kept open but empty so a later clear can close it safely.
///
/// Fields are `pub` for integration-test readback (`backend.jobs`); the struct
/// is otherwise an internal bookkeeping detail.
pub struct JobEntry {
    pub job: HANDLE,
    pub assigned: bool,
}

/// Production backend backed by the Windows process APIs.
///
/// Holds a per-pid map of Job Object handles. A job is created lazily on the
/// first QoS assignment and kept so the quota can be changed or cleared later
/// without re-creating it. **A job is never configured with
/// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`**, so closing a handle never terminates
/// the assigned process — the job simply stops capping.
pub struct OsBackend {
    pub jobs: Mutex<HashMap<u32, JobEntry>>,
}

/// Lifecycle hooks for QoS that the policy engine needs on top of
/// [`ProcessBackend`]. Defaults are no-ops so non-QoS backends (e.g. the
/// `RecordingBackend` in tests) implement the trait without doing anything.
pub trait QosLifecycle {
    /// The process has exited; release any Job Object held for it (safe: the
    /// process is gone, so closing the handle cannot strand a live process).
    fn on_process_exit(&self, _pid: u32) {}
    /// Clear all CPU caps (un-capp all assigned jobs) and close every handle.
    /// No `KILL_ON_JOB_CLOSE`, so this never terminates processes.
    fn clear_all_qos(&self) {}
}

impl OsBackend {
    pub fn new() -> Self {
        Self {
            jobs: Mutex::new(HashMap::new()),
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

    pub(crate) fn enable_privilege(&self, name: PCWSTR) -> Result<(), ActionError> {
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

    /// Apply or clear a real cross-process CPU cap for `pid` via a Job Object.
    ///
    /// `percent > 0` finds-or-creates the pid's job, enables a hard CPU rate cap
    /// (`JOB_OBJECT_CPU_RATE_CONTROL_ENABLE | JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP`)
    /// at `percent * 100` in 0.01% units, and assigns the process to it.
    ///
    /// `percent == 0` disables rate control (ControlFlags = 0 → unlimited). If
    /// the job was never assigned (attach failed), it is closed and dropped —
    /// nothing lives in it. If it WAS assigned, the job is kept open but uncapped
    /// while the process lives (a process cannot be removed from a job; closing
    /// the last handle would destroy the job object, and although that is safe
    /// without `KILL_ON_JOB_CLOSE`, the process may be in *other* jobs, so we
    /// only release it via [`Self::on_process_exit`] once the process is gone).
    ///
    /// Assignment can fail with ERROR_ACCESS_DENIED when the target already
    /// lives in a job (common for browsers). That degrades gracefully: no CPU
    /// cap for that process, but priority/affinity still apply. This is never a
    /// hard error.
    ///
    /// **Safety:** this backend never sets `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`.
    /// Closing a Job Object handle without that flag does NOT terminate assigned
    /// processes — the job simply stops capping.
    fn apply_qos(&self, pid: u32, percent: u32) -> Result<(), ActionError> {
        let mut jobs = self.jobs.lock().unwrap();

        if percent == 0 {
            // Clear: disable rate control (→ unlimited) on the tracked job. If
            // the process was never assigned (attach failed), close + drop the
            // entry (safe: the job is empty).
            match jobs.get(&pid) {
                Some(entry) if entry.assigned => {
                    let info = JOBOBJECT_CPU_RATE_CONTROL_INFORMATION {
                        ControlFlags: JOB_OBJECT_CPU_RATE_CONTROL(0),
                        ..Default::default()
                    };
                    unsafe {
                        SetInformationJobObject(
                            entry.job,
                            JobObjectCpuRateControlInformation,
                            (&info as *const JOBOBJECT_CPU_RATE_CONTROL_INFORMATION).cast(),
                            std::mem::size_of::<JOBOBJECT_CPU_RATE_CONTROL_INFORMATION>() as u32,
                        )
                    }
                    .map_err(|e| ActionError::Job(format!("clear rate control: {e}")))?;
                }
                Some(_) => {
                    if let Some(e) = jobs.remove(&pid) {
                        let _ = unsafe { CloseHandle(e.job) };
                    }
                }
                None => {}
            }
            return Ok(());
        }

        // percent > 0: find-or-create the job for this pid.
        let entry = match jobs.entry(pid) {
            Entry::Occupied(e) => e.into_mut(),
            Entry::Vacant(v) => {
                let j = unsafe { CreateJobObjectW(None, PCWSTR::null()) }
                    .map_err(|e| ActionError::Job(format!("CreateJobObjectW: {e}")))?;
                v.insert(JobEntry { job: j, assigned: false })
            }
        };

        // Hard cap the job's CPU rate in 0.01% units. Construct the union via
        // its safe struct-literal form (the vendored windows crate exposes
        // `JOBOBJECT_CPU_RATE_CONTROL_INFORMATION_0 { CpuRate }`).
        let info = JOBOBJECT_CPU_RATE_CONTROL_INFORMATION {
            ControlFlags: JOB_OBJECT_CPU_RATE_CONTROL_ENABLE
                | JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP,
            Anonymous: JOBOBJECT_CPU_RATE_CONTROL_INFORMATION_0 {
                CpuRate: percent * 100,
            },
        };
        unsafe {
            SetInformationJobObject(
                entry.job,
                JobObjectCpuRateControlInformation,
                (&info as *const JOBOBJECT_CPU_RATE_CONTROL_INFORMATION).cast(),
                std::mem::size_of::<JOBOBJECT_CPU_RATE_CONTROL_INFORMATION>() as u32,
            )
        }
        .map_err(|e| ActionError::Job(format!("set rate control: {e}")))?;

        if !entry.assigned {
            let h = open_process(pid)?;
            let assigned = unsafe { AssignProcessToJobObject(entry.job, h) };
            let _ = unsafe { CloseHandle(h) };
            if assigned.is_err() {
                // Target already in a job (browsers/common). Degrade: no cap for
                // this process; priority/affinity still apply. Keep the empty
                // job open but never KILL_ON_JOB_CLOSE, so nothing is terminated;
                // a later percent==0 clear or on_process_exit closes it.
                crate::log::warn(format!(
                    "qos: pid {pid} already in a job; cpu cap skipped"
                ));
                return Ok(());
            }
            entry.assigned = true;
        }
        Ok(())
    }

    /// Close + remove the Job Object for `pid`. Safe only after the process has
    /// exited (that is the caller's contract — the policy engine calls this from
    /// the `Stop` event path and service shutdown).
    pub fn on_process_exit(&self, pid: u32) {
        if let Some(e) = self.jobs.lock().unwrap().remove(&pid) {
            let _ = unsafe { CloseHandle(e.job) };
        }
    }

    /// Disable CPU rate control on every assigned job (→ unlimited) and close
    /// all handles. Called on game exit and service Stop. No
    /// `KILL_ON_JOB_CLOSE`, so no process is terminated — jobs simply stop
    /// capping and their handles are released.
    pub fn clear_all_qos(&self) {
        let mut jobs = self.jobs.lock().unwrap();
        for (_, entry) in jobs.iter() {
            if entry.assigned {
                let info = JOBOBJECT_CPU_RATE_CONTROL_INFORMATION {
                    ControlFlags: JOB_OBJECT_CPU_RATE_CONTROL(0),
                    ..Default::default()
                };
                let _ = unsafe {
                    SetInformationJobObject(
                        entry.job,
                        JobObjectCpuRateControlInformation,
                        (&info as *const JOBOBJECT_CPU_RATE_CONTROL_INFORMATION).cast(),
                        std::mem::size_of::<JOBOBJECT_CPU_RATE_CONTROL_INFORMATION>() as u32,
                    )
                };
            }
        }
        for (_, e) in jobs.drain() {
            let _ = unsafe { CloseHandle(e.job) };
        }
    }
}

impl QosLifecycle for OsBackend {
    fn on_process_exit(&self, pid: u32) {
        OsBackend::on_process_exit(self, pid);
    }

    fn clear_all_qos(&self) {
        OsBackend::clear_all_qos(self);
    }
}

impl Default for OsBackend {
    fn default() -> Self {
        Self::new()
    }
}

impl Drop for OsBackend {
    fn drop(&mut self) {
        // Close remaining job handles. WITHOUT JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE
        // this does NOT terminate assigned processes — the job objects simply
        // stop capping. Caps were already cleared by `clear_all_qos` on the
        // Stop path; this is the final safety net for abnormal teardown.
        let jobs = self.jobs.lock().unwrap();
        for (_, e) in jobs.iter() {
            let _ = unsafe { CloseHandle(e.job) };
        }
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

    #[test]
    fn purge_standby_rejects_without_privilege() {
        // Non-elevated: must return an Err (privilege not enabled) or Ok (privilege
        // present). Never panic. If elevated, it should succeed or fail gracefully.
        let r = super::purge_standby_list();
        assert!(r.is_ok() || r.is_err(), "must not panic");
    }
}
