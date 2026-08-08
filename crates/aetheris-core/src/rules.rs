use aho_corasick::AhoCorasick;

/// Case-insensitive substring matcher over a set of patterns, compiled once at
/// config load. Hot path is a single byte scan with zero allocation.
pub struct PatternMatcher {
    ac: AhoCorasick,
}

impl PatternMatcher {
    pub fn new(patterns: Vec<String>) -> Self {
        let lowered: Vec<String> = patterns.into_iter().map(|p| p.to_ascii_lowercase()).collect();
        let ac = AhoCorasick::builder()
            .ascii_case_insensitive(true)
            .build(&lowered)
            .expect("aho-corasick build");
        Self { ac }
    }

    pub fn matches(&self, name: &str) -> bool {
        self.ac.is_match(name.as_bytes())
    }

    pub fn is_empty(&self) -> bool {
        self.ac.patterns_len() == 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_substring_case_insensitive() {
        let m = PatternMatcher::new(vec!["steam_app_".into(), "game".into()]);
        assert!(m.matches("STEAM_APP_123.exe"));
        assert!(m.matches("MyGame.exe"));
        assert!(!m.matches("gazester.exe"));
        assert!(!m.matches("chrom.exe"));
    }

    #[test]
    fn empty_matcher_matches_nothing() {
        let m = PatternMatcher::new(vec![]);
        assert!(m.is_empty());
        assert!(!m.matches("anything.exe"));
    }

    #[test]
    fn exact_name_is_substring() {
        let m = PatternMatcher::new(vec!["browser.exe".into()]);
        assert!(m.matches("browser.exe"));
        assert!(m.matches("Browser.EXE"));
    }
}
