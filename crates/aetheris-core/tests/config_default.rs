//! Integration tests for the first-run default-config helpers.
//!
//! v2.2 Task 3: the service auto-generates a config at
//! `%PROGRAMDATA%\aetheris\aetheris.toml` on first run. These tests pin the two
//! helpers that produce the path and the template content.

use aetheris_core::config::Config;

#[test]
fn default_config_path_is_programdata() {
    let p = aetheris_core::config::default_config_path();
    assert!(p.to_string_lossy().contains("ProgramData"));
    assert!(p.ends_with("aetheris.toml"));
}

#[test]
fn default_config_str_is_valid_toml() {
    let s = aetheris_core::config::default_config_str();
    let cfg = Config::from_str(&s).expect("default config parses");
    assert!(cfg.validate().is_ok());
}
