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

    /// Iterate every (overlapping) match of the compiled patterns over
    /// `haystack`, each yielding a `Match` whose `pattern()` is the index of the
    /// matching pattern. `first_matching_*` takes the minimum pattern index over
    /// this iterator to recover "earliest rule in config order wins" — a
    /// plain `find_iter` reports matches in leftmost scan order, NOT pattern
    /// order, so `find_iter(...).next()` would not yield the earliest pattern.
    pub fn find_overlapping_iter<'h>(
        &self,
        haystack: &'h [u8],
    ) -> aho_corasick::FindOverlappingIter<'_, 'h> {
        self.ac.find_overlapping_iter(haystack)
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

    #[test]
    fn earliest_pattern_index_wins() {
        // A later pattern that matches earlier in the haystack must not win:
        // `min` over overlapping matches yields the earliest pattern index.
        let m = PatternMatcher::new(vec!["updater.exe".into(), "updater".into()]);
        let earliest = m
            .find_overlapping_iter(b"updater.exe")
            .map(|x| x.pattern().as_usize())
            .min();
        assert_eq!(earliest, Some(0));
    }
}
