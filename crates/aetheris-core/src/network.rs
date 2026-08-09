//! Reversible network QoS tweaks, applied on game-mode entry and reverted on
//! exit. Opt-in via `[network]` config (all flags default off).
//!
//! - Nagle disable: `TcpAckFrequency=1` + `TCPNoDelay=1` (DWORD) on every
//!   TCP/IP interface adapter under
//!   `HKLM\SYSTEM\CurrentControlSet\Services\Tcpip\Parameters\Interfaces\<GUID>`.
//! - Optional NetBIOS disable: `DisableNetbiosOverTcpip=2` (DWORD) under
//!   `HKLM\SYSTEM\CurrentControlSet\Services\NetBT\Parameters`.
//!
//! Every write is read-before-write: [`apply`] snapshots the previous DWORD
//! value (or its absence) into a [`BackupEntry`]; [`revert`] restores it (or
//! deletes the value that did not exist before). The backup is held by the
//! policy engine for the duration of GameBoost.
//!
//! Registry access is via the `windows::Win32::System::Registry` API. The
//! service runs elevated, which HKLM writes require. The low-level helpers are
//! parameterized over a root [`HKEY`] so the unit test can exercise the exact
//! backup/apply/revert mechanics against a scoped test key under `HKCU` without
//! ever touching real network settings.

use std::path::{Path, PathBuf};

use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{
    ERROR_FILE_NOT_FOUND, ERROR_NO_MORE_ITEMS, ERROR_SUCCESS, ERROR_UNSUPPORTED_TYPE,
};
use windows::Win32::System::Registry::{
    RegCloseKey, RegDeleteValueW, RegEnumKeyExW, RegGetValueW, RegOpenKeyExW, RegSetValueExW,
    HKEY, HKEY_LOCAL_MACHINE, KEY_READ, KEY_WRITE, REG_DWORD, REG_SAM_FLAGS, RRF_RT_REG_DWORD,
};

/// Root of the TCP/IP interface adapter list under HKLM.
const TCPIP_INTERFACES_KEY: &str = "SYSTEM\\CurrentControlSet\\Services\\Tcpip\\Parameters\\Interfaces";
/// NetBT parameters key (NetBIOS over TCP/IP) under HKLM.
const NETBT_PARAMETERS_KEY: &str = "SYSTEM\\CurrentControlSet\\Services\\NetBT\\Parameters";

/// A registry value as it was before a network tweak, so the tweak can be
/// reversed on game exit. `path` is the full registry path (relative to the
/// hive root), `value_name` is the value, and `old` is the prior DWORD (`None`
/// means the value did not exist before the tweak).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct BackupEntry {
    pub path: String,
    pub value_name: String,
    pub old: Option<u32>,
}

/// Encode a Rust string as a null-terminated UTF-16 buffer for the wide-char
/// registry APIs.
fn to_wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Open `path` under `root` with the given access rights.
fn open_key(root: HKEY, path: &str, access: REG_SAM_FLAGS) -> Result<HKEY, String> {
    let wide = to_wide(path);
    let mut hkey = HKEY::default();
    let err = unsafe { RegOpenKeyExW(root, PCWSTR(wide.as_ptr()), None, access, &mut hkey) };
    if err != ERROR_SUCCESS {
        return Err(format!("RegOpenKeyExW({path}): code {}", err.0));
    }
    Ok(hkey)
}

/// Read a DWORD value. Returns `Ok(None)` if the value does not exist.
fn read_dword_from_key(hkey: HKEY, value: &str) -> Result<Option<u32>, String> {
    let wide = to_wide(value);
    let mut data: u32 = 0;
    let mut size = std::mem::size_of::<u32>() as u32;
    let err = unsafe {
        RegGetValueW(
            hkey,
            PCWSTR::null(),
            PCWSTR(wide.as_ptr()),
            RRF_RT_REG_DWORD,
            None,
            Some((&mut data as *mut u32).cast()),
            Some(&mut size),
        )
    };
    if err == ERROR_FILE_NOT_FOUND {
        return Ok(None);
    }
    if err == ERROR_UNSUPPORTED_TYPE {
        // A pre-existing value of another type (e.g. REG_SZ where a DWORD is
        // expected) surfaces as 1630. Callers treat this as a per-entry skip
        // rather than a whole-apply abort, so give it a distinct message.
        return Err(format!(
            "RegGetValueW({value}): existing value is not REG_DWORD (code {})",
            err.0
        ));
    }
    if err != ERROR_SUCCESS {
        return Err(format!("RegGetValueW({value}): code {}", err.0));
    }
    Ok(Some(data))
}

fn read_dword(root: HKEY, path: &str, value: &str) -> Result<Option<u32>, String> {
    let hkey = open_key(root, path, KEY_READ)?;
    let result = read_dword_from_key(hkey, value);
    let _ = unsafe { RegCloseKey(hkey) };
    result
}

fn write_dword_to_key(hkey: HKEY, value: &str, dword: u32) -> Result<(), String> {
    let wide = to_wide(value);
    // NOTE (deviation from brief): the brief described the DWORD data pointer as
    // `&dword as *const u32 as *const c_void`, but windows 0.62.2's
    // `RegSetValueExW` takes `lpdata: Option<&[u8]>` (verified in the vendored
    // crate). We pass the little-endian byte slice directly.
    let bytes = dword.to_le_bytes();
    let err = unsafe {
        RegSetValueExW(hkey, PCWSTR(wide.as_ptr()), None, REG_DWORD, Some(&bytes))
    };
    if err != ERROR_SUCCESS {
        return Err(format!("RegSetValueExW({value}): code {}", err.0));
    }
    Ok(())
}

fn write_dword(root: HKEY, path: &str, value: &str, dword: u32) -> Result<(), String> {
    let hkey = open_key(root, path, KEY_WRITE)?;
    let result = write_dword_to_key(hkey, value, dword);
    let _ = unsafe { RegCloseKey(hkey) };
    result
}

fn delete_value(root: HKEY, path: &str, value: &str) -> Result<(), String> {
    let hkey = open_key(root, path, KEY_WRITE)?;
    let result = (|| {
        let wide = to_wide(value);
        let err = unsafe { RegDeleteValueW(hkey, PCWSTR(wide.as_ptr())) };
        if err != ERROR_SUCCESS {
            return Err(format!("RegDeleteValueW({value}): code {}", err.0));
        }
        Ok(())
    })();
    let _ = unsafe { RegCloseKey(hkey) };
    result
}

/// Snapshot the current DWORD (or its absence), then set it to `set_to`.
fn backup_and_set(root: HKEY, path: &str, value: &str, set_to: u32) -> Result<BackupEntry, String> {
    let old = read_dword(root, path, value)?;
    write_dword(root, path, value, set_to)?;
    Ok(BackupEntry { path: path.to_string(), value_name: value.to_string(), old })
}

/// Restore one entry: write back the prior value, or delete the value if it did
/// not exist before the tweak.
fn revert_entry(root: HKEY, entry: &BackupEntry) -> Result<(), String> {
    match entry.old {
        Some(v) => write_dword(root, &entry.path, &entry.value_name, v),
        None => delete_value(root, &entry.path, &entry.value_name),
    }
}

/// Enumerate the immediate subkey names under `path` (adapter GUIDs).
fn enum_subkeys(root: HKEY, path: &str) -> Result<Vec<String>, String> {
    let hkey = open_key(root, path, KEY_READ)?;
    let mut names = Vec::new();
    let mut index = 0u32;
    loop {
        let mut buf = [0u16; 256];
        let mut len = buf.len() as u32;
        let err = unsafe {
            RegEnumKeyExW(
                hkey,
                index,
                Some(PWSTR(buf.as_mut_ptr())),
                &mut len,
                None,
                None,
                None,
                None,
            )
        };
        if err == ERROR_NO_MORE_ITEMS {
            break;
        }
        if err != ERROR_SUCCESS {
            let _ = unsafe { RegCloseKey(hkey) };
            return Err(format!("RegEnumKeyExW({path}): code {}", err.0));
        }
        names.push(String::from_utf16_lossy(&buf[..len as usize]));
        index += 1;
    }
    let _ = unsafe { RegCloseKey(hkey) };
    Ok(names)
}

/// Apply the Nagle tweaks (backup + set `TcpAckFrequency=1`/`TCPNoDelay=1`) on
/// every interface adapter, and the optional NetBIOS disable. Returns the backup
/// so [`revert`] can reverse every change. Per-value registry failures are
/// logged and skipped (best-effort), so the returned backup always covers every
/// value that was actually modified. Errors if Nagle is requested but no
/// interface adapters are enumerated.
pub fn apply(nagle: bool, netbios: bool) -> Result<Vec<BackupEntry>, String> {
    apply_at(
        HKEY_LOCAL_MACHINE,
        TCPIP_INTERFACES_KEY,
        NETBT_PARAMETERS_KEY,
        nagle,
        netbios,
    )
}

/// [`apply`] parameterized over the hive root and registry paths so the unit
/// tests can drive the exact mid-enumeration failure path against a scoped
/// `HKCU` test key. Production always uses `HKEY_LOCAL_MACHINE`.
fn apply_at(
    root: HKEY,
    interfaces_path: &str,
    netbt_path: &str,
    nagle: bool,
    netbios: bool,
) -> Result<Vec<BackupEntry>, String> {
    let mut backup = Vec::new();
    if nagle {
        let adapters = enum_subkeys(root, interfaces_path)?;
        if adapters.is_empty() {
            return Err("no TCP/IP interface adapters found under Tcpip\\Parameters\\Interfaces".into());
        }
        // Best-effort per value. A single unreadable/unwritable key must not
        // abort the whole apply: the old `?` dropped the partially populated
        // backup on a later adapter failure, stranding earlier modified values
        // with no revert path. Log-and-continue so the returned backup still
        // covers every entry actually modified.
        for adapter in adapters {
            let path = format!("{interfaces_path}\\{adapter}");
            for value in ["TcpAckFrequency", "TCPNoDelay"] {
                match backup_and_set(root, &path, value, 1) {
                    Ok(entry) => backup.push(entry),
                    Err(err) => crate::log::warn(format!(
                        "network apply {path}\\{value}: {err}; skipping this value"
                    )),
                }
            }
        }
    }
    if netbios {
        match backup_and_set(root, netbt_path, "DisableNetbiosOverTcpip", 2) {
            Ok(entry) => backup.push(entry),
            Err(err) => crate::log::warn(format!(
                "network apply {netbt_path}\\DisableNetbiosOverTcpip: {err}; skipping"
            )),
        }
    }
    Ok(backup)
}

/// Revert every entry back to its pre-boost state. Failures are logged and
/// swallowed — a registry hiccup must never fail the game flow.
pub fn revert(entries: &[BackupEntry]) {
    for e in entries {
        if let Err(err) = revert_entry(HKEY_LOCAL_MACHINE, e) {
            crate::log::warn(format!("network revert {}\\{}: {err}", e.path, e.value_name));
        }
    }
}

/// Default crash-reconciliation marker path: `%PROGRAMDATA%\aetheris\network-tweaks.marker`
/// (falling back to `C:\ProgramData` when the env var is unset). Living under
/// PROGRAMDATA — not the config dir — lets the marker survive a config move.
pub fn default_marker_path() -> PathBuf {
    let base = std::env::var_os("PROGRAMDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(r"C:\ProgramData"));
    base.join("aetheris").join("network-tweaks.marker")
}

/// Persist `entries` as the crash marker at `path`. The marker is bincode-encoded
/// so [`reconcile`] can read the exact backup back on the next startup and revert
/// only what `apply` actually modified. Creates the parent directory.
///
/// Guard: a marker is only meaningful when it lists at least one applied value,
/// so an empty backup is rejected (the policy engine also guards before calling).
pub fn write_marker(entries: &[BackupEntry], path: &Path) -> Result<(), String> {
    if entries.is_empty() {
        return Err("write_marker: refusing to persist an empty backup".into());
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|e| format!("create marker dir {}: {e}", parent.display()))?;
    }
    let bytes = bincode::serialize(entries).map_err(|e| format!("serialize marker: {e}"))?;
    std::fs::write(path, bytes).map_err(|e| format!("write marker {}: {e}", path.display()))
}

/// Read the crash marker back as the persisted backup. `None` when no marker
/// exists — or when it cannot be read/decoded, so a corrupt marker never blocks
/// startup (it is simply treated as absent and left for cleanup).
pub fn read_marker(path: &Path) -> Option<Vec<BackupEntry>> {
    let bytes = std::fs::read(path).ok()?;
    bincode::deserialize(&bytes).ok()
}

/// Delete the crash marker. Missing file is a no-op.
pub fn remove_marker(path: &Path) {
    let _ = std::fs::remove_file(path);
}

/// Reconcile stale network-QoS tweaks after a service death mid-GameBoost: if
/// the crash marker exists, revert every entry it lists, then remove the marker.
///
/// Safe by construction: it never guesses what was applied — it reverts only the
/// entries the marker says the service set, and only when the marker exists.
pub fn reconcile(path: &Path) {
    let Some(entries) = read_marker(path) else {
        return;
    };
    revert(&entries);
    remove_marker(path);
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegDeleteKeyW, HKEY_CURRENT_USER, KEY_ALL_ACCESS,
        REG_OPTION_NON_VOLATILE, REG_SZ,
    };

    const TEST_VALUE: &str = "TestValue";

    /// Create (or open) the scoped test key under HKCU. Closes the returned
    /// handle; callers use the fixed path with the low-level helpers.
    fn ensure_test_key(path: &str) -> Result<(), String> {
        let wide = to_wide(path);
        let mut hkey = HKEY::default();
        let err = unsafe {
            RegCreateKeyExW(
                HKEY_CURRENT_USER,
                PCWSTR(wide.as_ptr()),
                None,
                None,
                REG_OPTION_NON_VOLATILE,
                KEY_ALL_ACCESS,
                None,
                &mut hkey,
                None,
            )
        };
        let _ = unsafe { RegCloseKey(hkey) };
        if err != ERROR_SUCCESS {
            return Err(format!("RegCreateKeyExW({path}): code {}", err.0));
        }
        Ok(())
    }

    fn remove_test_key(path: &str) {
        let wide = to_wide(path);
        unsafe {
            let _ = RegDeleteKeyW(HKEY_CURRENT_USER, PCWSTR(wide.as_ptr()));
        }
    }

    /// Write a `REG_SZ` value. Used to plant a non-DWORD under a test key so the
    /// DWORD read path hits the `ERROR_UNSUPPORTED_TYPE` branch.
    fn write_string_value(root: HKEY, path: &str, value: &str, text: &str) -> Result<(), String> {
        let hkey = open_key(root, path, KEY_WRITE)?;
        let result = (|| {
            let wide = to_wide(value);
            let data = text
                .encode_utf16()
                .chain(std::iter::once(0))
                .collect::<Vec<u16>>();
            let bytes: &[u8] =
                unsafe { std::slice::from_raw_parts(data.as_ptr().cast(), data.len() * 2) };
            let err =
                unsafe { RegSetValueExW(hkey, PCWSTR(wide.as_ptr()), None, REG_SZ, Some(bytes)) };
            if err != ERROR_SUCCESS {
                return Err(format!("RegSetValueExW({value}): code {}", err.0));
            }
            Ok(())
        })();
        let _ = unsafe { RegCloseKey(hkey) };
        result
    }

    /// Backup/apply/revert roundtrip against a scoped test key under HKCU, so
    /// the test never touches real network settings. Write 5 → backup → apply 1
    /// → assert 1 → revert → assert 5.
    #[test]
    fn registry_backup_apply_revert_roundtrip() {
        // Each test uses its own key path so the parallel test harness never
        // shares (and clobbers) the same HKCU key.
        let path = "Software\\AetherisTests\\Roundtrip";
        remove_test_key(path);
        ensure_test_key(path).expect("create test key");

        // Write 5.
        write_dword(HKEY_CURRENT_USER, path, TEST_VALUE, 5).expect("write 5");
        assert_eq!(
            read_dword(HKEY_CURRENT_USER, path, TEST_VALUE).expect("read").unwrap(),
            5
        );

        // Backup + apply 1.
        let entry = backup_and_set(HKEY_CURRENT_USER, path, TEST_VALUE, 1).expect("backup+set");
        assert_eq!(entry.old, Some(5), "backup captured prior value");
        assert_eq!(entry.value_name, TEST_VALUE);
        assert_eq!(
            read_dword(HKEY_CURRENT_USER, path, TEST_VALUE).expect("read").unwrap(),
            1,
            "applied value is 1"
        );

        // Revert → back to 5.
        revert_entry(HKEY_CURRENT_USER, &entry).expect("revert");
        assert_eq!(
            read_dword(HKEY_CURRENT_USER, path, TEST_VALUE).expect("read").unwrap(),
            5,
            "reverted value is 5"
        );

        remove_test_key(path);
    }

    /// When the value did not exist before the tweak, revert must delete it.
    #[test]
    fn revert_deletes_when_value_was_absent() {
        let path = "Software\\AetherisTests\\Absent";
        remove_test_key(path);
        ensure_test_key(path).expect("create test key");

        // No prior value: backup captures `old = None`.
        let entry = backup_and_set(HKEY_CURRENT_USER, path, TEST_VALUE, 1).expect("backup+set");
        assert_eq!(entry.old, None, "absent value backed up as None");
        assert_eq!(
            read_dword(HKEY_CURRENT_USER, path, TEST_VALUE).expect("read").unwrap(),
            1
        );

        // Revert deletes the applied value.
        revert_entry(HKEY_CURRENT_USER, &entry).expect("revert");
        assert_eq!(
            read_dword(HKEY_CURRENT_USER, path, TEST_VALUE).expect("read"),
            None,
            "revert deleted the applied value"
        );

        remove_test_key(path);
    }

    /// With both tweaks disabled, `apply` is a no-op returning an empty backup
    /// and never touches the registry.
    #[test]
    fn apply_noop_when_flags_off() {
        let backup = apply(false, false).expect("no-op apply");
        assert!(backup.is_empty(), "nothing applied when all flags off");
    }

    /// Marker persistence + crash reconciliation against a temp path:
    ///
    /// - `write_marker`/`read_marker` roundtrip the backup byte-for-byte, so a
    ///   startup `reconcile` reverts exactly the values `apply` actually
    ///   modified (and nothing else).
    /// - `remove_marker` (normal game exit) clears it, so the next startup
    ///   reconciles nothing.
    /// - `reconcile` consumes the marker: it reads back the listed entries,
    ///   reverts them (`revert` — covered end-to-end at the registry level by
    ///   `apply_continues_past_per_adapter_failure_and_reverts_what_applied`),
    ///   and removes the marker, so a second reconcile is a no-op. It only ever
    ///   acts on the entries the marker lists — never reverts on a guess.
    /// - The empty guard: a marker is only meaningful when it lists ≥1 entry,
    ///   so `write_marker` refuses an empty backup (the policy wires the same
    ///   guard before calling it).
    #[test]
    fn marker_roundtrip_and_reconcile() {
        let dir = std::env::temp_dir().join(format!("aetheris_net_{}", std::process::id()));
        let path = dir.join("marker.bin");
        let entries = vec![
            BackupEntry {
                path: "Software\\AetherisTests\\MarkerA".into(),
                value_name: "V1".into(),
                old: Some(5),
            },
            BackupEntry {
                path: "Software\\AetherisTests\\MarkerB".into(),
                value_name: "V2".into(),
                old: None,
            },
        ];

        // Roundtrip: the marker stores exactly what was written.
        write_marker(&entries, &path).expect("write marker");
        assert_eq!(read_marker(&path).expect("read marker"), entries);

        // Normal exit removes the marker: the next startup reconciles nothing.
        remove_marker(&path);
        assert!(read_marker(&path).is_none(), "removed marker must not be read");

        // A leftover crash marker is reconciled away: entries reverted and the
        // marker removed. A second reconcile sees no marker and is a no-op
        // (never reverts unless the marker exists).
        write_marker(&entries, &path).expect("write marker again");
        reconcile(&path);
        assert!(
            read_marker(&path).is_none(),
            "reconcile must remove the marker it consumed"
        );
        reconcile(&path); // no marker -> nothing to revert, no panic

        // Empty guard: never persist a marker that lists nothing to revert.
        let err = write_marker(&[], &path);
        assert!(err.is_err(), "write_marker must reject an empty backup");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// A mid-enumeration failure must not strand earlier entries: `apply` logs
    /// and continues, and the returned backup still covers every value it
    /// actually modified, so reverting it restores each one. Also exercises the
    /// non-DWORD (`ERROR_UNSUPPORTED_TYPE`) read path.
    #[test]
    fn apply_continues_past_per_adapter_failure_and_reverts_what_applied() {
        let base = "Software\\AetherisTests\\ApplyContinue";
        let path_a = format!("{base}\\A");
        let path_b = format!("{base}\\B");
        // Leaf subkeys first, then the parent key.
        remove_test_key(&path_a);
        remove_test_key(&path_b);
        remove_test_key(base);
        ensure_test_key(&path_a).expect("create adapter subkey A");
        ensure_test_key(&path_b).expect("create adapter subkey B");

        // Pre-write B's TcpAckFrequency as a string: reading it back as a
        // REG_DWORD fails with a distinct type-mismatch error. The old code
        // aborted the whole apply here, stranding A's already-applied values;
        // the fixed apply logs, skips this value, and keeps going.
        write_string_value(HKEY_CURRENT_USER, &path_b, "TcpAckFrequency", "not-a-dword")
            .expect("write string under B");

        let backup =
            apply_at(HKEY_CURRENT_USER, base, base, true, false).expect("apply continues");

        // A's two values applied; B's TcpAckFrequency skipped (type mismatch)
        // but B's TCPNoDelay (absent before) applied and backed up.
        assert_eq!(backup.len(), 3, "backup covers every value actually modified");
        assert_eq!(
            read_dword(HKEY_CURRENT_USER, &path_a, "TcpAckFrequency")
                .expect("read")
                .unwrap(),
            1,
            "A TcpAckFrequency applied"
        );
        assert_eq!(
            read_dword(HKEY_CURRENT_USER, &path_a, "TCPNoDelay")
                .expect("read")
                .unwrap(),
            1,
            "A TCPNoDelay applied"
        );
        assert_eq!(
            read_dword(HKEY_CURRENT_USER, &path_b, "TCPNoDelay")
                .expect("read")
                .unwrap(),
            1,
            "B TCPNoDelay applied despite sibling failure"
        );

        // Reverting the returned backup restores every modified value (each was
        // absent before apply, so every revert deletes the value).
        for entry in &backup {
            revert_entry(HKEY_CURRENT_USER, entry).expect("revert entry");
        }
        assert_eq!(
            read_dword(HKEY_CURRENT_USER, &path_a, "TcpAckFrequency").expect("read"),
            None,
            "A TcpAckFrequency reverted"
        );
        assert_eq!(
            read_dword(HKEY_CURRENT_USER, &path_a, "TCPNoDelay").expect("read"),
            None,
            "A TCPNoDelay reverted"
        );
        assert_eq!(
            read_dword(HKEY_CURRENT_USER, &path_b, "TCPNoDelay").expect("read"),
            None,
            "B TCPNoDelay reverted"
        );

        remove_test_key(&path_a);
        remove_test_key(&path_b);
        remove_test_key(base);
    }
}
