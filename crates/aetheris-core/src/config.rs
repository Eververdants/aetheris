use std::collections::BTreeSet;
use std::fmt;
use std::io;
use std::path::Path;

use serde::{Deserialize, Serialize};

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
    "aetheris-service.exe",
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
        for b in &self.background {
            if b.name.trim().is_empty() {
                return Err(ConfigError::Validation("background rule missing name".into()));
            }
            if b.suspend || b.trim_memory {
                let p = b.name.to_ascii_lowercase();
                if DEFAULT_PROTECTED.contains(&p.as_str()) {
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
            }
        }
        for r in &self.rule {
            if r.name.trim().is_empty() {
                return Err(ConfigError::Validation("rule missing name".into()));
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
