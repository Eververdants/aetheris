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

use windows::core::{PCWSTR, PWSTR};
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_NO_MORE_ITEMS, ERROR_SUCCESS};
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
#[derive(Debug, Clone, PartialEq, Eq)]
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
/// so [`revert`] can reverse every change. Errors if Nagle is requested but no
/// interface adapters are enumerated.
pub fn apply(nagle: bool, netbios: bool) -> Result<Vec<BackupEntry>, String> {
    let mut backup = Vec::new();
    if nagle {
        let adapters = enum_subkeys(HKEY_LOCAL_MACHINE, TCPIP_INTERFACES_KEY)?;
        if adapters.is_empty() {
            return Err("no TCP/IP interface adapters found under Tcpip\\Parameters\\Interfaces".into());
        }
        for adapter in adapters {
            let path = format!("{TCPIP_INTERFACES_KEY}\\{adapter}");
            backup.push(backup_and_set(HKEY_LOCAL_MACHINE, &path, "TcpAckFrequency", 1)?);
            backup.push(backup_and_set(HKEY_LOCAL_MACHINE, &path, "TCPNoDelay", 1)?);
        }
    }
    if netbios {
        backup.push(backup_and_set(
            HKEY_LOCAL_MACHINE,
            NETBT_PARAMETERS_KEY,
            "DisableNetbiosOverTcpip",
            2,
        )?);
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

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegDeleteKeyW, HKEY_CURRENT_USER, KEY_ALL_ACCESS,
        REG_OPTION_NON_VOLATILE,
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
}
