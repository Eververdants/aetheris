//! Integration tests for the v2.2.1 i18n foundation: `Lang`, system-language
//! detection, the bilingual `aetheris.toml` template, and the persisted
//! `ui.toml` settings.

use aetheris_core::config::{default_config_str, Config};
use aetheris_core::i18n::{detect_system, load_at, save_at, Lang, UiSettings};

#[test]
fn detect_system_returns_valid_lang() {
    let l = detect_system();
    assert!(l == Lang::Zh || l == Lang::En);
}

#[test]
fn lang_serde_roundtrip() {
    assert_eq!(serde_json::to_string(&Lang::Zh).unwrap(), "\"zh\"");
    assert_eq!(serde_json::from_str::<Lang>("\"en\"").unwrap(), Lang::En);
}

#[test]
fn default_config_str_parses_in_both_langs() {
    for l in [Lang::Zh, Lang::En] {
        let s = default_config_str(l);
        let cfg = Config::from_str(&s).expect("template parses");
        assert!(cfg.validate().is_ok());
    }
}

#[test]
fn ui_settings_roundtrip() {
    let path =
        std::env::temp_dir().join(format!("aetheris_ui_{}.toml", std::process::id()));
    let settings = UiSettings { lang: Lang::En };
    save_at(&path, &settings).unwrap();
    let loaded = load_at(&path);
    assert_eq!(loaded.lang, Lang::En);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn ui_settings_missing_or_corrupt_falls_back_to_system_lang() {
    // A missing file (and a corrupt one) must fall back to detect_system().
    let missing = std::env::temp_dir().join(format!(
        "aetheris_ui_missing_{}.toml",
        std::process::id()
    ));
    let loaded = load_at(&missing);
    assert_eq!(loaded.lang, detect_system());
    let _ = std::fs::remove_file(&missing);

    let corrupt = std::env::temp_dir().join(format!(
        "aetheris_ui_corrupt_{}.toml",
        std::process::id()
    ));
    std::fs::write(&corrupt, "lang = \"not_a_language\"").unwrap();
    let loaded = load_at(&corrupt);
    assert_eq!(loaded.lang, detect_system());
    let _ = std::fs::remove_file(&corrupt);
}
