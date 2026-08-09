//! Internationalization (i18n) foundation for aetheris.
//!
//! v2.2.1: the UI language (`Lang`, zh/en), system-language detection via
//! `GetUserDefaultUILanguage`, and the persisted UI settings in
//! `%PROGRAMDATA%\aetheris\ui.toml`. The default language is the detected
//! system language — nothing is hardcoded to Chinese.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// The two supported UI languages. `Zh` = a Chinese system UI (zh-CN, zh-TW,
/// zh-Hans, …); `En` = every other system language (English default).
#[derive(Clone, Copy, PartialEq, Eq, Debug, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Lang {
    Zh,
    En,
}

/// Detect the OS UI language and map it to a [`Lang`]: `GetUserDefaultUILanguage`
/// returns a `LANGID` whose low byte is the primary language; `LANG_CHINESE` is
/// `0x04`, so `(langid & 0xFF) == 0x04` → `Zh`, anything else → `En`.
pub fn detect_system() -> Lang {
    // SAFETY: GetUserDefaultUILanguage is a pure kernel32 call with no
    // arguments and no failure mode.
    let langid = unsafe { windows::Win32::Globalization::GetUserDefaultUILanguage() };
    if (langid & 0xFF) == 0x04 {
        Lang::Zh
    } else {
        Lang::En
    }
}

/// Default UI-settings location for the single-app layout:
/// `%PROGRAMDATA%\aetheris\ui.toml`. `PROGRAMDATA` is the machine-wide data
/// root (e.g. `C:\ProgramData`). The non-elevated UI process can read it; a
/// missing `PROGRAMDATA` falls back to `C:\ProgramData` like the config path.
pub fn ui_settings_path() -> PathBuf {
    let base = std::env::var_os("PROGRAMDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("C:\\ProgramData"));
    base.join("aetheris").join("ui.toml")
}

/// Persisted UI settings read from `ui.toml`. A missing `lang` field (or an
/// absent file) defaults to the detected system language via [`UiSettings::default`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct UiSettings {
    pub lang: Lang,
}

impl Default for UiSettings {
    fn default() -> Self {
        UiSettings {
            lang: detect_system(),
        }
    }
}

/// Load UI settings from the default [`ui_settings_path`]. A missing, corrupt,
/// or unreadable file falls back to [`UiSettings::default`] (the detected
/// system language).
pub fn load_ui_settings() -> UiSettings {
    load_at(&ui_settings_path())
}

/// Persist UI settings to the default [`ui_settings_path`], creating the
/// `aetheris` directory as needed. Returns `Err(msg)` on any I/O or
/// serialization failure.
pub fn save_ui_settings(s: &UiSettings) -> Result<(), String> {
    save_at(&ui_settings_path(), s)
}

/// Explicit-path variant of [`load_ui_settings`] — hermetic for tests.
pub fn load_at(path: &std::path::Path) -> UiSettings {
    match std::fs::read_to_string(path) {
        Ok(s) => toml::from_str::<UiSettings>(&s).unwrap_or_default(),
        Err(_) => UiSettings::default(),
    }
}

/// Explicit-path variant of [`save_ui_settings`] — hermetic for tests.
pub fn save_at(path: &std::path::Path, s: &UiSettings) -> Result<(), String> {
    let toml_str = toml::to_string(s).map_err(|e| format!("serialize ui settings: {e}"))?;
    let parent = path
        .parent()
        .ok_or_else(|| format!("ui settings path has no parent: {}", path.display()))?;
    std::fs::create_dir_all(parent)
        .map_err(|e| format!("create ui settings dir {}: {e}", parent.display()))?;
    std::fs::write(path, toml_str)
        .map_err(|e| format!("write ui settings {}: {e}", path.display()))
}
