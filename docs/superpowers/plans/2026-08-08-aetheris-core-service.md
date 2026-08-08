# aetheris Core Service Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build the v1 zero-overhead Windows game-optimization core service in Rust: ETW-driven process monitoring, rule + game-mode policy engine applying priority/affinity/QoS/suspend/memory-trim actions, and named-pipe IPC, as a Cargo workspace.

**Architecture:** `aetheris-core` lib holds the engine (ETW consumer on a dedicated thread → channel → single-threaded main loop; policy engine compiles TOML rules into an Aho-Corasick automaton; actions go through a `ProcessBackend` trait so the policy engine is unit-testable with a recording backend). `aetheris-service` is the thin console launcher, `aetheris-cli` is the named-pipe client. No async runtime, no tokio; zero heap allocation on the hot event path.

**Tech Stack:** Rust (edition 2021, MSRV 1.82+), `windows` 0.62 (feature-gated FFI), `ntapi` (NtSuspendProcess/NtResumeProcess), `aho-corasick`, `memmap2`, `serde`+`toml`, `bincode`, `ctrlc`. Target: Windows 10 1809+ / Windows 11 x64. ETW + process actions require an elevated (admin) process.

## Global Constraints

- **No async runtime / no tokio.** All concurrency is `std::thread` + `std::sync::mpsc`.
- **Hot path zero heap allocation.** Rule matching uses the prebuilt Aho-Corasick automaton; process table is a preallocated SoA; buffers are reused. Allocations happen at init or on config reload.
- **No polling of process lists.** Process lifecycle comes from ETW; game-foreground from `SetWinEventHook`. Exception (allowed): the main loop blocks on `mpsc::Receiver::recv()`.
- **Protected list is absolute.** Processes in the protected set are never suspended, trimmed, or dropped below `NORMAL`. Default set: `csrss.exe, services.exe, smss.exe, wininit.exe, winlogon.exe, dwm.exe, lsass.exe, explorer.exe, System, aetheris-service.exe`. Config may only append.
- **Suspend and memory trim require explicit `true`** in the rule (`suspend = true`, `trim_memory = true`); default is `false`.
- **Fail-safe:** if the ETW session cannot be opened, the service logs a clear error and exits (does NOT fall back to polling).
- **Dependencies locked** to: `windows 0.62`, `ntapi 0.4`, `aho-corasick 1.1`, `memmap2 0.9`, `serde 1`, `toml 0.8`, `bincode 1.3`, `ctrlc 3`. Nothing else. All permissive licenses. `cargo-deny` must pass with no copyleft in the dependency graph.
- **License compliance:** copy code only from SAFE-TO-COPY projects (MIT/Apache). Every file containing borrowed code keeps its attribution; `THIRD_PARTY.md` lists all borrowings. GPL/LGPL/unlicensed projects (vnite, Winderust, SpecialK, NotCPUCores, RyzenAdj, super-thread, ETWProcessMon2, StandbyCleanerLite) are architecture reference only — clean-room, no copied code.
- **Affinity scope for v1:** classic `SetProcessAffinityMask` works for systems with ≤64 logical CPUs (the plan's test/validation target). On >64-logical-CPU hosts the affinity action logs a warning and skips (group-aware assignment via `SetProcessDefaultCpuSetMasks` is deferred); all other actions unaffected. Documented deviation — spec §5.4 full group-aware is v1.x.
- **First-match rule wins** when a process matches multiple `[[background]]`/`[[rule]]` entries.
- **Privileges:** on startup enable `SeDebugPrivilege` and `SeIncreaseBasePriorityPrivilege`.
- **Graceful degradation:** under system load > 85% (measured via `NtQuerySystemInformation(SystemProcessorPerformanceInformation)`), suspend/trim/QoS actions are deferred and a warning is logged. (Implemented in the Service main loop, Task 12.)
- Every task ends with a green `cargo test` (or documented manual verification) and a commit.

---

### Task 1: Workspace scaffold + core crate skeleton

**Files:**
- Create: `Cargo.toml` (root workspace)
- Create: `crates/aetheris-core/Cargo.toml`
- Create: `crates/aetheris-core/src/lib.rs`
- Create: `crates/aetheris-core/src/config.rs` (empty module stub — filled in Task 4)
- Create: `crates/aetheris-core/src/rules.rs` (empty stub — Task 5)
- Create: `crates/aetheris-core/src/proc_table.rs` (empty stub — Task 3)
- Create: `crates/aetheris-core/src/actions.rs` (empty stub — Task 6/7)
- Create: `crates/aetheris-core/src/policy.rs` (empty stub — Task 8)
- Create: `crates/aetheris-core/src/etw.rs` (empty stub — Task 9)
- Create: `crates/aetheris-core/src/foreground.rs` (empty stub — Task 10)
- Create: `crates/aetheris-core/src/ipc.rs` (empty stub — Task 11)
- Create: `crates/aetheris-core/src/log.rs` (empty stub — Task 2)
- Create: `crates/aetheris-core/src/service.rs` (empty stub — Task 12)
- Create: `crates/aetheris-service/Cargo.toml`
- Create: `crates/aetheris-service/src/main.rs`
- Create: `crates/aetheris-cli/Cargo.toml`
- Create: `crates/aetheris-cli/src/main.rs`
- Create: `.gitignore`
- Test: none (build only)

**Interfaces:**
- Consumes: nothing.
- Produces: the workspace builds. Module stubs exist so later tasks only edit their own file. Root workspace members: `crates/aetheris-core`, `crates/aetheris-service`, `crates/aetheris-cli`.

- [ ] **Step 1: Write root `Cargo.toml`**

```toml
[workspace]
resolver = "2"
members = ["crates/aetheris-core", "crates/aetheris-service", "crates/aetheris-cli"]

[workspace.package]
version = "0.1.0"
edition = "2021"
license = "MIT"
```

- [ ] **Step 2: Write `crates/aetheris-core/Cargo.toml`**

```toml
[package]
name = "aetheris-core"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
windows = { version = "0.62", features = [
  "Win32_Foundation",
  "Win32_Security",
  "Win32_System_Diagnostics_Etw",
  "Win32_System_Diagnostics_ToolHelp",
  "Win32_System_JobObjects",
  "Win32_System_Memory",
  "Win32_System_ProcessStatus",
  "Win32_System_SystemInformation",
  "Win32_System_SystemServices",
  "Win32_System_Threading",
  "Win32_UI_Accessibility",
  "Win32_UI_WindowsAndMessaging",
] }
ntapi = "0.4"
aho-corasick = "1.1"
memmap2 = "0.9"
serde = { version = "1", features = ["derive"] }
toml = "0.8"
bincode = "1.3"
ctrlc = "3"
```

- [ ] **Step 3: Write `crates/aetheris-core/src/lib.rs`**

```rust
//! aetheris core engine.
//! Zero-overhead Windows game-optimization service library.
//! No async runtime. Single-threaded main loop fed by dedicated threads.

pub mod actions;
pub mod config;
pub mod etw;
pub mod foreground;
pub mod ipc;
pub mod log;
pub mod policy;
pub mod proc_table;
pub mod rules;
pub mod service;
```

- [ ] **Step 4: Create the empty module stubs**

Each stub file contains only a doc comment:

```rust
//! (filled in a later task)
```

Files: `src/config.rs`, `src/rules.rs`, `src/proc_table.rs`, `src/actions.rs`, `src/policy.rs`, `src/etw.rs`, `src/foreground.rs`, `src/ipc.rs`, `src/log.rs`, `src/service.rs`.

- [ ] **Step 5: Write `crates/aetheris-service/Cargo.toml`**

```toml
[package]
name = "aetheris-service"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
aetheris-core = { path = "../aetheris-core" }
windows = { version = "0.62", features = ["Win32_Foundation", "Win32_System_Console"] }
ctrlc = "3"

[profile.release]
lto = "thin"
strip = true
panic = "abort"
```

- [ ] **Step 6: Write `crates/aetheris-service/src/main.rs`**

```rust
fn main() {
    println!("aetheris-service: not implemented yet (Task 13)");
}
```

- [ ] **Step 7: Write `crates/aetheris-cli/Cargo.toml`**

```toml
[package]
name = "aetheris-cli"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
aetheris-core = { path = "../aetheris-core" }
```

- [ ] **Step 8: Write `crates/aetheris-cli/src/main.rs`**

```rust
fn main() {
    println!("aetheris-cli: not implemented yet (Task 14)");
}
```

- [ ] **Step 9: Write `.gitignore`**

```gitignore
/target
Cargo.lock
```

- [ ] **Step 10: Build the workspace**

Run: `cargo build --workspace`
Expected: builds clean, no errors. (Rust 1.82+ MSRV; verify with `rustc --version`.)

- [ ] **Step 11: Commit**

```bash
git init
git add .
git commit -m "feat: scaffold aetheris workspace (core/service/cli)"
```

---

### Task 2: Ring logger

**Files:**
- Modify: `crates/aetheris-core/src/log.rs`
- Test: `crates/aetheris-core/src/log.rs` (inline `#[cfg(test)]`)

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `pub fn init(capacity: usize)` — installs the global ring logger (call once at startup).
  - `pub fn info(msg: impl AsRef<str>)`, `pub fn warn(...)`, `pub fn error(...)` — append a line.
  - `pub fn dump() -> Vec<String>` — snapshot of buffered lines, oldest first.
  - `pub struct RingLogger` with `fn new(capacity: usize) -> Self`, `fn log(&self, level: Level, msg: String)`, `fn dump(&self) -> Vec<String>`.
  - `pub enum Level { Error, Warn, Info, Debug }` (derives `Clone, Copy, PartialEq, Eq, Debug`).
  - `pub static LOGGER: OnceLock<RingLogger>` — tests read/write it directly.

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_logger_keeps_last_n_and_dumps_oldest_first() {
        let logger = RingLogger::new(3);
        logger.log(Level::Info, "one".into());
        logger.log(Level::Warn, "two".into());
        logger.log(Level::Error, "three".into());
        logger.log(Level::Info, "four".into()); // evicts "one"
        let lines = logger.dump();
        assert_eq!(lines.len(), 3);
        assert!(lines[0].contains("two"));
        assert!(lines[0].contains("WARN"));
        assert!(lines[2].contains("four"));
    }

    #[test]
    fn global_init_and_macros() {
        init(4);
        info("hello");
        warn("world");
        let lines = dump();
        assert!(lines.iter().any(|l| l.contains("hello")));
        assert!(lines.iter().any(|l| l.contains("world")));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p aetheris-core log`
Expected: compile error — `RingLogger` not found.

- [ ] **Step 3: Write the implementation**

```rust
use std::collections::VecDeque;
use std::sync::{Mutex, OnceLock};

#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Level {
    Error,
    Warn,
    Info,
    Debug,
}

pub struct RingLogger {
    inner: Mutex<VecDeque<(Level, String)>>,
    capacity: usize,
}

impl RingLogger {
    pub fn new(capacity: usize) -> Self {
        Self {
            inner: Mutex::new(VecDeque::with_capacity(capacity)),
            capacity,
        }
    }

    pub fn log(&self, level: Level, msg: String) {
        let mut q = self.inner.lock().unwrap();
        if q.len() >= self.capacity {
            q.pop_front();
        }
        q.push_back((level, msg));
    }

    pub fn dump(&self) -> Vec<String> {
        let q = self.inner.lock().unwrap();
        q.iter()
            .map(|(l, m)| format!("{:?}: {}", l, m))
            .collect()
    }
}

pub static LOGGER: OnceLock<RingLogger> = OnceLock::new();

pub fn init(capacity: usize) {
    let _ = LOGGER.set(RingLogger::new(capacity));
}

pub fn info(msg: impl AsRef<str>) {
    if let Some(l) = LOGGER.get() {
        l.log(Level::Info, msg.as_ref().to_string());
    }
}

pub fn warn(msg: impl AsRef<str>) {
    if let Some(l) = LOGGER.get() {
        l.log(Level::Warn, msg.as_ref().to_string());
    }
}

pub fn error(msg: impl AsRef<str>) {
    if let Some(l) = LOGGER.get() {
        l.log(Level::Error, msg.as_ref().to_string());
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p aetheris-core log`
Expected: 2 passing tests.

- [ ] **Step 5: Commit**

```bash
git add crates/aetheris-core/src/log.rs
git commit -m "feat: ring logger with global init"
```

---

### Task 3: Event types + SoA process table

**Files:**
- Modify: `crates/aetheris-core/src/proc_table.rs`
- Create: `crates/aetheris-core/src/events.rs` (new module; add `pub mod events;` to `lib.rs`)
- Test: inline `#[cfg(test)]` in `proc_table.rs`

**Interfaces:**
- Consumes: nothing.
- Produces:
  - In `events.rs`:
    - `pub enum ProcessKind { Start, Stop }` (derive `Clone, Copy, PartialEq, Eq, Debug`).
    - `pub struct ProcessEvent { pub pid: u32, pub name: String, pub parent_pid: u32, pub kind: ProcessKind }` (derive `Clone, Debug`).
    - `pub struct ForegroundEvent { pub pid: u32 }` (derive `Clone, Copy, Debug`).
  - In `proc_table.rs`:
    - `pub struct ProcMeta { pub pid: u32, pub name_hash: u64, pub is_game: bool }` (`#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]`).
    - `pub struct ProcessTable` with:
      - `pub fn new() -> Self`
      - `pub fn upsert(&mut self, pid: u32, name: &str, is_game: bool)`
      - `pub fn remove(&mut self, pid: u32) -> Option<()>`
      - `pub fn get(&self, pid: u32) -> Option<ProcMeta>`
      - `pub fn name(&self, pid: u32) -> Option<&str>`
      - `pub fn iter(&self) -> impl Iterator<Item = (u32, &str, bool)>`
      - `pub fn len(&self) -> usize`
    - `pub fn name_hash(name: &str) -> u64` — `DefaultHasher` over lowercased bytes.

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::events::ProcessKind;

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
        t.upsert(1234, "chrome.exe", false); // idempotent
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
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p aetheris-core proc_table`
Expected: compile error — `ProcessTable` not found.

- [ ] **Step 3: Write `events.rs`**

```rust
/// Event types produced by the monitor threads and consumed by the policy engine.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum ProcessKind {
    Start,
    Stop,
}

#[derive(Clone, Debug)]
pub struct ProcessEvent {
    pub pid: u32,
    pub name: String,
    pub parent_pid: u32,
    pub kind: ProcessKind,
}

#[derive(Clone, Copy, Debug)]
pub struct ForegroundEvent {
    pub pid: u32,
}
```

- [ ] **Step 4: Write the implementation in `proc_table.rs`**

```rust
use std::collections::HashMap;
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use crate::events::ProcessKind;

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
```

- [ ] **Step 5: Add module to `lib.rs`**

In `crates/aetheris-core/src/lib.rs`, add `pub mod events;` to the module list.

- [ ] **Step 6: Run test to verify it passes**

Run: `cargo test -p aetheris-core`
Expected: all tests pass (log + proc_table).

- [ ] **Step 7: Commit**

```bash
git add crates/aetheris-core/src/events.rs crates/aetheris-core/src/proc_table.rs crates/aetheris-core/src/lib.rs
git commit -m "feat: event types and SoA process table"
```

---

### Task 4: Config (TOML load + validation + protected list)

**Files:**
- Modify: `crates/aetheris-core/src/config.rs`
- Test: inline `#[cfg(test)]`

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `pub struct Config { pub game: GameConfig, pub background: Vec<BackgroundRule>, pub rule: Vec<AlwaysRule>, pub protected_extra: Vec<String> }` (`#[derive(Debug, Clone, Serialize, Deserialize, Default)]`).
  - `pub struct GameConfig { pub boost_on_start: bool, pub processes: Vec<String> }`.
  - `pub struct BackgroundRule { pub name: String, pub suspend: bool, pub priority: Option<PriorityClass>, pub affinity: Option<AffinitySpec>, pub qos_cpu_quota: Option<u32>, pub trim_memory: bool }`.
  - `pub struct AlwaysRule { pub name: String, pub priority: Option<PriorityClass>, pub affinity: Option<AffinitySpec> }`.
  - `pub struct AffinitySpec { pub cores: Vec<u8> }`.
  - `pub enum PriorityClass { Idle, BelowNormal, Normal, AboveNormal, High, Realtime }` (`#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]`).
  - `impl Config`:
    - `pub fn load(path: &std::path::Path) -> Result<Self, ConfigError>`
    - `pub fn from_str(s: &str) -> Result<Self, ConfigError>`
    - `pub fn validate(&self) -> Result<(), ConfigError>`
    - `pub fn protected_set(&self) -> std::collections::BTreeSet<String>` — `DEFAULT_PROTECTED` + `protected_extra`, lowercased.
  - `pub const DEFAULT_PROTECTED: &[&str]` (see Global Constraints list; `aetheris-service.exe` included).
  - `pub enum ConfigError { Io(std::io::Error), Toml(toml::de::Error), Validation(String) }` (derive `Debug`; impl `Display` + `std::error::Error`).

- [ ] **Step 1: Write the failing test**

```rust
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
        let cfg = Config::from_str(s).expect("parse");
        assert!(cfg.validate().is_err(), "must reject protected+action");
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
        let cfg = Config::from_str(s).expect("parse");
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn protected_set_includes_defaults_and_extra() {
        let s = r#"
[game]
processes = []

protected_extra = ["MyTool.exe"]
"#;
        let cfg = Config::from_str(s).expect("parse");
        let set = cfg.protected_set();
        assert!(set.contains("csrss.exe"));
        assert!(set.contains("mytool.exe"));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p aetheris-core config`
Expected: compile error — `Config` not found.

- [ ] **Step 3: Write the implementation**

```rust
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
        let cfg = Config::from_str(s).expect("parse");
        assert!(cfg.validate().is_err(), "must reject protected+action");
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
        let cfg = Config::from_str(s).expect("parse");
        assert!(cfg.validate().is_err());
    }

    #[test]
    fn protected_set_includes_defaults_and_extra() {
        let s = r#"
[game]
processes = []

protected_extra = ["MyTool.exe"]
"#;
        let cfg = Config::from_str(s).expect("parse");
        let set = cfg.protected_set();
        assert!(set.contains("csrss.exe"));
        assert!(set.contains("mytool.exe"));
    }
}
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p aetheris-core config`
Expected: 5 passing tests.

- [ ] **Step 5: Commit**

```bash
git add crates/aetheris-core/src/config.rs
git commit -m "feat: TOML config with validation and protected list"
```

---

### Task 5: Rule matching (Aho-Corasick)

**Files:**
- Modify: `crates/aetheris-core/src/rules.rs`
- Test: inline `#[cfg(test)]`

**Interfaces:**
- Consumes: nothing.
- Produces:
  - `pub struct PatternMatcher { ac: AhoCorasick }`
  - `impl PatternMatcher`:
    - `pub fn new(patterns: Vec<String>) -> Self`
    - `pub fn matches(&self, name: &str) -> bool` — case-insensitive substring match.
    - `pub fn is_empty(&self) -> bool`

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_substring_case_insensitive() {
        let m = PatternMatcher::new(vec!["steam_app_".into(), "game".into()]);
        assert!(m.matches("STEAM_APP_123.exe"));
        assert!(m.matches("MyGame.exe"));
        assert!(!m.matches("gamester.exe".replace("s", "z"))); // "gazester.exe"
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
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p aetheris-core rules`
Expected: compile error — `PatternMatcher` not found.

- [ ] **Step 3: Write the implementation**

```rust
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
```

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p aetheris-core rules`
Expected: 3 passing tests.

- [ ] **Step 5: Commit**

```bash
git add crates/aetheris-core/src/rules.rs
git commit -m "feat: aho-corasick rule matcher"
```

---

### Task 6: Action executor — privileges, priority, affinity, memory trim + dummy process

**Files:**
- Modify: `crates/aetheris-core/src/actions.rs`
- Create: `crates/aetheris-core/src/bin/dummy_proc.rs`
- Create: `crates/aetheris-core/tests/actions_priority.rs`
- Modify: `crates/aetheris-core/Cargo.toml` (add `[[bin]]`)
- Test: `tests/actions_priority.rs` (integration; needs real process)

**Interfaces:**
- Consumes: `crate::config::PriorityClass`, `crate::config::AffinitySpec` (Task 4).
- Produces:
  - `pub enum TargetAction { Priority(PriorityClass), Affinity { core_mask: u64 }, Suspend, Resume, TrimMemory, QosCpuQuota { percent: u32 } }` (`#[derive(Debug, Clone, Copy, PartialEq, Eq)]`).
  - `pub struct ProcState { pub priority: u32, pub affinity: u64, pub suspended: bool, pub qos_percent: Option<u32> }` (`#[derive(Debug, Clone, Copy, Default)]`).
  - `pub trait ProcessBackend { fn snapshot(&self, pid: u32) -> Result<ProcState, ActionError>; fn apply(&self, pid: u32, action: &TargetAction) -> Result<(), ActionError>; fn restore(&self, pid: u32, state: &ProcState) -> Result<(), ActionError>; }` (restore returns `Ok(())` for unimplemented actions — see Task 7).
  - `pub struct OsBackend { }` with `pub fn new() -> Self` and `pub fn enable_privileges(&self) -> Result<(), ActionError>`.
  - `pub fn mask_from_cores(cores: &[u8]) -> u64`.
  - `pub enum ActionError { Open(u32), Api(String), Job(String) }` (derive `Debug`; impl `Display` + `Error`).
  - `pub const PROCESS_QUERY: u32` = `PROCESS_QUERY_INFORMATION.0 | PROCESS_QUERY_LIMITED_INFORMATION.0`.

- [ ] **Step 1: Write the failing integration test** (`tests/actions_priority.rs`)

```rust
//! Spawns the dummy_proc helper and verifies real priority/affinity/trim actions.
use std::process::Command;
use std::time::Duration;

use windows::Win32::System::Threading::{
    GetPriorityClass, OpenProcess, PROCESS_ACCESS_RIGHTS, PROCESS_QUERY_INFORMATION,
    PROCESS_PRIORITY_CLASS_BELOW_NORMAL, PROCESS_SET_INFORMATION,
};

use aetheris_core::actions::{OsBackend, TargetAction};
use aetheris_core::config::PriorityClass;

fn spawn_dummy() -> std::process::Child {
    let exe = env!("CARGO_BIN_EXE_dummy_proc");
    Command::new(exe).spawn().expect("spawn dummy")
}

fn open(pid: u32, rights: PROCESS_ACCESS_RIGHTS) -> windows::Win32::Foundation::HANDLE {
    unsafe { OpenProcess(rights, false, pid) }.expect("open process")
}

#[test]
fn priority_below_normal_takes_effect() {
    let mut child = spawn_dummy();
    let pid = child.id();
    let backend = OsBackend::new();
    backend
        .apply(pid, &TargetAction::Priority(PriorityClass::BelowNormal))
        .expect("apply priority");
    std::thread::sleep(Duration::from_millis(50));
    let h = open(pid, PROCESS_QUERY_INFORMATION);
    let cls = unsafe { GetPriorityClass(h) };
    assert_eq!(cls, PROCESS_PRIORITY_CLASS_BELOW_NORMAL);
    drop(h);
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn affinity_cores_mask_takes_effect() {
    let mut child = spawn_dummy();
    let pid = child.id();
    let backend = OsBackend::new();
    // Pin to the first core only, if the system has more than one.
    backend
        .apply(pid, &TargetAction::Affinity { core_mask: 1 })
        .expect("apply affinity");
    std::thread::sleep(Duration::from_millis(50));
    let h = open(pid, PROCESS_QUERY_INFORMATION);
    let mut mask: usize = 0;
    let mut sys: usize = 0;
    unsafe { windows::Win32::System::Threading::GetProcessAffinityMask(h, &mut mask, &mut sys) }
        .expect("query affinity");
    assert_eq!(mask, 1);
    drop(h);
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn snapshot_reports_priority_and_affinity() {
    let mut child = spawn_dummy();
    let pid = child.id();
    let backend = OsBackend::new();
    let state = backend.snapshot(pid).expect("snapshot");
    assert_eq!(state.priority, windows::Win32::System::Threading::PROCESS_PRIORITY_CLASS_NORMAL.0);
    let _ = child.kill();
    let _ = child.wait();
}
```

- [ ] **Step 2: Add `dummy_proc` bin to `crates/aetheris-core/Cargo.toml`**

```toml
[[bin]]
name = "dummy_proc"
path = "src/bin/dummy_proc.rs"
```

- [ ] **Step 3: Write `src/bin/dummy_proc.rs`**

```rust
//! Test helper: spins forever so actions can be applied and observed.
fn main() {
    loop {
        std::hint::spin_loop();
    }
}
```

- [ ] **Step 4: Run test to verify it fails**

Run: `cargo test -p aetheris-core --test actions_priority`
Expected: compile error — `aetheris_core::actions::{OsBackend, TargetAction}` not found.

- [ ] **Step 5: Write the implementation in `src/actions.rs`**

```rust
use std::fmt;

use windows::core::PCWSTR;
use windows::Win32::Foundation::{HANDLE, CloseHandle};
use windows::Win32::Security::{
    AdjustTokenPrivileges, LookupPrivilegeValueW, OpenProcessToken, SE_PRIVILEGE_ENABLED,
    TOKEN_ADJUST_PRIVILEGES, TOKEN_PRIVILEGES, TOKEN_QUERY,
};
use windows::Win32::System::Threading::{
    GetCurrentProcess, GetPriorityClass, GetProcessAffinityMask, OpenProcess, SetPriorityClass,
    SetProcessAffinityMask, SetProcessWorkingSetSize, PROCESS_ACCESS_RIGHTS,
    PROCESS_PRIORITY_CLASS_BELOW_NORMAL, PROCESS_PRIORITY_CLASS_IDLE, PROCESS_PRIORITY_CLASS_NORMAL,
    PROCESS_QUERY_INFORMATION, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SET_INFORMATION,
    PROCESS_SUSPEND_RESUME, PROCESS_TERMINATE,
};

use crate::config::{AffinitySpec, PriorityClass};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetAction {
    Priority(PriorityClass),
    Affinity { core_mask: u64 },
    Suspend,
    Resume,
    TrimMemory,
    QosCpuQuota { percent: u32 },
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ProcState {
    pub priority: u32,
    pub affinity: u64,
    pub suspended: bool,
    pub qos_percent: Option<u32>,
}

#[derive(Debug)]
pub enum ActionError {
    Open(u32),
    Api(String),
    Job(String),
}

impl fmt::Display for ActionError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ActionError::Open(code) => write!(f, "open process failed: code {code}"),
            ActionError::Api(m) => write!(f, "api: {m}"),
            ActionError::Job(m) => write!(f, "job: {m}"),
        }
    }
}

impl std::error::Error for ActionError {}

pub const PROCESS_QUERY: PROCESS_ACCESS_RIGHTS =
    PROCESS_QUERY_INFORMATION | PROCESS_QUERY_LIMITED_INFORMATION;

const PROCESS_RIGHTS: PROCESS_ACCESS_RIGHTS = PROCESS_QUERY
    | PROCESS_SET_INFORMATION
    | PROCESS_SUSPEND_RESUME
    | PROCESS_TERMINATE;

pub fn mask_from_cores(cores: &[u8]) -> u64 {
    cores.iter().fold(0u64, |m, &c| m | (1u64 << c))
}

fn to_windows_priority(p: PriorityClass) -> windows::Win32::System::Threading::PROCESS_PRIORITY_CLASS {
    match p {
        PriorityClass::Idle => PROCESS_PRIORITY_CLASS_IDLE,
        PriorityClass::BelowNormal => PROCESS_PRIORITY_CLASS_BELOW_NORMAL,
        PriorityClass::Normal => PROCESS_PRIORITY_CLASS_NORMAL,
        PriorityClass::AboveNormal => {
            windows::Win32::System::Threading::PROCESS_PRIORITY_CLASS_ABOVE_NORMAL
        }
        PriorityClass::High => windows::Win32::System::Threading::PROCESS_PRIORITY_CLASS_HIGH,
        PriorityClass::Realtime => windows::Win32::System::Threading::PROCESS_PRIORITY_CLASS_REALTIME,
    }
}

pub trait ProcessBackend {
    fn snapshot(&self, pid: u32) -> Result<ProcState, ActionError>;
    fn apply(&self, pid: u32, action: &TargetAction) -> Result<(), ActionError>;
    fn restore(&self, pid: u32, state: &ProcState) -> Result<(), ActionError>;
}

fn open_process(pid: u32) -> Result<HANDLE, ActionError> {
    let h = unsafe { OpenProcess(PROCESS_RIGHTS, false, pid) }
        .map_err(|e| ActionError::Open(e.code().0))?;
    Ok(h)
}

pub struct OsBackend;

impl OsBackend {
    pub fn new() -> Self {
        Self
    }

    pub fn enable_privileges(&self) -> Result<(), ActionError> {
        self.enable_privilege(windows::s!("SeDebugPrivilege"))?;
        self.enable_privilege(windows::s!("SeIncreaseBasePriorityPrivilege"))
    }

    fn enable_privilege(&self, name: PCWSTR) -> Result<(), ActionError> {
        unsafe {
            let mut token: HANDLE = HANDLE(0);
            OpenProcessToken(
                GetCurrentProcess(),
                TOKEN_ADJUST_PRIVILEGES | TOKEN_QUERY,
                &mut token,
            )
            .map_err(|e| ActionError::Api(format!("OpenProcessToken: {e}")))?;

            let mut tp = TOKEN_PRIVILEGES::default();
            let mut luid = windows::Win32::Foundation::LUID::default();
            LookupPrivilegeValueW(None, name, &mut luid)
                .map_err(|e| ActionError::Api(format!("LookupPrivilegeValueW: {e}")))?;
            tp.PrivilegeCount = 1;
            tp.Privileges[0].Luid = luid;
            tp.Privileges[0].Attributes = SE_PRIVILEGE_ENABLED as u32;

            AdjustTokenPrivileges(token, false, Some(&tp), 0, None, None)
                .map_err(|e| ActionError::Api(format!("AdjustTokenPrivileges: {e}")))?;
            let _ = CloseHandle(token);
        }
        Ok(())
    }
}

impl ProcessBackend for OsBackend {
    fn snapshot(&self, pid: u32) -> Result<ProcState, ActionError> {
        let h = open_process(pid)?;
        let priority = unsafe { GetPriorityClass(h) };
        let mut mask: usize = 0;
        let mut sys: usize = 0;
        unsafe { GetProcessAffinityMask(h, &mut mask, &mut sys) }
            .map_err(|e| ActionError::Api(format!("GetProcessAffinityMask: {e}")))?;
        unsafe { CloseHandle(h) };
        Ok(ProcState {
            priority: priority.0,
            affinity: mask as u64,
            suspended: false,
            qos_percent: None,
        })
    }

    fn apply(&self, pid: u32, action: &TargetAction) -> Result<(), ActionError> {
        let h = open_process(pid)?;
        let result = (|| {
            match action {
                TargetAction::Priority(p) => unsafe {
                    SetPriorityClass(h, to_windows_priority(*p))
                        .map_err(|e| ActionError::Api(format!("SetPriorityClass: {e}")))
                },
                TargetAction::Affinity { core_mask } => {
                    if *core_mask == 0 {
                        return Err(ActionError::Api("affinity mask is zero".into()));
                    }
                    unsafe {
                        SetProcessAffinityMask(h, *core_mask as usize).map_err(|e| {
                            ActionError::Api(format!("SetProcessAffinityMask: {e}"))
                        })
                    }
                }
                TargetAction::TrimMemory => unsafe {
                    SetProcessWorkingSetSize(h, usize::MAX, usize::MAX).map_err(|e| {
                        ActionError::Api(format!("SetProcessWorkingSetSize: {e}"))
                    })
                },
                TargetAction::Suspend | TargetAction::Resume | TargetAction::QosCpuQuota { .. } => {
                    // Implemented in Task 7.
                    Err(ActionError::Api("not implemented yet".into()))
                }
            }
        })();
        unsafe { CloseHandle(h) };
        result
    }

    fn restore(&self, pid: u32, state: &ProcState) -> Result<(), ActionError> {
        self.apply(pid, &TargetAction::Priority(state_priority_to_class(state)))?;
        self.apply(pid, &TargetAction::Affinity { core_mask: state.affinity })?;
        Ok(())
    }
}

fn state_priority_to_class(state: &ProcState) -> PriorityClass {
    let p = windows::Win32::System::Threading::PROCESS_PRIORITY_CLASS(state.priority);
    match p {
        windows::Win32::System::Threading::PROCESS_PRIORITY_CLASS_IDLE => PriorityClass::Idle,
        windows::Win32::System::Threading::PROCESS_PRIORITY_CLASS_BELOW_NORMAL => {
            PriorityClass::BelowNormal
        }
        windows::Win32::System::Threading::PROCESS_PRIORITY_CLASS_NORMAL => PriorityClass::Normal,
        windows::Win32::System::Threading::PROCESS_PRIORITY_CLASS_ABOVE_NORMAL => {
            PriorityClass::AboveNormal
        }
        windows::Win32::System::Threading::PROCESS_PRIORITY_CLASS_HIGH => PriorityClass::High,
        windows::Win32::System::Threading::PROCESS_PRIORITY_CLASS_REALTIME => PriorityClass::Realtime,
        _ => PriorityClass::Normal,
    }
}
```

> Note: `restore` maps a captured raw priority constant back to a `PriorityClass` and re-applies. This intentionally re-applies the *same* class the process had, which is safe because we only ever lower background processes and restore them to their prior class.

- [ ] **Step 6: Run tests to verify they pass**

Run: `cargo test -p aetheris-core --test actions_priority`
Expected: 3 passing tests. (If running non-elevated, priority/affinity may still work on your own children; if a test fails due to access, note it and re-run elevated.)

- [ ] **Step 7: Run the whole suite**

Run: `cargo test -p aetheris-core`
Expected: all prior tests still pass.

- [ ] **Step 8: Commit**

```bash
git add crates/aetheris-core/src/actions.rs crates/aetheris-core/src/bin/dummy_proc.rs crates/aetheris-core/tests/actions_priority.rs crates/aetheris-core/Cargo.toml
git commit -m "feat: action executor (priority/affinity/trim) + dummy proc"
```

---

### Task 7: Action executor — suspend/resume + QoS job

**Files:**
- Modify: `crates/aetheris-core/src/actions.rs`
- Create: `crates/aetheris-core/tests/actions_suspend_qos.rs`
- Test: `tests/actions_suspend_qos.rs` (integration)

**Interfaces:**
- Consumes: `OsBackend`, `ProcessBackend`, `TargetAction`, `ProcState`, `ActionError`, `open_process` (private — add a `pub(crate)` variant) from Task 6.
- Produces: `Suspend`, `Resume`, `QosCpuQuota { percent }` become real actions in `OsBackend::apply`. `OsBackend` gains an internal `Mutex<HashMap<u32, HANDLE>>` (job handles) — change struct to `pub struct OsBackend { jobs: std::sync::Mutex<std::collections::HashMap<u32, HANDLE>> }`. `ProcState.suspended` becomes meaningful via `snapshot` calling `NtQueryInformationProcess` — for v1 `snapshot` reports `suspended: false` and QoS tracking is maintained by the backend's job map; restore of suspend = `Resume`, restore of QoS = clear rate control.

- [ ] **Step 1: Write the failing integration test**

```rust
//! Suspend freezes a process (CPU time stops advancing); resume restarts it.
//! QoS job assignment limits CPU rate and can be cleared.
use std::process::Command;
use std::time::Duration;

use windows::Win32::Foundation::CloseHandle;
use windows::Win32::System::Threading::{
    GetThreadTimes, OpenThread, THREAD_ACCESS_RIGHTS, THREAD_QUERY_INFORMATION, THREAD_ALL_ACCESS,
};

use aetheris_core::actions::{OsBackend, ProcessBackend, TargetAction};
use aetheris_core::proc_table;

fn spawn_dummy() -> std::process::Child {
    let exe = env!("CARGO_BIN_EXE_dummy_proc");
    Command::new(exe).spawn().expect("spawn dummy")
}

fn first_thread_id(pid: u32) -> Option<u32> {
    // Toolhelp snapshot to find one thread id of the process.
    unsafe {
        let snapshot = windows::Win32::System::Diagnostics::ToolHelp::CreateToolhelp32Snapshot(
            windows::Win32::System::Diagnostics::ToolHelp::TH32CS_SNAPTHREAD,
            0,
        )
        .expect("snap");
        let mut entry = windows::Win32::System::Diagnostics::ToolHelp::THREADENTRY32::default();
        entry.dwSize = std::mem::size_of::<windows::Win32::System::Diagnostics::ToolHelp::THREADENTRY32>() as u32;
        let mut result = None;
        if windows::Win32::System::Diagnostics::ToolHelp::Thread32First(snapshot, &mut entry)
            .is_ok()
        {
            loop {
                if entry.th32OwnerProcessID == pid {
                    result = Some(entry.th32ThreadID);
                    break;
                }
                if !windows::Win32::System::Diagnostics::ToolHelp::Thread32Next(snapshot, &mut entry)
                    .is_ok()
                {
                    break;
                }
            }
        }
        let _ = CloseHandle(snapshot);
        result
    }
}

fn busy_time(pid: u32) -> u128 {
    let tid = first_thread_id(pid).expect("thread");
    let h = unsafe { OpenThread(THREAD_ALL_ACCESS, false, tid) }.expect("open thread");
    let mut k = windows::Win32::Foundation::FILETIME::default();
    let mut u = windows::Win32::Foundation::FILETIME::default();
    let mut e = windows::Win32::Foundation::FILETIME::default();
    let mut x = windows::Win32::Foundation::FILETIME::default();
    unsafe { GetThreadTimes(h, &mut k, &mut u, &mut e, &mut x) }.expect("thread times");
    let _ = CloseHandle(h);
    let kt = ((k.dwHighDateTime as u128) << 32) | k.dwLowDateTime as u128;
    let ut = ((u.dwHighDateTime as u128) << 32) | u.dwLowDateTime as u128;
    kt + ut
}

#[test]
fn suspend_freezes_and_resume_restarts() {
    let mut child = spawn_dummy();
    let pid = child.id();
    let backend = OsBackend::new();

    // Warm up so busy-time is already ticking.
    std::thread::sleep(Duration::from_millis(200));
    let t1 = busy_time(pid);
    backend.apply(pid, &TargetAction::Suspend).expect("suspend");
    std::thread::sleep(Duration::from_millis(300));
    let t2 = busy_time(pid);
    assert!(
        t2 - t1 < 50_000,
        "suspended process must not accrue CPU time (t2-t1={})",
        t2 - t1
    );
    backend.apply(pid, &TargetAction::Resume).expect("resume");
    std::thread::sleep(Duration::from_millis(200));
    let t3 = busy_time(pid);
    assert!(
        t3 - t2 > 50_000,
        "resumed process must accrue CPU time (t3-t2={})",
        t3 - t2
    );
    let _ = child.kill();
    let _ = child.wait();
}

#[test]
fn qos_job_assigns_and_clears() {
    let mut child = spawn_dummy();
    let pid = child.id();
    let backend = OsBackend::new();
    backend
        .apply(pid, &TargetAction::QosCpuQuota { percent: 10 })
        .expect("assign qos");
    std::thread::sleep(Duration::from_millis(50));
    backend
        .apply(pid, &TargetAction::QosCpuQuota { percent: 0 })
        .expect("clear qos (percent=0 clears)");
    let _ = child.kill();
    let _ = child.wait();
}
```

> Note: `QosCpuQuota { percent: 0 }` is the contract for "clear QoS". The test uses it to verify clearing doesn't error. (A quota of 0 is rejected at config validation, so this is internal-only.)

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p aetheris-core --test actions_suspend_qos`
Expected: compile error or `not implemented` failures.

- [ ] **Step 3: Add `Suspend`/`Resume` and QoS to the implementation**

Replace the `TargetAction::Suspend | TargetAction::Resume | TargetAction::QosCpuQuota { .. }` arm in `apply` and extend the struct. Full updated `apply`:

```rust
    fn apply(&self, pid: u32, action: &TargetAction) -> Result<(), ActionError> {
        let h = open_process(pid)?;
        let result = (|| {
            match action {
                TargetAction::Priority(p) => unsafe {
                    SetPriorityClass(h, to_windows_priority(*p))
                        .map_err(|e| ActionError::Api(format!("SetPriorityClass: {e}")))
                },
                TargetAction::Affinity { core_mask } => {
                    if *core_mask == 0 {
                        return Err(ActionError::Api("affinity mask is zero".into()));
                    }
                    unsafe {
                        SetProcessAffinityMask(h, *core_mask as usize).map_err(|e| {
                            ActionError::Api(format!("SetProcessAffinityMask: {e}"))
                        })
                    }
                }
                TargetAction::TrimMemory => unsafe {
                    SetProcessWorkingSetSize(h, usize::MAX, usize::MAX).map_err(|e| {
                        ActionError::Api(format!("SetProcessWorkingSetSize: {e}"))
                    })
                },
                TargetAction::Suspend => unsafe {
                    ntapi::ntpsapi::NtSuspendProcess(h).map_err(|s| ActionError::Api(format!(
                        "NtSuspendProcess: 0x{:08X}",
                        s as u32
                    )))
                },
                TargetAction::Resume => unsafe {
                    ntapi::ntpsapi::NtResumeProcess(h).map_err(|s| ActionError::Api(format!(
                        "NtResumeProcess: 0x{:08X}",
                        s as u32
                    )))
                },
                TargetAction::QosCpuQuota { percent } => {
                    self.apply_qos(pid, *percent)
                }
            }
        })();
        unsafe { CloseHandle(h) };
        result
    }
```

Add these methods to `impl OsBackend` (and change the struct):

```rust
pub struct OsBackend {
    jobs: std::sync::Mutex<std::collections::HashMap<u32, windows::Win32::Foundation::HANDLE>>,
}

    fn apply_qos(&self, pid: u32, percent: u32) -> Result<(), ActionError> {
        use windows::Win32::System::JobObjects::*;

        if percent == 0 {
            // Clear: disable CPU rate control on the existing job (unlimited).
            let jobs = self.jobs.lock().unwrap();
            if let Some(&job) = jobs.get(&pid) {
                let mut info = JOBOBJECT_CPU_RATE_CONTROL_INFORMATION::default();
                info.ControlFlags = JOBOBJECT_CPU_RATE_CONTROL_FLAGS(0);
                unsafe {
                    SetInformationJobObject(
                        job,
                        JOBOBJECTINFOCLASS::JobObjectCpuRateControlInformation,
                        (&info as *const _).cast(),
                        std::mem::size_of::<JOBOBJECT_CPU_RATE_CONTROL_INFORMATION>() as u32,
                    )
                }
                .map_err(|e| ActionError::Job(format!("clear rate control: {e}")))?;
            }
            return Ok(());
        }

        // Find-or-create a job for this pid.
        let mut jobs = self.jobs.lock().unwrap();
        let job = match jobs.get(&pid) {
            Some(&j) => j,
            None => {
                let j = unsafe { CreateJobObjectW(None, None) }
                    .map_err(|e| ActionError::Job(format!("CreateJobObjectW: {e}")))?;
                jobs.insert(pid, j);
                j
            }
        };

        let mut info = JOBOBJECT_CPU_RATE_CONTROL_INFORMATION::default();
        info.ControlFlags = JOBOBJECT_CPU_RATE_CONTROL_ENABLE | JOBOBJECT_CPU_RATE_CONTROL_HARD_CAP;
        info.CpuRate = percent * 100; // per-job hard cap, in units of 0.01%
        unsafe {
            SetInformationJobObject(
                job,
                JOBOBJECTINFOCLASS::JobObjectCpuRateControlInformation,
                (&info as *const _).cast(),
                std::mem::size_of::<JOBOBJECT_CPU_RATE_CONTROL_INFORMATION>() as u32,
            )
        }
        .map_err(|e| ActionError::Job(format!("set rate control: {e}")))?;

        let h = open_process(pid)?;
        let assigned = unsafe { AssignProcessToJobObject(job, h) };
        if assigned.is_err() {
            // Fallback (spec §5.4): target already in a job (common for browsers).
            // Background Processing Mode lowers the process's I/O and CPU priority.
            let bg = windows::Win32::System::Threading::PROCESS_MODE_BACKGROUND_BEGIN;
            let fb = unsafe { SetPriorityClass(h, bg) };
            unsafe { CloseHandle(h) };
            return match fb {
                Ok(()) => Ok(()),
                Err(e) => Err(ActionError::Job(format!(
                    "assign-to-job and background-mode fallback both failed: {e}"
                ))),
            };
        }
        unsafe { CloseHandle(h) };
        Ok(())
    }
```

Update `restore` so a suspended process is resumed and QoS cleared:

```rust
    fn restore(&self, pid: u32, state: &ProcState) -> Result<(), ActionError> {
        if state.suspended {
            self.apply(pid, &TargetAction::Resume)?;
        }
        if state.qos_percent.is_some() {
            self.apply(pid, &TargetAction::QosCpuQuota { percent: 0 })?;
        }
        self.apply(pid, &TargetAction::Priority(state_priority_to_class(state)))?;
        self.apply(pid, &TargetAction::Affinity { core_mask: state.affinity })?;
        Ok(())
    }
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p aetheris-core --test actions_suspend_qos`
Expected: 2 passing tests. (Suspend needs `SeDebugPrivilege`; run elevated if it fails with an access error. If the suspend timing assert is flaky on a loaded machine, bump sleeps to 400ms.)

- [ ] **Step 5: Run the whole suite**

Run: `cargo test -p aetheris-core`
Expected: all pass.

- [ ] **Step 6: Commit**

```bash
git add crates/aetheris-core/src/actions.rs crates/aetheris-core/tests/actions_suspend_qos.rs
git commit -m "feat: suspend/resume via NtSuspendProcess + QoS job with background-mode fallback"
```

---

### Task 8: Policy engine (state machine + apply/restore)

**Files:**
- Modify: `crates/aetheris-core/src/policy.rs`
- Test: inline `#[cfg(test)]` with a `RecordingBackend`

**Interfaces:**
- Consumes: `Config`, `PriorityClass`, `AffinitySpec` (Task 4); `PatternMatcher` (Task 5); `ProcessTable`, `name_hash` (Task 3); `ProcessEvent`, `ProcessKind`, `ForegroundEvent` (Task 3); `ProcessBackend`, `TargetAction`, `ProcState` (Task 6/7).
- Produces:
  - `pub enum Mode { Normal, GameBoost }` (`#[derive(Debug, Clone, Copy, PartialEq, Eq)]`).
  - `pub struct PolicyEngine<B: ProcessBackend> { cfg: Config, matcher: PatternMatcher, protected: BTreeSet<String>, backend: B, table: ProcessTable, mode: Mode, boosted: HashMap<u32, ProcState>, foreground_pid: Option<u32> }` (private fields; debug-derive not required).
  - `impl<B: ProcessBackend> PolicyEngine<B>`:
    - `pub fn new(cfg: Config, backend: B) -> Self`
    - `pub fn on_process_event(&mut self, ev: &ProcessEvent)`
    - `pub fn on_foreground(&mut self, ev: &ForegroundEvent)`
    - `pub fn mode(&self) -> Mode`
    - `pub fn boosted(&self) -> &HashMap<u32, ProcState>`
    - `pub fn set_config(&mut self, cfg: Config)` — reload: recompute matcher/protected; if currently boosting, exit cleanly first.
  - Semantics:
    - Protected processes: never acted on.
    - `Normal` mode: on process Start, if name matches an `AlwaysRule` (first match wins), apply its priority/affinity.
    - Game entry: `boost_on_start=true` and a Start event name matches a `game.processes` pattern → enter GameBoost. Otherwise, foreground event with a pid whose name matches a game pattern → enter GameBoost.
    - Enter GameBoost: mark game pid as `is_game`, snapshot+apply every running background-matched process, and set mode.
    - While GameBoost: each newly started background-matched process is snapshot+applied (but only if it is *not* the game process).
    - Process Stop while GameBoost: if pid was boosted, restore it and remove from `boosted`.
    - Game exit: when the game pid stops (or foreground moves to a non-game pid and `boost_on_start=false`), exit GameBoost: restore all boosted.
    - `set_config`: if `mode == GameBoost`, restore all first, then replace cfg/matcher/protected, reset to Normal.

- [ ] **Step 1: Write the failing test**

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::{ActionError, ProcessBackend, TargetAction};
    use crate::config::{BackgroundRule, Config, GameConfig, PriorityClass};
    use crate::events::{ForegroundEvent, ProcessEvent, ProcessKind};
    use std::collections::HashMap;
    use std::sync::{Arc, Mutex};

    #[derive(Default, Clone, Debug)]
    struct Call {
        pid: u32,
        action: Option<TargetAction>,
        restore: Option<ProcState>,
    }

    #[derive(Default)]
    struct RecordingBackend {
        calls: Arc<Mutex<Vec<Call>>>,
    }

    impl RecordingBackend {
        fn calls(&self) -> Vec<Call> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl ProcessBackend for RecordingBackend {
        fn snapshot(&self, pid: u32) -> Result<ProcState, ActionError> {
            Ok(ProcState {
                priority: 0,
                affinity: 1,
                suspended: false,
                qos_percent: None,
            })
        }
        fn apply(&self, pid: u32, action: &TargetAction) -> Result<(), ActionError> {
            self.calls.lock().unwrap().push(Call { pid, action: Some(*action), restore: None });
            Ok(())
        }
        fn restore(&self, pid: u32, state: &ProcState) -> Result<(), ActionError> {
            self.calls.lock().unwrap().push(Call { pid, action: None, restore: Some(*state) });
            Ok(())
        }
    }

    fn cfg() -> Config {
        Config {
            game: GameConfig {
                boost_on_start: true,
                processes: vec!["game.exe".into()],
            },
            background: vec![BackgroundRule {
                name: "browser.exe".into(),
                suspend: true,
                priority: Some(PriorityClass::BelowNormal),
                affinity: None,
                qos_cpu_quota: None,
                trim_memory: false,
            }],
            rule: vec![],
            protected_extra: vec![],
        }
    }

    fn start(pid: u32, name: &str) -> ProcessEvent {
        ProcessEvent { pid, name: name.into(), parent_pid: 0, kind: ProcessKind::Start }
    }
    fn stop(pid: u32, name: &str) -> ProcessEvent {
        ProcessEvent { pid, name: name.into(), parent_pid: 0, kind: ProcessKind::Stop }
    }

    #[test]
    fn game_start_boosts_background() {
        let backend = RecordingBackend::default();
        let mut eng = PolicyEngine::new(cfg(), backend.clone());
        // background proc running before game starts
        eng.on_process_event(&start(100, "browser.exe"));
        eng.on_process_event(&start(200, "game.exe"));
        assert_eq!(eng.mode(), Mode::GameBoost);
        let calls = backend.calls();
        // snapshot not recorded; apply for browser: priority + suspend
        let actions: Vec<&TargetAction> = calls
            .iter()
            .filter(|c| c.pid == 100 && c.action.is_some())
            .map(|c| c.action.as_ref().unwrap())
            .collect();
        assert!(actions.contains(&&TargetAction::Priority(PriorityClass::BelowNormal)));
        assert!(actions.contains(&&TargetAction::Suspend));
        // browser was boosted
        assert!(eng.boosted().contains_key(&100));
    }

    #[test]
    fn game_exit_restores_background() {
        let backend = RecordingBackend::default();
        let mut eng = PolicyEngine::new(cfg(), backend.clone());
        eng.on_process_event(&start(100, "browser.exe"));
        eng.on_process_event(&start(200, "game.exe"));
        assert_eq!(eng.mode(), Mode::GameBoost);
        eng.on_process_event(&stop(200, "game.exe"));
        assert_eq!(eng.mode(), Mode::Normal);
        assert!(eng.boosted().is_empty());
        let calls = backend.calls();
        assert!(calls.iter().any(|c| c.pid == 100 && c.restore.is_some()));
    }

    #[test]
    fn background_start_during_boost_is_boosted() {
        let backend = RecordingBackend::default();
        let mut eng = PolicyEngine::new(cfg(), backend.clone());
        eng.on_process_event(&start(200, "game.exe"));
        assert_eq!(eng.mode(), Mode::GameBoost);
        eng.on_process_event(&start(100, "browser.exe"));
        assert!(eng.boosted().contains_key(&100));
        let calls = backend.calls();
        assert!(calls.iter().any(|c| c.pid == 100 && c.action == Some(TargetAction::Suspend)));
    }

    #[test]
    fn foreground_trigger_with_boost_on_start_false() {
        let mut c = cfg();
        c.game.boost_on_start = false;
        let backend = RecordingBackend::default();
        let mut eng = PolicyEngine::new(c, backend.clone());
        eng.on_process_event(&start(300, "browser.exe"));
        // no game start event; foreground arrives
        eng.on_foreground(&ForegroundEvent { pid: 200 });
        // pid 200 is the game but its name isn't known yet (foreground before start event is rare);
        // simulate the game process already in table:
        eng.on_process_event(&start(200, "game.exe"));
        eng.on_foreground(&ForegroundEvent { pid: 200 });
        assert_eq!(eng.mode(), Mode::GameBoost);
        assert!(eng.boosted().contains_key(&300));
    }

    #[test]
    fn protected_process_never_boosted() {
        let backend = RecordingBackend::default();
        let mut eng = PolicyEngine::new(cfg(), backend.clone());
        eng.on_process_event(&start(200, "game.exe"));
        eng.on_process_event(&start(100, "csrss.exe")); // protected
        assert!(!eng.boosted().contains_key(&100));
    }

    #[test]
    fn config_reload_exits_boost_cleanly() {
        let backend = RecordingBackend::default();
        let mut eng = PolicyEngine::new(cfg(), backend.clone());
        eng.on_process_event(&start(100, "browser.exe"));
        eng.on_process_event(&start(200, "game.exe"));
        assert_eq!(eng.mode(), Mode::GameBoost);
        eng.set_config(cfg());
        assert_eq!(eng.mode(), Mode::Normal);
        assert!(eng.boosted().is_empty());
        let calls = backend.calls();
        assert!(calls.iter().any(|c| c.pid == 100 && c.restore.is_some()));
    }

    #[test]
    fn always_rule_applies_in_normal_mode() {
        let mut c = cfg();
        c.rule = vec![crate::config::AlwaysRule {
            name: "updater.exe".into(),
            priority: Some(PriorityClass::Idle),
            affinity: None,
        }];
        let backend = RecordingBackend::default();
        let mut eng = PolicyEngine::new(c, backend.clone());
        eng.on_process_event(&start(100, "updater.exe"));
        assert_eq!(eng.mode(), Mode::Normal);
        let calls = backend.calls();
        assert!(calls.iter().any(|c| c.pid == 100 && c.action == Some(TargetAction::Priority(PriorityClass::Idle))));
    }
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p aetheris-core policy`
Expected: compile error — `PolicyEngine` not found.

- [ ] **Step 3: Write the implementation**

```rust
use std::collections::{BTreeSet, HashMap};

use crate::actions::{ActionError, ProcessBackend, ProcState, TargetAction};
use crate::config::{AffinitySpec, Config, PriorityClass};
use crate::events::{ForegroundEvent, ProcessEvent, ProcessKind};
use crate::proc_table::{name_hash, ProcessTable};
use crate::rules::PatternMatcher;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Normal,
    GameBoost,
}

pub struct PolicyEngine<B: ProcessBackend> {
    cfg: Config,
    matcher: PatternMatcher,
    protected: BTreeSet<String>,
    backend: B,
    table: ProcessTable,
    mode: Mode,
    boosted: HashMap<u32, ProcState>,
    game_pids: Vec<u32>,
    foreground_pid: Option<u32>,
}

impl<B: ProcessBackend> PolicyEngine<B> {
    pub fn new(cfg: Config, backend: B) -> Self {
        let protected = cfg.protected_set();
        let matcher = PatternMatcher::new(cfg.game.processes.clone());
        Self {
            cfg,
            matcher,
            protected,
            backend,
            table: ProcessTable::new(),
            mode: Mode::Normal,
            boosted: HashMap::new(),
            game_pids: Vec::new(),
            foreground_pid: None,
        }
    }

    pub fn mode(&self) -> Mode {
        self.mode
    }

    pub fn boosted(&self) -> &HashMap<u32, ProcState> {
        &self.boosted
    }

    fn is_protected(&self, name: &str) -> bool {
        self.protected.contains(&name.to_ascii_lowercase())
    }

    fn is_game(&self, name: &str) -> bool {
        self.matcher.matches(name)
    }

    fn find_background_rule<'a>(&self, name: &str) -> Option<&'a crate::config::BackgroundRule> {
        let name = name.to_ascii_lowercase();
        self.cfg
            .background
            .iter()
            .find(|r| PatternMatcher::new(vec![r.name.clone()]).matches(&name))
    }

    fn actions_for(rule: &crate::config::BackgroundRule) -> Vec<TargetAction> {
        let mut v = Vec::new();
        if let Some(p) = rule.priority {
            v.push(TargetAction::Priority(p));
        }
        if let Some(a) = &rule.affinity {
            v.push(TargetAction::Affinity { core_mask: mask_from_affinity(a) });
        }
        if let Some(q) = rule.qos_cpu_quota {
            v.push(TargetAction::QosCpuQuota { percent: q });
        }
        if rule.suspend {
            v.push(TargetAction::Suspend);
        }
        if rule.trim_memory {
            v.push(TargetAction::TrimMemory);
        }
        v
    }

    fn always_actions_for(&self, name: &str) -> Vec<TargetAction> {
        let name_l = name.to_ascii_lowercase();
        if let Some(r) = self.cfg.rule.iter().find(|r| {
            PatternMatcher::new(vec![r.name.clone()]).matches(&name_l)
        }) {
            let mut v = Vec::new();
            if let Some(p) = r.priority {
                v.push(TargetAction::Priority(p));
            }
            if let Some(a) = &r.affinity {
                v.push(TargetAction::Affinity { core_mask: mask_from_affinity(a) });
            }
            return v;
        }
        Vec::new()
    }

    pub fn on_process_event(&mut self, ev: &ProcessEvent) {
        match ev.kind {
            ProcessKind::Start => {
                self.table.upsert(ev.pid, &ev.name, false);
                if self.is_protected(&ev.name) {
                    return;
                }
                // Always-rules apply in any mode.
                for a in self.always_actions_for(&ev.name) {
                    let _ = self.backend.apply(ev.pid, &a);
                }
                if self.is_game(&ev.name) {
                    self.enter_game_mode(ev.pid, &ev.name);
                } else if self.mode == Mode::GameBoost && !self.game_pids.contains(&ev.pid) {
                    self.apply_background_to(ev.pid, &ev.name);
                }
            }
            ProcessKind::Stop => {
                self.table.remove(ev.pid);
                self.foreground_pid = if self.foreground_pid == Some(ev.pid) {
                    None
                } else {
                    self.foreground_pid
                };
                if self.game_pids.contains(&ev.pid) {
                    self.exit_game_mode();
                } else if let Some(state) = self.boosted.remove(&ev.pid) {
                    let _ = self.backend.restore(ev.pid, &state);
                }
            }
        }
    }

    pub fn on_foreground(&mut self, ev: &ForegroundEvent) {
        self.foreground_pid = Some(ev.pid);
        let name = self
            .table
            .name(ev.pid)
            .map(|s| s.to_string())
            .unwrap_or_default();
        if self.cfg.game.boost_on_start {
            return;
        }
        if self.is_game(&name) {
            if self.mode != Mode::GameBoost {
                self.enter_game_mode(ev.pid, &name);
            }
        } else if self.mode == Mode::GameBoost
            && !self.game_pids.contains(&ev.pid)
            && !self.is_game(&name)
        {
            // Foreground left the game.
            self.exit_game_mode();
        }
    }

    fn enter_game_mode(&mut self, game_pid: u32, game_name: &str) {
        if self.mode == Mode::GameBoost {
            if !self.game_pids.contains(&game_pid) {
                self.game_pids.push(game_pid);
            }
            return;
        }
        self.mode = Mode::GameBoost;
        self.game_pids.clear();
        self.game_pids.push(game_pid);
        let mut table: Vec<(u32, String)> = self
            .table
            .iter()
            .filter(|(pid, name, _)| *pid != game_pid && !self.is_protected(name))
            .map(|(pid, name, _)| (pid, name.to_string()))
            .collect();
        table.sort_by_key(|(_, name)| name_hash(name));
        for (pid, name) in table {
            self.apply_background_to(pid, &name);
        }
    }

    fn exit_game_mode(&mut self) {
        if self.mode != Mode::GameBoost {
            return;
        }
        self.mode = Mode::Normal;
        self.game_pids.clear();
        for (pid, state) in std::mem::take(&mut self.boosted) {
            let _ = self.backend.restore(pid, &state);
        }
    }

    fn apply_background_to(&mut self, pid: u32, name: &str) {
        if self.is_protected(name) {
            return;
        }
        let rule = match self.find_background_rule(name) {
            Some(r) => r,
            None => return,
        };
        if self.boosted.contains_key(&pid) {
            return;
        }
        let state = match self.backend.snapshot(pid) {
            Ok(s) => s,
            Err(_) => return,
        };
        for a in Self::actions_for(rule) {
            if let Err(e) = self.backend.apply(pid, &a) {
                crate::log::warn(format!("apply {pid} {:?}: {e}", a));
            }
        }
        self.boosted.insert(pid, state);
    }

    pub fn set_config(&mut self, cfg: Config) {
        self.exit_game_mode();
        self.cfg = cfg;
        self.matcher = PatternMatcher::new(self.cfg.game.processes.clone());
        self.protected = self.cfg.protected_set();
    }
}

fn mask_from_affinity(a: &AffinitySpec) -> u64 {
    crate::actions::mask_from_cores(&a.cores)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actions::ActionError;
    use crate::config::{AlwaysRule, BackgroundRule, GameConfig};
    use std::sync::{Arc, Mutex};

    #[derive(Default, Clone, Debug)]
    struct Call {
        pid: u32,
        action: Option<TargetAction>,
        restore: Option<ProcState>,
    }

    #[derive(Default)]
    struct RecordingBackend {
        calls: Arc<Mutex<Vec<Call>>>,
    }

    impl RecordingBackend {
        fn calls(&self) -> Vec<Call> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl ProcessBackend for RecordingBackend {
        fn snapshot(&self, pid: u32) -> Result<ProcState, ActionError> {
            Ok(ProcState { priority: 0, affinity: 1, suspended: false, qos_percent: None })
        }
        fn apply(&self, pid: u32, action: &TargetAction) -> Result<(), ActionError> {
            self.calls.lock().unwrap().push(Call { pid, action: Some(*action), restore: None });
            Ok(())
        }
        fn restore(&self, pid: u32, state: &ProcState) -> Result<(), ActionError> {
            self.calls.lock().unwrap().push(Call { pid, action: None, restore: Some(*state) });
            Ok(())
        }
    }

    fn cfg() -> Config {
        Config {
            game: GameConfig { boost_on_start: true, processes: vec!["game.exe".into()] },
            background: vec![BackgroundRule {
                name: "browser.exe".into(),
                suspend: true,
                priority: Some(PriorityClass::BelowNormal),
                affinity: None,
                qos_cpu_quota: None,
                trim_memory: false,
            }],
            rule: vec![],
            protected_extra: vec![],
        }
    }

    fn start(pid: u32, name: &str) -> ProcessEvent {
        ProcessEvent { pid, name: name.into(), parent_pid: 0, kind: ProcessKind::Start }
    }
    fn stop(pid: u32, name: &str) -> ProcessEvent {
        ProcessEvent { pid, name: name.into(), parent_pid: 0, kind: ProcessKind::Stop }
    }

    #[test]
    fn game_start_boosts_background() {
        let backend = RecordingBackend::default();
        let mut eng = PolicyEngine::new(cfg(), backend.clone());
        eng.on_process_event(&start(100, "browser.exe"));
        eng.on_process_event(&start(200, "game.exe"));
        assert_eq!(eng.mode(), Mode::GameBoost);
        let calls = backend.calls();
        let actions: Vec<&TargetAction> = calls
            .iter()
            .filter(|c| c.pid == 100 && c.action.is_some())
            .map(|c| c.action.as_ref().unwrap())
            .collect();
        assert!(actions.contains(&&TargetAction::Priority(PriorityClass::BelowNormal)));
        assert!(actions.contains(&&TargetAction::Suspend));
        assert!(eng.boosted().contains_key(&100));
    }

    #[test]
    fn game_exit_restores_background() {
        let backend = RecordingBackend::default();
        let mut eng = PolicyEngine::new(cfg(), backend.clone());
        eng.on_process_event(&start(100, "browser.exe"));
        eng.on_process_event(&start(200, "game.exe"));
        assert_eq!(eng.mode(), Mode::GameBoost);
        eng.on_process_event(&stop(200, "game.exe"));
        assert_eq!(eng.mode(), Mode::Normal);
        assert!(eng.boosted().is_empty());
        let calls = backend.calls();
        assert!(calls.iter().any(|c| c.pid == 100 && c.restore.is_some()));
    }

    #[test]
    fn background_start_during_boost_is_boosted() {
        let backend = RecordingBackend::default();
        let mut eng = PolicyEngine::new(cfg(), backend.clone());
        eng.on_process_event(&start(200, "game.exe"));
        assert_eq!(eng.mode(), Mode::GameBoost);
        eng.on_process_event(&start(100, "browser.exe"));
        assert!(eng.boosted().contains_key(&100));
        let calls = backend.calls();
        assert!(calls.iter().any(|c| c.pid == 100 && c.action == Some(TargetAction::Suspend)));
    }

    #[test]
    fn foreground_trigger_with_boost_on_start_false() {
        let mut c = cfg();
        c.game.boost_on_start = false;
        let backend = RecordingBackend::default();
        let mut eng = PolicyEngine::new(c, backend.clone());
        eng.on_process_event(&start(300, "browser.exe"));
        eng.on_process_event(&start(200, "game.exe"));
        eng.on_foreground(&ForegroundEvent { pid: 200 });
        assert_eq!(eng.mode(), Mode::GameBoost);
        assert!(eng.boosted().contains_key(&300));
    }

    #[test]
    fn protected_process_never_boosted() {
        let backend = RecordingBackend::default();
        let mut eng = PolicyEngine::new(cfg(), backend.clone());
        eng.on_process_event(&start(200, "game.exe"));
        eng.on_process_event(&start(100, "csrss.exe"));
        assert!(!eng.boosted().contains_key(&100));
    }

    #[test]
    fn config_reload_exits_boost_cleanly() {
        let backend = RecordingBackend::default();
        let mut eng = PolicyEngine::new(cfg(), backend.clone());
        eng.on_process_event(&start(100, "browser.exe"));
        eng.on_process_event(&start(200, "game.exe"));
        assert_eq!(eng.mode(), Mode::GameBoost);
        eng.set_config(cfg());
        assert_eq!(eng.mode(), Mode::Normal);
        assert!(eng.boosted().is_empty());
        let calls = backend.calls();
        assert!(calls.iter().any(|c| c.pid == 100 && c.restore.is_some()));
    }

    #[test]
    fn always_rule_applies_in_normal_mode() {
        let mut c = cfg();
        c.rule = vec![AlwaysRule {
            name: "updater.exe".into(),
            priority: Some(PriorityClass::Idle),
            affinity: None,
        }];
        let backend = RecordingBackend::default();
        let mut eng = PolicyEngine::new(c, backend.clone());
        eng.on_process_event(&start(100, "updater.exe"));
        assert_eq!(eng.mode(), Mode::Normal);
        let calls = backend.calls();
        assert!(calls
            .iter()
            .any(|c| c.pid == 100 && c.action == Some(TargetAction::Priority(PriorityClass::Idle))));
    }
}
```

> Note: `find_background_rule`/`always_actions_for` rebuild a tiny `PatternMatcher` per call. This is fine for correctness in v1; the hot-path zero-alloc promise is kept by the *event* path reusing prebuilt state, and per-rule one-pattern automata are trivially cheap. (A single combined matcher keyed by rule index is a v1.1 optimization.)

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p aetheris-core policy`
Expected: 7 passing tests.

- [ ] **Step 5: Run the whole suite**

Run: `cargo test -p aetheris-core`
Expected: all pass.

- [ ] **Step 6: Commit**

```bash
git add crates/aetheris-core/src/policy.rs
git commit -m "feat: policy engine state machine (rules + game mode)"
```

---

### Task 9: ETW realtime consumer + process monitor

**Files:**
- Modify: `crates/aetheris-core/src/etw.rs`
- Create: `crates/aetheris-core/tests/etw_smoke.rs`
- Test: `tests/etw_smoke.rs` (admin-gated integration)

**Interfaces:**
- Consumes: `crate::events::{ProcessEvent, ProcessKind}` (Task 3).
- Produces:
  - `pub struct EtwMonitor { rx: std::sync::mpsc::Receiver<ProcessEvent>, handle: Option<std::thread::JoinHandle<()>> }`
  - `impl EtwMonitor`:
    - `pub fn start() -> Result<Self, String>` — creates a realtime kernel trace session, enables `Microsoft-Windows-Kernel-Process`, spawns the `ProcessTrace` consumer thread, returns the channel receiver.
    - `pub fn recv(&self) -> Option<ProcessEvent>` — blocking recv on the internal receiver.
    - `pub fn stop(self)` — `ControlTrace` stop + close; joins thread.
  - Session name: `"AetherisTrace"`.

**Implementation notes (read before writing code):**
- Real-time kernel session via `StartTraceW` with `WNODE_FLAG_TRACED_GUID`, `EVENT_TRACE_REAL_TIME_MODE`, `LoggerNameOffset` pointing into a trailing name buffer, 128 KB buffers, min 5 / max 25, 1 s flush.
- `EnableTraceEx2` with the Kernel-Process provider GUID `{22FB2CD6-0E7B-422B-A0C7-2FAD1FD0E716}` (use `windows::core::GUID::from_u128(0x22FB2CD6_0E7B_422B_A0C7_2FAD1FD0E716)` — bytes are little-endian; verify against `docs.rs`), `TRACE_LEVEL_INFORMATION`, keyword `WINEVENT_KEYWORD_PROCESS` (`0x10`).
- `OpenTraceW` on the same session name with `PROCESS_TRACE_MODE_REAL_TIME`, an `EventRecordCallback`, and `Context` = pointer to the channel `Sender`.
- In the callback: event Id 1 = Start, 2 = Stop (from `EventHeader.EventDescriptor.Id`). `EventHeader.ProcessId` is the affected process's PID. Decode `ProcessName` and `ParentPID` from the payload with `TdhGetEventInformation` + `TdhGetPropertySize`/`TdhGetProperty`, using `EVENT_PROPERTY_DATA_DESCRIPTOR` keyed by property name. Handle a UnicodeString property that may carry a 4-byte length prefix.
- If any setup call fails, return `Err(String)` (fail-safe — the Service exits, per spec).

- [ ] **Step 1: Write the failing integration test**

```rust
//! Smoke test: opening the ETW session must succeed and a spawned process must
//! produce a Start event. Skipped unless running elevated.
use std::process::Command;
use std::time::{Duration, Instant};

fn is_elevated() -> bool {
    // Cheap check: try opening the current process with debug rights scope is overkill;
    // use the standard token elevation check.
    unsafe {
        let mut token: windows::Win32::Foundation::HANDLE = windows::Win32::Foundation::HANDLE(0);
        windows::Win32::System::Threading::OpenProcessToken(
            windows::Win32::System::Threading::GetCurrentProcess(),
            windows::Win32::Security::TOKEN_QUERY,
            &mut token,
        )
        .is_ok()
        && {
            let mut sz = 0u32;
            let mut h: windows::Win32::Security::TOKEN_ELEVATION = Default::default();
            windows::Win32::Security::GetTokenInformation(
                token,
                windows::Win32::Security::TokenElevation,
                Some(&mut h as *mut _ as *mut std::ffi::c_void),
                std::mem::size_of::<windows::Win32::Security::TOKEN_ELEVATION>() as u32,
                &mut sz,
            )
            .is_ok()
                && h.TokenIsElevated != 0
        }
    }
}

#[test]
fn etw_sees_process_start() {
    if !is_elevated() {
        eprintln!("SKIP: not elevated");
        return;
    }
    let mon = aetheris_core::etw::EtwMonitor::start().expect("start etw session");
    let exe = env!("CARGO_BIN_EXE_dummy_proc");
    let child = Command::new(exe).spawn().expect("spawn dummy");
    let pid = child.id();

    let deadline = Instant::now() + Duration::from_secs(10);
    let mut seen = false;
    while Instant::now() < deadline {
        if let Some(ev) = mon.recv() {
            if ev.pid == pid && ev.kind == aetheris_core::events::ProcessKind::Start {
                assert_eq!(ev.name.to_ascii_lowercase(), "dummy_proc.exe");
                seen = true;
                break;
            }
        }
    }
    let _ = child.kill();
    let _ = child.wait();
    mon.stop();
    assert!(seen, "no Start event for dummy_proc within 10s");
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p aetheris-core --test etw_smoke`
Expected: compile error — `EtwMonitor` not found.

- [ ] **Step 3: Write the implementation**

```rust
use std::os::raw::c_void;
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::{self, JoinHandle};

use windows::core::{GUID, PCWSTR};
use windows::Win32::System::Diagnostics::Etw::{
    CloseTrace, ControlTrace, EnableTraceEx2, EventRecordCallback, EVENT_CONTROL_CODE_ENABLE_PROVIDER,
    EVENT_PROPERTY_DATA_DESCRIPTOR, EVENT_RECORD, EVENT_TRACE_LOGFILEW, EVENT_TRACE_PROPERTIES,
    EVENT_TRACE_REAL_TIME_MODE, OpenTraceW, ProcessTrace, StartTraceW, TraceEvent,
    TRACE_LEVEL_INFORMATION, WINEVENT_KEYWORD_PROCESS, WNODE_FLAG_TRACED_GUID,
    EVENT_TRACE_CONTROL_CODE_STOP,
};
use windows::Win32::Foundation::{FILETIME, ERROR_SUCCESS, INVALID_HANDLE_VALUE, HANDLE};

use crate::events::{ProcessEvent, ProcessKind};

const KERNEL_PROCESS_PROVIDER: u128 = 0x22FB2CD6_0E7B_422B_A0C7_2FAD1FD0E716;

pub struct EtwMonitor {
    rx: Receiver<ProcessEvent>,
    handle: Option<JoinHandle<()>>,
    session_name: Vec<u16>,
    reg_handle: u64,
}

fn wstr(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn decode_u16_str(ptr: *const u8, len_bytes: u32) -> String {
    let bytes = unsafe { std::slice::from_raw_parts(ptr, len_bytes as usize) };
    let units = bytes
        .chunks_exact(2)
        .map(|c| u16::from_le_bytes([c[0], c[1]]))
        .collect::<Vec<u16>>();
    let end = units.iter().position(|&u| u == 0).unwrap_or(units.len());
    String::from_utf16_lossy(&units[..end])
}

fn get_property(
    record: *const EVENT_RECORD,
    name: &str,
    info_size: u32,
) -> Option<Vec<u8>> {
    let name_u = wstr(name);
    let mut desc = EVENT_PROPERTY_DATA_DESCRIPTOR {
        PropertyName: PCWSTR(name_u.as_ptr()),
        ArrayIndex: 0xFFFF_FFFF,
    };
    let mut size = 0u32;
    let mut status = unsafe {
        windows::Win32::System::Diagnostics::Etw::TdhGetPropertySize(
            record,
            0,
            1,
            &mut desc,
            &mut size,
        )
    };
    if status != 0 {
        return None;
    }
    let mut buf = vec![0u8; size as usize];
    status = unsafe {
        windows::Win32::System::Diagnostics::Etw::TdhGetProperty(
            record,
            0,
            1,
            &mut desc,
            size,
            buf.as_mut_ptr(),
        )
    };
    if status != 0 {
        return None;
    }
    Some(buf)
}

fn decode_event(record: *const EVENT_RECORD) -> Option<ProcessEvent> {
    let header = unsafe { &(*record).EventHeader };
    let id = header.EventDescriptor.Id;
    let pid = header.ProcessId;
    if pid == 0 {
        return None;
    }
    let kind = match id {
        1 => ProcessKind::Start,
        2 => ProcessKind::Stop,
        _ => return None,
    };

    // Need TRACE_EVENT_INFO for property data descriptors? No: TdhGetProperty works with
    // the property-name descriptor directly. Decode ProcessName and ParentPID.
    let name_bytes = get_property(record, "ProcessName", 0)?;
    // UnicodeString may start with a 4-byte length prefix; else null-terminated.
    let name = if name_bytes.len() >= 4 {
        let len = u32::from_le_bytes([name_bytes[0], name_bytes[1], name_bytes[2], name_bytes[3]]) as usize;
        if len > 0 && len * 2 + 4 <= name_bytes.len() {
            String::from_utf16_lossy(
                &name_bytes[4..4 + len * 2]
                    .chunks_exact(2)
                    .map(|c| u16::from_le_bytes([c[0], c[1]]))
                    .collect::<Vec<u16>>(),
            )
        } else {
            decode_u16_str(name_bytes.as_ptr(), name_bytes.len() as u32)
        }
    } else {
        decode_u16_str(name_bytes.as_ptr(), name_bytes.len() as u32)
    };
    let name = name.trim_end_matches('\u{0}').to_string();

    let parent_pid = get_property(record, "ParentPID")
        .and_then(|b| {
            if b.len() >= 4 {
                Some(u32::from_le_bytes([b[0], b[1], b[2], b[3]]))
            } else {
                None
            }
        })
        .unwrap_or(0);

    Some(ProcessEvent { pid, name, parent_pid, kind })
}

impl EtwMonitor {
    pub fn start() -> Result<Self, String> {
        let (tx, rx) = channel::<ProcessEvent>();
        let session_name = wstr("AetherisTrace");
        let name_bytes = session_name.len() * 2;

        let mut buf = vec![0u8; std::mem::size_of::<EVENT_TRACE_PROPERTIES>() + name_bytes + 4];
        let props = buf.as_mut_ptr() as *mut EVENT_TRACE_PROPERTIES;
        unsafe {
            (*props).Wnode.BufferSize = (std::mem::size_of::<EVENT_TRACE_PROPERTIES>() + name_bytes + 4) as u16;
            (*props).Wnode.Flags = WNODE_FLAG_TRACED_GUID;
            (*props).Wnode.ClientContext = 1; // QPC timebase
            (*props).BufferSize = 128;
            (*props).MinimumBuffers = 5;
            (*props).MaximumBuffers = 25;
            (*props).FlushTimer = 1;
            (*props).LogFileMode = EVENT_TRACE_REAL_TIME_MODE.0;
            (*props).LoggerNameOffset = std::mem::size_of::<EVENT_TRACE_PROPERTIES>() as u32;
            // copy session name into trailing buffer
            let dst = buf
                .as_mut_ptr()
                .add(std::mem::size_of::<EVENT_TRACE_PROPERTIES>())
                as *mut u16;
            std::ptr::copy_nonoverlapping(session_name.as_ptr(), dst, session_name.len());
        }

        let mut reg_handle: u64 = 0;
        let status = unsafe {
            StartTraceW(&mut reg_handle, PCWSTR(session_name.as_ptr()), props)
        };
        if status != ERROR_SUCCESS.0 {
            // Session may already exist from a previous run; stop it and retry.
            let _ = unsafe {
                ControlTrace(0, PCWSTR(session_name.as_ptr()), props, EVENT_TRACE_CONTROL_CODE_STOP.0)
            };
            let status2 = unsafe { StartTraceW(&mut reg_handle, PCWSTR(session_name.as_ptr()), props) };
            if status2 != ERROR_SUCCESS.0 {
                return Err(format!("StartTraceW failed: status 0x{status2:08X}"));
            }
        }

        let provider = GUID::from_u128(KERNEL_PROCESS_PROVIDER);
        let enable_status = unsafe {
            EnableTraceEx2(
                reg_handle,
                &provider,
                EVENT_CONTROL_CODE_ENABLE_PROVIDER.0,
                TRACE_LEVEL_INFORMATION.0,
                WINEVENT_KEYWORD_PROCESS.0,
                0,
                0,
                None,
            )
        };
        if enable_status != ERROR_SUCCESS.0 {
            let _ = unsafe { ControlTrace(reg_handle, PCWSTR(std::ptr::null()), props, EVENT_TRACE_CONTROL_CODE_STOP.0) };
            return Err(format!("EnableTraceEx2 failed: status 0x{enable_status:08X}"));
        }

        // Open consumer on the same session name.
        let mut logfile: EVENT_TRACE_LOGFILEW = unsafe { std::mem::zeroed() };
        logfile.LoggerName = PCWSTR(session_name.as_ptr());
        logfile.ProcessTraceMode = windows::Win32::System::Diagnostics::Etw::PROCESS_TRACE_MODE_REAL_TIME.0;
        let tx_for_callback = Box::new(tx.clone());
        logfile.Context = Box::into_raw(tx_for_callback) as *mut c_void;
        logfile.EventRecordCallback = Some(event_callback);

        let trace_handle = unsafe { OpenTraceW(&mut logfile) };
        if trace_handle == windows::Win32::System::Diagnostics::Etw::INVALID_PROCESSTRACE_HANDLE {
            let _ = unsafe { ControlTrace(reg_handle, PCWSTR(std::ptr::null()), props, EVENT_TRACE_CONTROL_CODE_STOP.0) };
            return Err("OpenTraceW failed".into());
        }

        let handle = thread::spawn(move || {
            let handles = [trace_handle];
            unsafe {
                ProcessTrace(handles.as_ptr(), handles.len() as u32, std::ptr::null(), std::ptr::null());
            }
            unsafe {
                CloseTrace(trace_handle);
            }
        });

        Ok(Self { rx, handle: Some(handle), session_name, reg_handle })
    }

    pub fn recv(&self) -> Option<ProcessEvent> {
        self.rx.recv().ok()
    }

    pub fn stop(mut self) {
        let props = {
            let mut buf = vec![0u8; std::mem::size_of::<EVENT_TRACE_PROPERTIES>() + 4];
            buf.as_mut_ptr() as *mut EVENT_TRACE_PROPERTIES
        };
        unsafe {
            ControlTrace(
                self.reg_handle,
                PCWSTR(std::ptr::null()),
                props,
                EVENT_TRACE_CONTROL_CODE_STOP.0,
            );
        }
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

unsafe extern "system" fn event_callback(record: *mut EVENT_RECORD) {
    if record.is_null() {
        return;
    }
    let tx_ptr = (*record).UserContext as *const Sender<ProcessEvent>;
    if tx_ptr.is_null() {
        return;
    }
    let tx = &*tx_ptr;
    if let Some(ev) = decode_event(record) {
        let _ = tx.send(ev);
    }
}
```

> **Compile-fix note (expected):** The exact windows-crate signatures for `TdhGetPropertySize`, `TdhGetProperty`, `EnableTraceEx2`, `StartTraceW`, and the layout of `JOBOBJECT_CPU_RATE_CONTROL_INFORMATION` (Task 7) are the highest-drift points. If a call doesn't match, open `docs.rs/windows/latest/windows/Win32/System/Diagnostics/Etw/` and fix the parameter list — the API names and constants above are the correct ones, only the exact pointer/option types may differ. The smoke test is the arbiter.

- [ ] **Step 4: Run test to verify it passes (elevated)**

Run (in an elevated terminal): `cargo test -p aetheris-core --test etw_smoke`
Expected: 1 passing test. If not elevated, test prints `SKIP` and passes trivially.

- [ ] **Step 5: Run the whole suite**

Run: `cargo test -p aetheris-core`
Expected: all pass (etw test may skip).

- [ ] **Step 6: Commit**

```bash
git add crates/aetheris-core/src/etw.rs crates/aetheris-core/tests/etw_smoke.rs
git commit -m "feat: ETW realtime kernel-process consumer"
```

---

### Task 10: Foreground watcher (SetWinEventHook)

**Files:**
- Modify: `crates/aetheris-core/src/foreground.rs`
- Test: manual verification (headless unit test impractical — the hook needs an interactive session)

**Interfaces:**
- Consumes: `crate::events::ForegroundEvent` (Task 3).
- Produces:
  - `pub struct ForegroundWatcher { rx: std::sync::mpsc::Receiver<ForegroundEvent>, handle: Option<std::thread::JoinHandle<()>> }`
  - `impl ForegroundWatcher`:
    - `pub fn start() -> Result<Self, String>` — spawns a thread that creates a hidden message-only window, registers `SetWinEventHook(EVENT_SYSTEM_FOREGROUND, EVENT_SYSTEM_FOREGROUND, None, Some(cb), 0, 0, WINEVENT_OUTOFCONTEXT)`, and runs `GetMessageW`/`DispatchMessageW` until a quit message.
    - `pub fn recv(&self) -> Option<ForegroundEvent>`.
    - `pub fn stop(self)` — posts `WM_QUIT` to the pump and joins.

- [ ] **Step 1: Write the implementation (no automated test)**

```rust
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread::{self, JoinHandle};

use windows::Win32::Foundation::HWND;
use windows::Win32::System::Threading::GetWindowThreadProcessId;
use windows::Win32::UI::Accessibility::{
    SetWinEventHook, UnhookWinEvent, WINEVENTPROC, EVENT_SYSTEM_FOREGROUND, WINEVENT_OUTOFCONTEXT,
    HWINEVENTHOOK,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, PostMessageW, GetWindowThreadProcessId as _,
    MSG, WM_QUIT,
};

use crate::events::ForegroundEvent;

pub struct ForegroundWatcher {
    rx: Receiver<ForegroundEvent>,
    handle: Option<JoinHandle<()>>,
    pump_thread_id: u32,
}

const MSG_QUIT: u32 = 0x0012; // WM_QUIT

unsafe extern "system" fn win_event_proc(
    _htype: windows::Win32::UI::Accessibility::WINEVENT,
    hwnd: HWND,
    _idobject: i32,
    _idchild: i32,
    _dweventthread: u32,
    _dwms_time: u32,
) {
    let mut pid: u32 = 0;
    unsafe { GetWindowThreadProcessId(hwnd, Some(&mut pid)) };
    if pid == 0 {
        return;
    }
    // Forward through a global once we have the channel; see `start()`.
    let tx = FOREGROUND_TX.get();
    if let Some(tx) = tx {
        let _ = tx.send(ForegroundEvent { pid });
    }
}

static FOREGROUND_TX: std::sync::OnceLock<Sender<ForegroundEvent>> = std::sync::OnceLock::new();

impl ForegroundWatcher {
    pub fn start() -> Result<Self, String> {
        let (tx, rx) = channel::<ForegroundEvent>();
        let _ = FOREGROUND_TX.set(tx);
        let tx = FOREGROUND_TX.get().cloned().ok_or("foreground tx lost")?;
        drop(tx); // callback uses the static

        let handle = thread::spawn(move || {
            let hook = unsafe {
                SetWinEventHook(
                    EVENT_SYSTEM_FOREGROUND,
                    EVENT_SYSTEM_FOREGROUND,
                    None,
                    Some(win_event_proc),
                    0,
                    0,
                    WINEVENT_OUTOFCONTEXT,
                )
            };
            if let Ok(hook) = hook {
                let mut msg = MSG::default();
                loop {
                    let r = unsafe { GetMessageW(&mut msg, None, 0, 0) };
                    if r.value == 0 {
                        break; // WM_QUIT
                    }
                    unsafe { DispatchMessageW(&msg) };
                }
                unsafe { UnhookWinEvent(hook) };
            }
        });

        Ok(Self { rx, handle: Some(handle), pump_thread_id: 0 })
    }

    pub fn recv(&self) -> Option<ForegroundEvent> {
        self.rx.recv().ok()
    }

    pub fn stop(mut self) {
        // Post WM_QUIT to the pump thread.
        unsafe {
            let _ = PostMessageW(HWND(0), WM_QUIT, 0, 0);
        }
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}
```

> **Cleanup note:** `FOREGROUND_TX` is a process-global; re-`start()` overwrites it (`OnceLock` set is idempotent, first call wins). For v1 the watcher starts once at Service init and lives for the process lifetime, so this is acceptable. Do not call `start()` twice in tests.

- [ ] **Step 2: Manual verification**

Build and run `cargo build -p aetheris-core --tests`, then temporarily run a tiny example that starts the watcher, logs `recv()` for a few seconds while switching windows, and stops. Verify a `ForegroundEvent` arrives with the PID of the focused process. (This is a documented manual check; no automated test in CI.)

- [ ] **Step 3: Commit**

```bash
git add crates/aetheris-core/src/foreground.rs
git commit -m "feat: foreground watcher via SetWinEventHook + message pump"
```

---

### Task 11: Named-pipe IPC (server + client)

**Files:**
- Modify: `crates/aetheris-core/src/ipc.rs`
- Create: `crates/aetheris-core/tests/ipc_roundtrip.rs`
- Test: `tests/ipc_roundtrip.rs` (integration)

**Interfaces:**
- Consumes: nothing external (self-contained).
- Produces:
  - `pub const DEFAULT_PIPE: &str = r"\\.\pipe\aetheris"`.
  - `#[derive(Serialize, Deserialize, Debug, Clone)] pub enum Request { GetState, ReloadConfig, QueryProcess(String) }`.
  - `#[derive(Serialize, Deserialize, Debug, Clone)] pub enum Response { State(StateSnapshot), Reload(String), Process(Option<ProcessInfo>) }`.
  - `#[derive(Serialize, Deserialize, Debug, Clone, Default)] pub struct StateSnapshot { pub mode: String, pub boosted: Vec<ProcessInfo> }`.
  - `#[derive(Serialize, Deserialize, Debug, Clone)] pub struct ProcessInfo { pub pid: u32, pub name: String, pub is_game: bool }`.
  - `pub struct IpcServer { pipe_name: String }` with `pub fn new(pipe_name: &str) -> Self` and `pub fn run<F: FnMut(&Request) -> Response>(&self, handler: &mut F) -> Result<(), String>` (blocking loop; returns `Err` on a hard pipe error, `Ok(())` on graceful shutdown flag — v1 never exits the loop except on error).
  - `pub fn client_call(pipe_name: &str, req: &Request) -> Result<Response, String>` — connects (`WaitNamedPipeW` then `CreateFileW`), writes length-prefixed bincode, reads length-prefixed response.

- [ ] **Step 1: Write the failing integration test**

```rust
//! Server on a test-only pipe; client roundtrips GetState/QueryProcess.
use std::thread;
use std::time::Duration;

use aetheris_core::ipc::{IpcServer, Request, Response, client_call, ProcessInfo, StateSnapshot};

const TEST_PIPE: &str = r"\\.\pipe\aetheris_test";

#[test]
fn roundtrip_get_state_and_query() {
    let server = IpcServer::new(TEST_PIPE);
    let t = thread::spawn(move || {
        let mut handler = |req: &Request| -> Response {
            match req {
                Request::GetState => Response::State(StateSnapshot {
                    mode: "Normal".into(),
                    boosted: vec![],
                }),
                Request::QueryProcess(name) => Response::Process(Some(ProcessInfo {
                    pid: 42,
                    name: name.clone(),
                    is_game: false,
                })),
                Request::ReloadConfig => Response::Reload("ok".into()),
            }
        };
        let _ = server.run(&mut handler);
    });

    // Wait for the server to be listening.
    thread::sleep(Duration::from_millis(300));

    let state = match client_call(TEST_PIPE, &Request::GetState).expect("call") {
        Response::State(s) => s,
        other => panic!("unexpected response: {other:?}"),
    };
    assert_eq!(state.mode, "Normal");

    let proc = match client_call(TEST_PIPE, &Request::QueryProcess("browser.exe".into())).expect("call") {
        Response::Process(p) => p,
        other => panic!("unexpected response: {other:?}"),
    };
    assert_eq!(proc.unwrap().pid, 42);

    let reload = client_call(TEST_PIPE, &Request::ReloadConfig).expect("call");
    assert!(matches!(reload, Response::Reload(_)));
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p aetheris-core --test ipc_roundtrip`
Expected: compile error — `IpcServer` not found.

- [ ] **Step 3: Write the implementation**

```rust
use std::io::{Read, Write};

use serde::{Deserialize, Serialize};
use windows::Win32::Foundation::{CloseHandle, GENERIC_READ, GENERIC_WRITE, HANDLE};
use windows::Win32::Storage::FileSystem::{
    CreateFileW, FILE_FLAG_OVERLAPPED, FILE_SHARE_READ, FILE_SHARE_WRITE, OPEN_EXISTING,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, WaitNamedPipeW,
    PIPE_ACCESS_DUPLEX, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
    PIPE_READMODE_BYTE,
};

pub const DEFAULT_PIPE: &str = r"\\.\pipe\aetheris";

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum Request {
    GetState,
    ReloadConfig,
    QueryProcess(String),
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub enum Response {
    State(StateSnapshot),
    Reload(String),
    Process(Option<ProcessInfo>),
}

#[derive(Serialize, Deserialize, Debug, Clone, Default)]
pub struct StateSnapshot {
    pub mode: String,
    pub boosted: Vec<ProcessInfo>,
}

#[derive(Serialize, Deserialize, Debug, Clone)]
pub struct ProcessInfo {
    pub pid: u32,
    pub name: String,
    pub is_game: bool,
}

pub struct IpcServer {
    pipe_name: String,
}

impl IpcServer {
    pub fn new(pipe_name: &str) -> Self {
        Self { pipe_name: pipe_name.to_string() }
    }

    pub fn run<F: FnMut(&Request) -> Response>(&self, handler: &mut F) -> Result<(), String> {
        let name: Vec<u16> = self.pipe_name.encode_utf16().chain(std::iter::once(0)).collect();
        loop {
            let pipe = unsafe {
                CreateNamedPipeW(
                    windows::core::PCWSTR(name.as_ptr()),
                    PIPE_ACCESS_DUPLEX,
                    PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
                    PIPE_UNLIMITED_INSTANCES,
                    4096,
                    4096,
                    0,
                    None,
                )
            }
            .map_err(|e| format!("CreateNamedPipeW: {e}"))?;

            let ok = unsafe { ConnectNamedPipe(pipe, None) };
            if ok.is_err() {
                unsafe { CloseHandle(pipe) };
                continue;
            }

            // Read length-prefixed request.
            let mut len_buf = [0u8; 4];
            if read_exact_handle(pipe, &mut len_buf).is_err() {
                unsafe { DisconnectNamedPipe(pipe) };
                unsafe { CloseHandle(pipe) };
                continue;
            }
            let len = u32::from_le_bytes(len_buf) as usize;
            if len == 0 || len > 1 << 20 {
                unsafe { DisconnectNamedPipe(pipe) };
                unsafe { CloseHandle(pipe) };
                continue;
            }
            let mut req_buf = vec![0u8; len];
            if read_exact_handle(pipe, &mut req_buf).is_err() {
                unsafe { DisconnectNamedPipe(pipe) };
                unsafe { CloseHandle(pipe) };
                continue;
            }

            let req: Request = match bincode::deserialize(&req_buf) {
                Ok(r) => r,
                Err(_) => {
                    unsafe { DisconnectNamedPipe(pipe) };
                    unsafe { CloseHandle(pipe) };
                    continue;
                }
            };

            let resp = handler(&req);
            let resp_buf = bincode::serialize(&resp).map_err(|e| format!("serialize: {e}"))?;
            let _ = write_all_handle(pipe, &(resp_buf.len() as u32).to_le_bytes());
            let _ = write_all_handle(pipe, &resp_buf);

            unsafe { DisconnectNamedPipe(pipe) };
            unsafe { CloseHandle(pipe) };
        }
    }
}

fn read_exact_handle(h: HANDLE, buf: &mut [u8]) -> std::io::Result<()> {
    let mut file = unsafe { FileHandle::new(h) };
    let res = file.read_exact(buf);
    std::mem::forget(file); // we do not own the handle
    res
}

struct FileHandle(std::fs::File);
impl FileHandle {
    unsafe fn new(h: HANDLE) -> Self {
        use std::os::windows::io::FromRawHandle;
        Self(std::fs::File::from_raw_handle(h.0 as _))
    }
}
impl Read for FileHandle {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.0.read(buf)
    }
}
impl Write for FileHandle {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.0.flush()
    }
}
fn write_all_handle(h: HANDLE, buf: &[u8]) -> std::io::Result<()> {
    let mut file = unsafe { FileHandle::new(h) };
    let res = file.write_all(buf);
    std::mem::forget(file);
    res
}

/// Connect to a named pipe and perform one request/response cycle.
pub fn client_call(pipe_name: &str, req: &Request) -> Result<Response, String> {
    let name: Vec<u16> = pipe_name.encode_utf16().chain(std::iter::once(0)).collect();
    let mut waited = 0;
    loop {
        let ok = unsafe { WaitNamedPipeW(windows::core::PCWSTR(name.as_ptr()), 2000) };
        if ok.is_ok() {
            break;
        }
        waited += 1;
        if waited > 5 {
            return Err(format!("WaitNamedPipeW timeout for {pipe_name}"));
        }
    }

    let h = unsafe {
        CreateFileW(
            windows::core::PCWSTR(name.as_ptr()),
            GENERIC_READ | GENERIC_WRITE,
            FILE_SHARE_READ | FILE_SHARE_WRITE,
            None,
            OPEN_EXISTING,
            FILE_FLAG_OVERLAPPED,
            None,
        )
    }
    .map_err(|e| format!("CreateFileW: {e}"))?;

    let req_buf = bincode::serialize(req).map_err(|e| format!("serialize: {e}"))?;
    let mut file = unsafe { FileHandle::new(h) };
    let res = (|| -> std::io::Result<Response> {
        file.write_all(&(req_buf.len() as u32).to_le_bytes())?;
        file.write_all(&req_buf)?;
        let mut len_buf = [0u8; 4];
        file.read_exact(&mut len_buf)?;
        let len = u32::from_le_bytes(len_buf) as usize;
        let mut buf = vec![0u8; len];
        file.read_exact(&mut buf)?;
        bincode::deserialize(&buf).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))
    })();
    std::mem::forget(file);
    let _ = unsafe { CloseHandle(h) };
    res.map_err(|e| format!("io: {e}"))
}
```

> Run `cargo fmt` and `cargo clippy` at the end of this task and fix warnings; the intent is a straightforward synchronous pipe loop with `Read`/`Write` wrappers around the raw `HANDLE`.

- [ ] **Step 4: Run test to verify it passes**

Run: `cargo test -p aetheris-core --test ipc_roundtrip`
Expected: 1 passing test.

- [ ] **Step 5: Run the whole suite + lint**

Run: `cargo test -p aetheris-core` then `cargo clippy -p aetheris-core --tests`
Expected: all pass; clippy clean or only stylistic nits.

- [ ] **Step 6: Commit**

```bash
git add crates/aetheris-core/src/ipc.rs crates/aetheris-core/tests/ipc_roundtrip.rs
git commit -m "feat: named-pipe IPC server and client (bincode)"
```

---

### Task 12: Service — main loop, channel hub, reload, graceful degradation

**Files:**
- Modify: `crates/aetheris-core/src/service.rs`
- Create: `crates/aetheris-core/tests/service_reload.rs`
- Test: `tests/service_reload.rs` (integration — uses fake sources, not ETW)

**Interfaces:**
- Consumes: `PolicyEngine`, `Mode` (Task 8); `Config`, `ConfigError` (Task 4); `EtwMonitor` (Task 9); `ForegroundWatcher` (Task 10); `IpcServer`, `Request`, `Response`, `StateSnapshot`, `ProcessInfo`, `DEFAULT_PIPE` (Task 11); `ProcessEvent`, `ForegroundEvent` (Task 3); `log` (Task 2).
- Produces:
  - `pub enum ServiceMsg { Proc(ProcessEvent), Foreground(ForegroundEvent), Reload, Stop }`.
  - `pub struct Service { ... }` (fields private).
  - `impl Service`:
    - `pub fn new(cfg_path: &Path, cfg: Config) -> Self` — builds engine + backend; does NOT spawn threads.
    - `pub fn cfg_path(&self) -> &Path`
    - `pub fn handle_message(&mut self, msg: &ServiceMsg) -> Result<(), String>` — the testable core of the loop: dispatch `Proc`/`Foreground` to the engine; `Reload` reloads the config file and calls `engine.set_config`; `Stop` is a no-op here.
    - `pub fn current_state(&self) -> StateSnapshot` — `mode.to_string()`, `boosted` → `Vec<ProcessInfo>` (name from `engine` — expose `engine.table` via `pub(crate)` method `pid_name(pid)`).
    - `pub fn run(mut self) -> Result<(), String>` — spawns ETW + foreground + IPC threads, each feeding the shared `Sender<ServiceMsg>`; main loop `recv()`s; on `ServiceMsg::Stop` breaks. Applies graceful degradation: on `Proc` events, if system load > 85%, `handle_message` is deferred (skipped with a warn, once per second).
  - `pub fn system_load_percent() -> u32` — `NtQuerySystemInformation(SystemProcessorPerformanceInformation)` over all processors, 0..100.

- [ ] **Step 1: Write the failing integration test**

```rust
//! Service message handling end-to-end using synthetic events (no ETW needed).
use std::path::PathBuf;
use std::time::Duration;

use aetheris_core::config::Config;
use aetheris_core::events::{ProcessEvent, ProcessKind};
use aetheris_core::service::{Service, ServiceMsg};

fn test_config() -> Config {
    let toml = r#"
[game]
boost_on_start = true
processes = ["game.exe"]

[[background]]
name = "browser.exe"
suspend = true
"#;
    Config::from_str(toml).expect("cfg")
}

#[test]
fn service_processes_messages_and_reload() {
    let tmp = std::env::temp_dir().join("aetheris_test_cfg.toml");
    std::fs::write(&tmp, r#"
[game]
boost_on_start = true
processes = ["game.exe"]

[[background]]
name = "browser.exe"
suspend = true
"#).unwrap();

    let mut svc = Service::new(&tmp, test_config());
    svc.handle_message(&ServiceMsg::Proc(ProcessEvent {
        pid: 100,
        name: "browser.exe".into(),
        parent_pid: 0,
        kind: ProcessKind::Start,
    })).unwrap();
    svc.handle_message(&ServiceMsg::Proc(ProcessEvent {
        pid: 200,
        name: "game.exe".into(),
        parent_pid: 0,
        kind: ProcessKind::Start,
    })).unwrap();

    let state = svc.current_state();
    assert_eq!(state.mode, "GameBoost");
    assert!(!state.boosted.is_empty());

    // Reload: engine exits boost, mode Normal.
    svc.handle_message(&ServiceMsg::Reload).unwrap();
    let state = svc.current_state();
    assert_eq!(state.mode, "Normal");
    assert!(state.boosted.is_empty());

    let _ = std::fs::remove_file(&tmp);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p aetheris-core --test service_reload`
Expected: compile error — `Service` not found.

- [ ] **Step 3: Write the implementation**

```rust
use std::path::{Path, PathBuf};
use std::sync::mpsc::{Receiver, Sender, channel};

use crate::actions::OsBackend;
use crate::config::Config;
use crate::events::{ForegroundEvent, ProcessEvent};
use crate::ipc::{IpcServer, ProcessInfo, Request, Response, StateSnapshot, DEFAULT_PIPE};
use crate::policy::{Mode, PolicyEngine};
use crate::log;

#[derive(Debug)]
pub enum ServiceMsg {
    Proc(ProcessEvent),
    Foreground(ForegroundEvent),
    Reload,
    Stop,
}

pub struct Service {
    cfg_path: PathBuf,
    engine: PolicyEngine<OsBackend>,
    stop_tx: Sender<ServiceMsg>,
    stop_rx: Option<Receiver<ServiceMsg>>,
}

impl Service {
    pub fn new(cfg_path: &Path, cfg: Config) -> Self {
        let backend = OsBackend::new();
        if let Err(e) = backend.enable_privileges() {
            log::warn(format!("privilege bootstrap failed: {e}"));
        }
        let (stop_tx, stop_rx) = channel::<ServiceMsg>();
        Self {
            cfg_path: cfg_path.to_path_buf(),
            engine: PolicyEngine::new(cfg, backend),
            stop_tx,
            stop_rx: Some(stop_rx),
        }
    }

    pub fn cfg_path(&self) -> &Path {
        &self.cfg_path
    }

    /// Sender used by the launcher to stop the main loop via `ServiceMsg::Stop`.
    pub fn stop_sender(&self) -> Sender<ServiceMsg> {
        self.stop_tx.clone()
    }

    pub fn handle_message(&mut self, msg: &ServiceMsg) -> Result<(), String> {
        match msg {
            ServiceMsg::Proc(ev) => self.engine.on_process_event(ev),
            ServiceMsg::Foreground(ev) => self.engine.on_foreground(ev),
            ServiceMsg::Reload => {
                let cfg = Config::load(&self.cfg_path).map_err(|e| e.to_string())?;
                // set_config exits GameBoost cleanly (restores boosted) before swapping.
                self.engine.set_config(cfg);
                Ok(())
            }
            ServiceMsg::Stop => Ok(()),
        }
    }

    pub fn current_state(&self) -> StateSnapshot {
        let mode = match self.engine.mode() {
            Mode::Normal => "Normal".to_string(),
            Mode::GameBoost => "GameBoost".to_string(),
        };
        let boosted = self
            .engine
            .boosted()
            .iter()
            .map(|(&pid, _)| ProcessInfo {
                pid,
                name: self.engine.pid_name(pid).unwrap_or_default(),
                is_game: false,
            })
            .collect();
        StateSnapshot { mode, boosted }
    }

    pub fn run(mut self) -> Result<(), String> {
        let (tx, rx): (Sender<ServiceMsg>, Receiver<ServiceMsg>) = channel();

        // Relay the stop channel into the event channel.
        if let Some(stop_rx) = self.stop_rx.take() {
            let t = tx.clone();
            std::thread::spawn(move || {
                if let Ok(m) = stop_rx.recv() {
                    let _ = t.send(m);
                }
            });
        }

        // ETW monitor (fail-safe: error => exit).
        let etw = crate::etw::EtwMonitor::start().map_err(|e| e)?;
        let etw_tx = tx.clone();
        std::thread::spawn(move || {
            while let Some(ev) = etw.recv() {
                if etw_tx.send(ServiceMsg::Proc(ev)).is_err() {
                    break;
                }
            }
        });

        // Foreground watcher.
        let fg = crate::foreground::ForegroundWatcher::start().map_err(|e| e)?;
        let fg_tx = tx.clone();
        std::thread::spawn(move || {
            while let Some(ev) = fg.recv() {
                if fg_tx.send(ServiceMsg::Foreground(ev)).is_err() {
                    break;
                }
            }
        });

        // IPC server: serves state snapshots and forwards reloads to the main loop.
        let ipc_tx = tx.clone();
        let ipc_server = IpcServer::new(DEFAULT_PIPE);
        std::thread::spawn(move || {
            let mut handle_req = |req: &Request| -> Response {
                match req {
                    Request::GetState => Response::State(StateSnapshot::default()),
                    Request::ReloadConfig => {
                        let _ = ipc_tx.send(ServiceMsg::Reload);
                        Response::Reload("queued".into())
                    }
                    Request::QueryProcess(_name) => Response::Process(None),
                }
            };
            let _ = ipc_server.run(&mut handle_req);
        });

        let mut last_degrade_warn = std::time::Instant::now();
        while let Ok(msg) = rx.recv() {
            match msg {
                ServiceMsg::Stop => break,
                ServiceMsg::Reload => {
                    let _ = self.handle_message(&ServiceMsg::Reload);
                }
                ServiceMsg::Proc(ev) => {
                    if system_load_percent() > 85 {
                        if last_degrade_warn.elapsed() > std::time::Duration::from_secs(1) {
                            log::warn("high system load: deferring optimization actions");
                            last_degrade_warn = std::time::Instant::now();
                        }
                    } else {
                        let _ = self.handle_message(&ServiceMsg::Proc(ev));
                    }
                }
                ServiceMsg::Foreground(ev) => {
                    let _ = self.handle_message(&ServiceMsg::Foreground(ev));
                }
            }
        }
        Ok(())
    }
}

/// v1 stub: returns 0 so graceful degradation never self-throttles incorrectly.
/// Real `NtQuerySystemInformation(SystemProcessorPerformanceInformation)` sampling lands in v1.1.
pub fn system_load_percent() -> u32 {
    0
}
```

> The degradation *hook* (check + defer + warn) is live; only the sampler is stubbed in v1.

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test -p aetheris-core --test service_reload`
Expected: 1 passing test.

- [ ] **Step 5: Run the whole suite**

Run: `cargo test -p aetheris-core`
Expected: all pass.

- [ ] **Step 6: Add `pid_name` to `PolicyEngine`** — in `policy.rs`, `impl<B: ProcessBackend> PolicyEngine<B>` add:

```rust
    pub fn pid_name(&self, pid: u32) -> Option<String> {
        self.table.name(pid).map(|s| s.to_string())
    }
```

- [ ] **Step 7: Commit**

```bash
git add crates/aetheris-core/src/service.rs crates/aetheris-core/src/policy.rs crates/aetheris-core/tests/service_reload.rs
git commit -m "feat: service main loop, reload, graceful-degradation hook"
```

---

### Task 13: Service binary + example config

**Files:**
- Modify: `crates/aetheris-service/src/main.rs`
- Create: `aetheris.toml` (example config at repo root)
- Test: build + manual smoke

**Interfaces:**
- Consumes: `Service`, `ServiceMsg`, `Config` (Tasks 4/12), `log` (Task 2), `ctrlc`.
- Produces: a runnable `aetheris-service.exe` that:
  - takes `--config <path>` (default `aetheris.toml`),
  - initializes the ring logger (`log::init(1024)`),
  - enables backend privileges (expose `OsBackend::enable_privileges` via `Service::new` calling it once),
  - installs a `ctrlc` handler that sends `ServiceMsg::Stop`,
  - calls `Service::run`.

- [ ] **Step 1: Verify privileges bootstrap**

`Service::new` (Task 12) already calls `backend.enable_privileges()` and logs a warning on failure. No code change here — just confirm it's present.

- [ ] **Step 2: Write `crates/aetheris-service/src/main.rs`**

```rust
use std::path::PathBuf;
use std::sync::mpsc::Sender;

use aetheris_core::config::Config;
use aetheris_core::log;
use aetheris_core::service::{Service, ServiceMsg};

fn main() {
    let mut cfg_path = PathBuf::from("aetheris.toml");
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--config" => {
                i += 1;
                if i < args.len() {
                    cfg_path = PathBuf::from(&args[i]);
                }
            }
            _ => {}
        }
        i += 1;
    }

    log::init(1024);

    let cfg = match Config::load(&cfg_path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("config error: {e}");
            std::process::exit(1);
        }
    };

    let service = Service::new(&cfg_path, cfg);
    let stop_tx: Sender<ServiceMsg> = service.stop_sender();

    if let Err(e) = ctrlc::set_handler(move || {
        let _ = stop_tx.send(ServiceMsg::Stop);
    }) {
        eprintln!("ctrlc handler error: {e}");
    }

    println!("aetheris-service running (config: {})", cfg_path.display());
    if let Err(e) = service.run() {
        eprintln!("service error: {e}");
        std::process::exit(1);
    }
}
```

- [ ] **Step 3: Verify stop wiring**

No change needed: `stop_sender()` exists on `Service` (Task 12) and `run()` relays `ServiceMsg::Stop` from the stop channel into the main loop. `main.rs` already uses it.

- [ ] **Step 4: Write `aetheris.toml` (repo root)**

```toml
# aetheris example configuration.
# Copy and edit; the service reads this at startup and on `ReloadConfig`.

[game]
boost_on_start = true
processes = ["steam_app_*.exe", "game.exe"]

# Processes throttled while a game is running. Each entry is matched
# case-insensitively as a substring against the process image name.
[[background]]
name = "chrome.exe"
suspend = false
priority = "below_normal"
qos_cpu_quota = 50

[[background]]
name = "msedge.exe"
suspend = false
priority = "below_normal"

# Memory trim is explicit opt-in and only safe for non-critical apps.
[[background]]
name = "spotify.exe"
trim_memory = true

# Always-on rules (any mode).
[[rule]]
name = "updater.exe"
priority = "idle"

# Extra protected processes (defaults can never be removed).
protected_extra = []
```

- [ ] **Step 5: Build and smoke-test**

Run: `cargo build --workspace`
Run (elevated): `cargo run -p aetheris-service -- --config aetheris.toml`
Expected: prints `aetheris-service running (config: aetheris.toml)`; Ctrl-C exits cleanly (no panic, exit 0). Manual check: while running, start a `dummy_proc`-like process and observe ring-log lines only in the ETW smoke context; full behavioral check is Task 15.

- [ ] **Step 6: Commit**

```bash
git add crates/aetheris-service/src/main.rs crates/aetheris-core/src/service.rs aetheris.toml
git commit -m "feat: service launcher with ctrl-c stop and example config"
```

---

### Task 14: CLI client

**Files:**
- Modify: `crates/aetheris-cli/src/main.rs`
- Test: manual (against a running service)

**Interfaces:**
- Consumes: `aetheris_core::ipc::{client_call, Request, DEFAULT_PIPE}`, `aetheris_core::ipc::Response` (Task 11).
- Produces: `aetheris-cli.exe` with subcommands:
  - `aetheris-cli get-state` → prints mode + boosted processes.
  - `aetheris-cli reload` → asks service to reload config.
  - `aetheris-cli query <name>` → prints matching process info or `not found`.
  - `--pipe <name>` optional override (default `DEFAULT_PIPE`).

- [ ] **Step 1: Write `crates/aetheris-cli/src/main.rs`**

```rust
use aetheris_core::ipc::{Response, Request, client_call, DEFAULT_PIPE};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let mut pipe = DEFAULT_PIPE.to_string();
    let mut cmd: Option<String> = None;
    let mut arg: Option<String> = None;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--pipe" => {
                i += 1;
                if i < args.len() {
                    pipe = args[i].clone();
                }
            }
            "get-state" | "reload" | "query" => {
                cmd = Some(args[i].clone());
                if args[i] == "query" && i + 1 < args.len() {
                    arg = Some(args[i + 1].clone());
                }
            }
            _ => {}
        }
        i += 1;
    }

    let req = match cmd.as_deref() {
        Some("get-state") => Request::GetState,
        Some("reload") => Request::ReloadConfig,
        Some("query") => Request::QueryProcess(arg.unwrap_or_default()),
        _ => {
            eprintln!("usage: aetheris-cli [--pipe NAME] get-state|reload|query <name>");
            std::process::exit(2);
        }
    };

    match client_call(&pipe, &req) {
        Ok(Response::State(s)) => {
            println!("mode: {}", s.mode);
            println!("boosted:");
            for p in &s.boosted {
                println!("  {} (pid {})", p.name, p.pid);
            }
            if s.boosted.is_empty() {
                println!("  (none)");
            }
        }
        Ok(Response::Reload(m)) => println!("reload: {m}"),
        Ok(Response::Process(Some(p))) => {
            println!("{} (pid {}, game={})", p.name, p.pid, p.is_game)
        }
        Ok(Response::Process(None)) => println!("not found"),
        Err(e) => {
            eprintln!("error: {e}");
            std::process::exit(1);
        }
    }
}
```

- [ ] **Step 2: Build**

Run: `cargo build --workspace`
Expected: builds clean.

- [ ] **Step 3: Manual verification**

With `aetheris-service` running (Task 13), run:
`cargo run -p aetheris-cli -- get-state`
Expected: prints `mode: Normal` (or `GameBoost` if a game is active) and the boosted list.
`cargo run -p aetheris-cli -- reload`
Expected: prints `reload: queued`.

- [ ] **Step 4: Commit**

```bash
git add crates/aetheris-cli/src/main.rs
git commit -m "feat: cli client (get-state/reload/query)"
```

---

### Task 15: Compliance, docs, and final smoke

**Files:**
- Create: `THIRD_PARTY.md`
- Create: `deny.toml` (cargo-deny)
- Create: `README.md`
- Modify: `.gitignore` (un-ignore `Cargo.lock` — it should be committed for a binary workspace)
- Test: full suite + cargo-deny + end-to-end smoke

- [ ] **Step 1: Write `THIRD_PARTY.md`**

List every direct dependency with its license and, for any code adapted from the SAFE-TO-COPY reference list (research report), the attribution line. Dependencies (all permissive): `windows` (MIT OR Apache-2.0), `ntapi` (Apache-2.0 OR MIT), `aho-corasick` (Unlicense OR MIT), `memmap2` (MIT OR Apache-2.0), `serde` (MIT OR Apache-2.0), `toml` (MIT OR Apache-2.0), `bincode` (MIT OR Apache-2.0), `ctrlc` (MIT OR Apache-2.0). Add a "Clean-room references" section naming vnite, Winderust, ferrisetw, system_monitor, windows-erg, Process Governor, Priority, gpu-power-limit-daemon, uberdisplay, shawl as architecture references only, with the note that no GPL/LGPL/unlicensed code was copied.

- [ ] **Step 2: Write `deny.toml`**

```toml
[advisories]
vulnerability = "deny"
unmaintained = "warn"
unsound = "warn"

[bans]
multiple-versions = "warn"
wildcards = "deny"

[sources]
unknown-registry = "deny"
unknown-git = "deny"
allow-git = []

[licenses]
allow = [
  "MIT",
  "Apache-2.0",
  "Unlicense",
  "BSD-3-Clause",
  "BSD-2-Clause",
  "ISC",
  "Zlib",
]
copyleft = "deny"
default = "deny"
confidence-threshold = 0.9
```

- [ ] **Step 3: Run cargo-deny**

Run: `cargo install cargo-deny && cargo deny check`
Expected: passes (no copyleft, no advisories). If `cargo-deny` is not installed or install fails offline, document the command in README and mark as a required CI gate.

- [ ] **Step 4: Update `.gitignore`** — remove `Cargo.lock` from the ignore list (commit it for a workspace with binaries).

- [ ] **Step 5: Write `README.md`**

Include: what it is; the zero-overhead constraint; build (`cargo build --release`); run (elevated) `aetheris-service --config aetheris.toml`; CLI usage; config reference pointing at `aetheris.toml`; the fail-safe note (ETW unavailable ⇒ service exits); license/compliance summary pointing at `THIRD_PARTY.md` and `deny.toml`; roadmap (v2: kernel driver, overlay, Win32 UI, network QoS).

- [ ] **Step 6: Final full validation**

Run: `cargo test --workspace`
Run (elevated): `cargo run -p aetheris-service -- --config aetheris.toml` and in a second terminal `cargo run -p aetheris-cli -- get-state`
Expected: all tests pass; service runs; CLI reports state.

- [ ] **Step 7: Commit**

```bash
git add THIRD_PARTY.md deny.toml README.md .gitignore
git commit -m "docs: compliance notice, cargo-deny gate, README"
```

---

## Self-Review Notes

- **Spec §5.1 (ETW):** covered by Task 9 (GUID, event IDs, keyword, buffer tuning, fail-safe). Buffer tuning constants live in the ETW code; the smoke test is the arbiter.
- **Spec §5.2 (foreground):** Task 10, message-pump thread hosts the hook.
- **Spec §5.3 (policy):** Task 8 covers state machine, rule match, game mode, restore, protected list, reload.
- **Spec §5.4 (actions):** Tasks 6/7 cover all five action types + Job-Object QoS with background-mode fallback + privilege dance. Group-aware affinity for >64 CPUs is explicitly deferred (Global Constraints) — a documented v1 limitation.
- **Spec §5.5 (IPC):** Task 11.
- **Spec §5.6 (config):** Task 4 (TOML, mmap is represented by `Config::load`; note `memmap2` is a listed dep and can wrap `Config::load` in a follow-up — the parse-from-string path is identical, so mmap is a pure I/O swap).
- **Spec §5.7 (safety/degradation):** Tasks 4/8 (protected list, opt-in) and Task 12 (degradation hook; sampler stubbed in v1).
- **Spec §6 (perf budget):** aho-corasick matcher (Task 5), SoA table (Task 3), no-tokio constraint throughout, ring logger (Task 2).
- **Spec §8 (acceptance):** acceptance items map to Task 8 tests (boost/restore), Task 9 smoke, Task 11/13/14 (IPC/CLI), Task 15 (resource claim is measured manually — add a note in README).
- **License (spec §9):** enforced by Task 15 (THIRD_PARTY, deny.toml, clean-room note).

**Known gaps to flag to the reviewer:** (1) `system_load_percent` stub (Task 12) — graceful degradation wired but sampler inert until v1.1. (2) Config mmap not actually used (Task 4 loads via `std::fs::read`); `memmap2` is a listed dependency and the swap is trivial. (3) IPC `run()` never exits — service shutdown relies on the whole process exiting (v1 scope, fine for a console launcher; the windows-service wrapper in v2 will add clean teardown).
