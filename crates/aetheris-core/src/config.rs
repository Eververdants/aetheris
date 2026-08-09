use std::collections::BTreeSet;
use std::fmt;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::i18n::Lang;

pub const DEFAULT_PROTECTED: &[&str] = &[
    "csrss.exe",
    "services.exe",
    "smss.exe",
    "wininit.exe",
    "winlogon.exe",
    "dwm.exe",
    "lsass.exe",
    "explorer.exe",
    "system",
    "aetheris.exe",
];

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PriorityClass {
    Idle,
    BelowNormal,
    Normal,
    AboveNormal,
    High,
    Realtime,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AffinitySpec {
    pub cores: Vec<u8>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct GameConfig {
    #[serde(default)]
    pub boost_on_start: bool,
    #[serde(default)]
    pub processes: Vec<String>,
    /// Opt-in: purge the Windows standby memory list once on game-mode entry so
    /// the game's working set can grow from free pages. Defaults to off — the
    /// standby list is never touched unless the user asks for it.
    #[serde(default)]
    pub purge_standby_on_boost: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BackgroundRule {
    pub name: String,
    #[serde(default)]
    pub suspend: bool,
    #[serde(default)]
    pub priority: Option<PriorityClass>,
    #[serde(default)]
    pub affinity: Option<AffinitySpec>,
    #[serde(default)]
    pub qos_cpu_quota: Option<u32>,
    #[serde(default)]
    pub trim_memory: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AlwaysRule {
    pub name: String,
    #[serde(default)]
    pub priority: Option<PriorityClass>,
    #[serde(default)]
    pub affinity: Option<AffinitySpec>,
}

/// Opt-in network QoS tweaks, applied on game-mode entry and reverted on exit.
///
/// All flags default to `false` — nothing is touched unless explicitly enabled:
/// `enabled` gates the whole feature, `nagle` toggles the per-interface
/// `TcpAckFrequency`/`TCPNoDelay` disable, and `netbios` toggles
/// `DisableNetbiosOverTcpip`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub struct NetworkConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub nagle: bool,
    #[serde(default)]
    pub netbios: bool,
}

/// Overlay launcher settings.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct OverlayConfig {
    /// Global hotkey that opens the overlay, e.g. `"ctrl+alt+o"`. Absent or
    /// empty disables the hotkey.
    #[serde(default)]
    pub hotkey: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct Config {
    #[serde(default)]
    pub game: GameConfig,
    #[serde(default)]
    pub background: Vec<BackgroundRule>,
    #[serde(default)]
    pub rule: Vec<AlwaysRule>,
    #[serde(default)]
    pub protected_extra: Vec<String>,
    #[serde(default)]
    pub network: NetworkConfig,
    #[serde(default)]
    pub overlay: OverlayConfig,
}

/// Default config location for the single-app layout:
/// `%PROGRAMDATA%\aetheris\aetheris.toml`. `PROGRAMDATA` is the machine-wide
/// data root (e.g. `C:\ProgramData`) that the elevated service can write on
/// first run even though the non-elevated UI process cannot.
pub fn default_config_path() -> PathBuf {
    let base = std::env::var_os("PROGRAMDATA")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("C:\\ProgramData"));
    base.join("aetheris").join("aetheris.toml")
}

/// English template: the repo's example `aetheris.toml`, fully commented so
/// nothing is active until the user uncomments/edits.
const EN_TEMPLATE: &str = r#"# aetheris default configuration - auto-generated on first run.
# Edit via `aetheris ui` or this file; the service reads this at startup and on
# `ReloadConfig`. Everything below is commented as a template: uncomment to
# enable.

# [game]
# boost_on_start = true
# processes = ["steam_app_*.exe", "game.exe"]

# # Opt-in: purge the Windows standby memory list once on game-mode entry so the
# # game's working set can grow from free pages. Off by default. Not reversible,
# # but harmless (the OS rebuilds its standby list as needed).
# purge_standby_on_boost = false

# # Processes throttled while a game is running. Each entry is matched
# # case-insensitively as a substring against the process image name.
# [[background]]
# name = "chrome.exe"
# suspend = false
# priority = "below_normal"
# # Real cross-process CPU cap via a Job Object (hard cap). `qos_cpu_quota` is a
# # percentage of the machine's total CPU capacity (0.01% units internally,
# # CpuRate = quota * 100). Only applied to processes NOT already in a job; a
# # process that is already job-bound degrades to no-cap with a warn (priority /
# # affinity still apply). The cap clears when the game exits or the service
# # stops. Note: browsers and many apps are already in a job, so QoS often won't
# # bind on them - it works cleanly for processes aetheris launches fresh.
# qos_cpu_quota = 50

# [[background]]
# name = "msedge.exe"
# suspend = false
# priority = "below_normal"

# # Memory trim is explicit opt-in and only safe for non-critical apps.
# [[background]]
# name = "spotify.exe"
# trim_memory = true

# # Always-on rules (any mode).
# [[rule]]
# name = "updater.exe"
# priority = "idle"

# # Extra protected processes (defaults can never be removed).
# protected_extra = []

# # Network QoS tweaks are explicit opt-in (all flags default to false) and are
# # applied on game-mode entry, then reverted on exit. They write registry values
# # under HKLM, so the service must be elevated to use them.
# [network]
# enabled = true
# nagle = true
# netbios = false
"#;

/// Chinese template: the same example `aetheris.toml` with translated comments.
/// The TOML structure and values are byte-identical to [`EN_TEMPLATE`] — only
/// the comment text differs.
const ZH_TEMPLATE: &str = r#"# aetheris 默认配置 - 首次运行时自动生成。
# 可通过 `aetheris ui` 或本文件编辑;服务在启动及收到
# `ReloadConfig` 时读取。以下全部以注释形式作为模板:取消注释以启用。

# [game]
# boost_on_start = true
# processes = ["steam_app_*.exe", "game.exe"]

# # 可选:进入游戏模式时清理一次 Windows 待机内存列表,以便游戏的
# # 工作集可以使用空闲内存页。默认关闭。不可逆,但无害
# # (系统会在需要时重建待机列表)。
# purge_standby_on_boost = false

# # 游戏运行时被节流的进程。每个条目按进程映像名,不区分大小写,
# # 以子串方式匹配。
# [[background]]
# name = "chrome.exe"
# suspend = false
# priority = "below_normal"
# # 通过 Job Object 实现真正的跨进程 CPU 上限(硬上限)。`qos_cpu_quota`
# # 是机器总 CPU 容量的百分比(内部以 0.01% 为单位,CpuRate = quota * 100)。
# # 仅应用于尚不在任何 Job 中的进程;已在 Job 中的进程会降级为无上限并
# # 发出警告(优先级 / 亲和性仍然生效)。上限在游戏退出或服务停止时清除。
# # 注意:浏览器和许多应用已经在 Job 中,因此 QoS 往往无法绑定它们 -
# # 它对 aetheris 新启动的进程工作良好。
# qos_cpu_quota = 50

# [[background]]
# name = "msedge.exe"
# suspend = false
# priority = "below_normal"

# # 内存清理是显式选择,只对非关键应用安全。
# [[background]]
# name = "spotify.exe"
# trim_memory = true

# # 始终生效的规则(任意模式)。
# [[rule]]
# name = "updater.exe"
# priority = "idle"

# # 额外保护进程(默认项永远无法移除)。
# protected_extra = []

# # 网络 QoS 调整是显式选择(所有标志默认均为 false),在进入游戏模式时
# # 应用,退出时还原。它们写入 HKLM 下的注册表值,因此服务必须以管理员
# # 权限运行才能使用。
# [network]
# enabled = true
# nagle = true
# netbios = false
"#;

/// First-run template written before [`Config::load`]: the repo's example
/// `aetheris.toml` content in the given language, fully commented so nothing
/// is active until the user uncomments/edits. A no-op template parses to a
/// valid (default) config in both languages.
pub fn default_config_str(lang: Lang) -> String {
    match lang {
        Lang::En => EN_TEMPLATE.to_string(),
        Lang::Zh => ZH_TEMPLATE.to_string(),
    }
}

#[derive(Debug)]
pub enum ConfigError {
    Io(io::Error),
    Toml(toml::de::Error),
    Validation(String),
}

impl fmt::Display for ConfigError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConfigError::Io(e) => write!(f, "io: {e}"),
            ConfigError::Toml(e) => write!(f, "toml: {e}"),
            ConfigError::Validation(m) => write!(f, "validation: {m}"),
        }
    }
}

impl std::error::Error for ConfigError {}

impl Config {
    pub fn load(path: &Path) -> Result<Self, ConfigError> {
        let bytes = std::fs::read(path).map_err(ConfigError::Io)?;
        let s = String::from_utf8(bytes).map_err(|e| ConfigError::Validation(e.to_string()))?;
        Self::from_str(&s)
    }

    pub fn from_str(s: &str) -> Result<Self, ConfigError> {
        let cfg: Config = toml::from_str(s).map_err(ConfigError::Toml)?;
        cfg.validate()?;
        Ok(cfg)
    }

    pub fn validate(&self) -> Result<(), ConfigError> {
        let protected = self.protected_set();
        for b in &self.background {
            if b.name.trim().is_empty() {
                return Err(ConfigError::Validation("background rule missing name".into()));
            }
            if b.suspend || b.trim_memory {
                let p = b.name.to_ascii_lowercase();
                if protected.contains(&p) {
                    return Err(ConfigError::Validation(format!(
                        "rule '{}' targets a protected process with suspend/trim",
                        b.name
                    )));
                }
            }
            if let Some(q) = b.qos_cpu_quota {
                if q == 0 || q > 100 {
                    return Err(ConfigError::Validation(format!(
                        "qos_cpu_quota for '{}' must be 1..=100, got {}",
                        b.name, q
                    )));
                }
            }
            if let Some(a) = &b.affinity {
                if a.cores.is_empty() {
                    return Err(ConfigError::Validation(format!(
                        "affinity for '{}' has no cores",
                        b.name
                    )));
                }
                if let Some(c) = a.cores.iter().copied().find(|c| *c >= 64) {
                    // `mask_from_cores` builds the mask as a u64 (`1u64 << c`);
                    // any core index >= 64 would panic (and the release profile
                    // aborts on panic, crashing the service).
                    return Err(ConfigError::Validation(format!(
                        "affinity for '{}' has core index {c} >= 64 (max 63)",
                        b.name
                    )));
                }
                // DEV (warn, non-fatal): cores beyond the host's logical CPU
                // count can't be pinned; flag it but do NOT reject the config.
                let n = crate::actions::logical_cpu_count();
                if n > 0 && a.cores.iter().any(|&c| c as u32 >= n) {
                    crate::log::warn(format!(
                        "rule '{}' affinity cores exceed logical CPU count ({}) on this host",
                        b.name, n
                    ));
                }
            }
        }
        for r in &self.rule {
            if r.name.trim().is_empty() {
                return Err(ConfigError::Validation("rule missing name".into()));
            }
            if let Some(a) = &r.affinity {
                if let Some(c) = a.cores.iter().copied().find(|c| *c >= 64) {
                    return Err(ConfigError::Validation(format!(
                        "affinity for '{}' has core index {c} >= 64 (max 63)",
                        r.name
                    )));
                }
                // DEV (warn, non-fatal): same host-count check as background rules.
                let n = crate::actions::logical_cpu_count();
                if n > 0 && a.cores.iter().any(|&c| c as u32 >= n) {
                    crate::log::warn(format!(
                        "rule '{}' affinity cores exceed logical CPU count ({}) on this host",
                        r.name, n
                    ));
                }
            }
        }
        Ok(())
    }

    pub fn protected_set(&self) -> BTreeSet<String> {
        let mut s: BTreeSet<String> = DEFAULT_PROTECTED.iter().map(|p| p.to_ascii_lowercase()).collect();
        for p in &self.protected_extra {
            s.insert(p.to_ascii_lowercase());
        }
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_valid_config() {
        let s = r#"
[game]
boost_on_start = true
processes = ["steam_app_*.exe"]

[[background]]
name = "browser.exe"
suspend = true
priority = "below_normal"
trim_memory = true

[[background]]
name = "updater.exe"
qos_cpu_quota = 30

[[rule]]
name = "foo.exe"
priority = "idle"
"#;
        let cfg = Config::from_str(s).expect("parse");
        cfg.validate().expect("valid");
        assert!(cfg.game.boost_on_start);
        assert_eq!(cfg.background.len(), 2);
        assert!(cfg.background[0].suspend);
        assert_eq!(cfg.background[0].priority, Some(PriorityClass::BelowNormal));
        assert_eq!(cfg.background[1].qos_cpu_quota, Some(30));
    }

    #[test]
    fn reject_suspend_on_protected() {
        let s = r#"
[game]
processes = ["game.exe"]

[[background]]
name = "csrss.exe"
suspend = true
"#;
        assert!(Config::from_str(s).is_err(), "must reject protected+action");
    }

    #[test]
    fn reject_suspend_on_protected_extra() {
        let s = r#"
protected_extra = ["MyCritical.exe"]

[game]
processes = ["game.exe"]

[[background]]
name = "MyCritical.exe"
suspend = true
"#;
        assert!(Config::from_str(s).is_err(), "must reject suspend on protected_extra");
    }

    #[test]
    fn reject_empty_affinity_cores() {
        let s = r#"
[game]
processes = []

[[background]]
name = "x.exe"
affinity = { cores = [] }
"#;
        assert!(Config::from_str(s).is_err());
    }

    #[test]
    fn reject_affinity_core_index_ge_64() {
        // core index 64 would make mask_from_cores shift `1u64 << 64`, which
        // panics in release (abort). It must be rejected at config load.
        let s = r#"
[game]
processes = []

[[background]]
name = "x.exe"
affinity = { cores = [0, 64] }
"#;
        assert!(Config::from_str(s).is_err());

        // An always-rule affinity is validated too.
        let s2 = r#"
[game]
processes = []

[[rule]]
name = "y.exe"
affinity = { cores = [64] }
"#;
        assert!(Config::from_str(s2).is_err());

        // Boundary core 63 is fine.
        let s3 = r#"
[game]
processes = []

[[background]]
name = "z.exe"
affinity = { cores = [63] }
"#;
        Config::from_str(s3).expect("core 63 is the maximum valid index");
    }

    #[test]
    fn reject_bad_priority_value() {
        let s = r#"
[game]
processes = []

[[rule]]
name = "x.exe"
priority = "turbo"
"#;
        assert!(Config::from_str(s).is_err());
    }

    #[test]
    fn reject_qos_quota_out_of_range() {
        let s = r#"
[game]
processes = []

[[background]]
name = "x.exe"
qos_cpu_quota = 150
"#;
        assert!(Config::from_str(s).is_err());
    }

    #[test]
    fn affinity_cores_within_logical_cpu_count_validate() {
        // Cores [0, 1] are within any plausible host logical CPU count; must
        // parse and validate cleanly (the host-count check is warn-only).
        let s = r#"
[game]
processes = []

[[background]]
name = "x.exe"
affinity = { cores = [0, 1] }
"#;
        Config::from_str(s).expect("cores 0..2 within host logical CPU count");
    }

    #[test]
    fn affinity_core_beyond_logical_cpu_count_warns_not_panics() {
        // Core 63 is a valid index (< 64) but exceeds any plausible host count;
        // validation must warn (non-fatal), not panic or reject.
        let s = r#"
[game]
processes = []

[[background]]
name = "x.exe"
affinity = { cores = [63] }
"#;
        Config::from_str(s).expect("high core index warns but still validates");

        let s2 = r#"
[game]
processes = []

[[rule]]
name = "y.exe"
affinity = { cores = [63] }
"#;
        Config::from_str(s2).expect("always-rule high core index warns but still validates");
    }

    #[test]
    fn network_defaults_off() {
        // `network` is opt-in: an absent `[network]` section must keep every
        // flag off (never touch the registry unless the user asked for it).
        let s = "[game]\nprocesses = []\n";
        let cfg = Config::from_str(s).expect("parse without [network]");
        assert!(!cfg.network.enabled, "enabled must default off");
        assert!(!cfg.network.nagle, "nagle must default off");
        assert!(!cfg.network.netbios, "netbios must default off");

        // `Config::default()` behaves identically.
        let d = Config::default();
        assert!(!d.network.enabled);
        assert!(!d.network.nagle);
        assert!(!d.network.netbios);
    }

    #[test]
    fn network_section_parses() {
        let s = r#"
[network]
enabled = true
nagle = true
netbios = true

[game]
processes = []
"#;
        let cfg = Config::from_str(s).expect("parse with [network]");
        assert!(cfg.network.enabled);
        assert!(cfg.network.nagle);
        assert!(cfg.network.netbios);
    }

    #[test]
    fn game_config_defaults_purge_off() {
        // Standby purge is opt-in: an absent flag must keep it off (never touch
        // the system standby list unless the user asked for it).
        let cfg = Config::from_str("[game]\nprocesses=[]\n").unwrap();
        assert!(!cfg.game.purge_standby_on_boost);
    }

    #[test]
    fn overlay_hotkey_config_parse() {
        // `[overlay] hotkey` present → parsed; absent → None (disabled).
        let c = Config::from_str("[overlay]\nhotkey = \"ctrl+alt+o\"\n").unwrap();
        assert_eq!(c.overlay.hotkey.as_deref(), Some("ctrl+alt+o"));
        let c2 = Config::from_str("[game]\nprocesses=[]\n").unwrap();
        assert!(c2.overlay.hotkey.is_none());
    }

    #[test]
    fn protected_set_includes_defaults_and_extra() {
        let s = r#"
protected_extra = ["MyTool.exe"]

[game]
processes = []
"#;
        let cfg = Config::from_str(s).expect("parse");
        let set = cfg.protected_set();
        assert!(set.contains("csrss.exe"));
        assert!(set.contains("mytool.exe"));
    }
}
