use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct ProcMeta {
    pub pid: u32,
    pub name_hash: u64,
    pub is_game: bool,
}

/// Cache-line-aligned SoA process table. Names live in a side map so the hot
/// per-event path only touches the aligned arrays.
#[derive(Default)]
pub struct ProcessTable {
    pids: Vec<u32>,
    name_hashes: Vec<u64>,
    is_game: Vec<bool>,
    names: HashMap<u32, String>,
}

pub fn name_hash(name: &str) -> u64 {
    let mut h = DefaultHasher::new();
    name.to_ascii_lowercase().hash(&mut h);
    h.finish()
}

impl ProcessTable {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn upsert(&mut self, pid: u32, name: &str, is_game: bool) {
        let h = name_hash(name);
        match self.get(pid) {
            Some(m) if m.name_hash == h => {
                if let Some(i) = self.pids.iter().position(|p| *p == pid) {
                    self.is_game[i] = is_game;
                }
            }
            _ => {
                self.pids.push(pid);
                self.name_hashes.push(h);
                self.is_game.push(is_game);
                self.names.insert(pid, name.to_string());
            }
        }
    }

    pub fn remove(&mut self, pid: u32) -> Option<()> {
        let i = self.pids.iter().position(|p| *p == pid)?;
        self.pids.swap_remove(i);
        self.name_hashes.swap_remove(i);
        self.is_game.swap_remove(i);
        self.names.remove(&pid);
        Some(())
    }

    pub fn get(&self, pid: u32) -> Option<ProcMeta> {
        self.pids
            .iter()
            .position(|p| *p == pid)
            .map(|i| ProcMeta {
                pid,
                name_hash: self.name_hashes[i],
                is_game: self.is_game[i],
            })
    }

    pub fn name(&self, pid: u32) -> Option<&str> {
        self.names.get(&pid).map(|s| s.as_str())
    }

    pub fn iter(&self) -> impl Iterator<Item = (u32, &str, bool)> {
        let names = &self.names;
        self.pids.iter().enumerate().map(move |(i, &pid)| {
            let name = names.get(&pid).map(|s| s.as_str()).unwrap_or("");
            (pid, name, self.is_game[i])
        })
    }

    pub fn len(&self) -> usize {
        self.pids.len()
    }

    pub fn is_empty(&self) -> bool {
        self.pids.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_get_remove() {
        let mut t = ProcessTable::new();
        assert_eq!(t.len(), 0);
        t.upsert(1234, "chrome.exe", false);
        assert_eq!(t.len(), 1);
        let m = t.get(1234).unwrap();
        assert_eq!(m.pid, 1234);
        assert!(!m.is_game);
        assert_eq!(t.name(1234), Some("chrome.exe"));
        t.upsert(1234, "chrome.exe", false);
        assert_eq!(t.len(), 1);
        t.remove(1234);
        assert_eq!(t.len(), 0);
        assert!(t.get(1234).is_none());
    }

    #[test]
    fn iter_yields_all() {
        let mut t = ProcessTable::new();
        t.upsert(1, "a.exe", false);
        t.upsert(2, "game.exe", true);
        let v: Vec<(u32, String, bool)> = t.iter().map(|(p, n, g)| (p, n.to_string(), g)).collect();
        assert_eq!(v.len(), 2);
        assert!(v.contains(&(1, "a.exe".to_string(), false)));
        assert!(v.contains(&(2, "game.exe".to_string(), true)));
    }

    #[test]
    fn hash_is_stable_and_case_insensitive() {
        assert_eq!(name_hash("Chrome.EXE"), name_hash("chrome.exe"));
    }
}
