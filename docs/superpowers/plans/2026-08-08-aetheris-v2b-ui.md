# aetheris v2-B Configuration UI Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A non-resident Win32 configuration dialog (`aetheris-ui`) that shows live status and edits the rules, saving back to `aetheris.toml` and reloading the service.

**Architecture:** `aetheris-core` gains two IPC messages (`GetConfig` → `Response::Config`, `SaveConfig(Config)` → service writes the file + `set_config` reload). A new `aetheris-ui` crate is a plain Win32 programmatic dialog (no `.rc`, no GUI framework) that talks to the running service over the named pipe.

**Tech Stack:** Rust, windows 0.62.2 (UI features: `Win32_UI_WindowsAndMessaging`, `Win32_UI_Controls`, `Win32_UI_Input_KeyboardAndMouse`, `Win32_Graphics_Gdi`), bincode/serde, existing `aetheris-core::ipc`.

## Global Constraints

- **No async runtime.** `aetheris-ui` is a single-threaded Win32 message pump; IPC calls are synchronous on button actions.
- **Non-resident:** the UI process exits on window close; it holds no state the service needs.
- **Service-side IPC is the testable core:** `GetConfig`/`SaveConfig` get unit/integration tests in `aetheris-core`; the dialog itself is manual-verified.
- **SaveConfig security:** the service writes the config file it already owns (`cfg_path`), then `set_config` reloads. Validate the incoming `Config` via the existing `Config::validate` before persisting; reject invalid configs with a clear error, never write a broken file.
- **Config never lost:** SaveConfig writes to a temp file then renames over `cfg_path` (atomic-ish), so a crash mid-save can't truncate the user's config.
- Dependencies locked; no new external deps (windows features only). cargo-deny green.
- Every task ends green + committed.

---

### Task 1: IPC GetConfig / SaveConfig (core, testable)

**Files:**
- Modify: `crates/aetheris-core/src/ipc.rs` (Request/Response variants)
- Modify: `crates/aetheris-core/src/service.rs` (handle both in the IPC thread; SaveConfig routes to the main loop like Reload)
- Modify: `crates/aetheris-core/tests/ipc_roundtrip.rs`
- Test: integration

**Interfaces:**
- Consumes: `Request`, `Response`, `Config`, `ServiceMsg`.
- Produces:
  - `Request::GetConfig` → `Response::Config(Config)`.
  - `Request::SaveConfig(Config)` → the service validates, writes `cfg_path` (temp + rename), reloads via `set_config`, and returns `Response::SaveConfig(Result<String, String>)` (Ok = "saved", Err = validation/io error; invalid config is NOT persisted).
  - `ServiceMsg::SaveConfig(Config)` new variant: main loop handles it (validate → persist → `engine.set_config`); the IPC thread sends it and waits for the result via a `oneshot`-style channel (`std::sync::mpsc::Sender<Result<...>>` carried in the message).

- [ ] **Step 1: Write the failing test** (`tests/ipc_roundtrip.rs` + a new `tests/service_saveconfig.rs`):

```rust
// tests/service_saveconfig.rs — exercises the real service message path.
#[test]
fn save_config_validates_then_persists() {
    let dir = std::env::temp_dir();
    let path = dir.join(format!("aetheris_savecfg_{}.toml", std::process::id()));
    std::fs::write(&path, "[game]\nboost_on_start=true\nprocesses=[\"game.exe\"]\n").unwrap();

    let (mut svc, _state) = Service::new(&path, Config::load(&path).unwrap());

    // Valid new config: persists + reloads.
    let good = Config::from_str("[game]\nboost_on_start=true\nprocesses=[\"g.exe\"]\n[[background]]\nname=\"b.exe\"\nsuspend=true\n").unwrap();
    let res = svc.handle_message(&ServiceMsg::SaveConfig(good.clone()));
    assert!(res.is_ok());
    assert_eq!(Config::load(&path).unwrap().game.processes, vec!["g.exe".to_string()]);
    assert_eq!(svc.current_state().mode, "Normal");

    // Invalid config (qos 150): rejected, file unchanged.
    let bad = Config::from_str("[game]\nprocesses=[]\n[[background]]\nname=\"x.exe\"\nqos_cpu_quota=150\n").unwrap();
    let res = svc.handle_message(&ServiceMsg::SaveConfig(bad));
    assert!(res.is_err(), "invalid config must be rejected");
    assert_eq!(Config::load(&path).unwrap().game.processes, vec!["g.exe".to_string()]);

    let _ = std::fs::remove_file(&path);
}
```

- [ ] **Step 2: Run test to verify it fails**

Run: `cargo test -p aetheris-core --test service_saveconfig`
Expected: compile error — `ServiceMsg::SaveConfig` missing.

- [ ] **Step 3: Implement**

`ipc.rs`:
```rust
pub enum Request {
    GetState,
    GetConfig,
    ReloadConfig,
    SaveConfig(Config),
    QueryProcess(String),
}
pub enum Response {
    State(StateSnapshot),
    Config(Config),
    Reload(String),
    SaveConfig(Result<String, String>),
    Process(Option<ProcessInfo>),
}
```
`Config` already derives `Serialize, Deserialize` — confirm and add to ipc.rs imports.

`service.rs`:
```rust
pub enum ServiceMsg {
    Proc(ProcessEvent),
    Foreground(ForegroundEvent),
    Reload,
    SaveConfig { cfg: Config, reply: Sender<Result<String, String>> },
    Stop,
}
```
In `handle_message`:
```rust
ServiceMsg::SaveConfig { cfg, reply } => {
    let res = self.persist_config(&cfg);
    let _ = reply.send(res);
    Ok(())
}
```
where `persist_config` validates, writes temp + rename, then `engine.set_config(cfg)`:

```rust
fn persist_config(&self, cfg: &Config) -> Result<String, String> {
    cfg.validate().map_err(|e| e.to_string())?;
    let dir = self.cfg_path.parent().unwrap_or(std::path::Path::new("."));
    let tmp = dir.join(format!(".aetheris.toml.tmp{}", std::process::id()));
    std::fs::write(&tmp, toml::to_string(cfg).map_err(|e| e.to_string())?)
        .map_err(|e| e.to_string())?;
    std::fs::rename(&tmp, &self.cfg_path).map_err(|e| e.to_string())?;
    Ok("saved".into())
}
```
But `persist_config` takes `&self` while `set_config` needs `&mut self.engine`. Restructure: do the file write first (read-only on self), then mutate engine:
```rust
fn persist_config(&mut self, cfg: &Config) -> Result<String, String> {
    cfg.validate().map_err(|e| e.to_string())?;
    let dir = ...;
    let tmp = ...;
    std::fs::write(&tmp, toml::to_string(cfg)...)...;
    std::fs::rename(&tmp, &self.cfg_path)...;
    self.engine.set_config(cfg.clone());
    Ok("saved".into())
}
```

IPC thread in `run()`:
```rust
Request::GetConfig => {
    let cfg = state.read().unwrap().config.clone(); // add `config: Config` to StateSnapshot? 
    Response::Config(cfg)
}
```
> Add `config: Config` to `StateSnapshot` (cloned on refresh). `Config` needs `Clone` (it has it). This makes GetConfig serve the live config without a separate roundtrip.

```rust
Request::SaveConfig(cfg) => {
    let (tx, rx) = channel();
    let _ = ipc_tx.send(ServiceMsg::SaveConfig { cfg, reply: tx });
    match rx.recv() {
        Ok(res) => Response::SaveConfig(res),
        Err(_) => Response::SaveConfig(Err("service unavailable".into())),
    }
}
```

- [ ] **Step 4: Run tests**

Run: `cargo test -p aetheris-core` (all pass), clippy clean (except pre-existing config.rs:107).

- [ ] **Step 5: Commit**

```bash
git add crates/aetheris-core/src/ipc.rs crates/aetheris-core/src/service.rs crates/aetheris-core/tests/service_saveconfig.rs crates/aetheris-core/tests/ipc_roundtrip.rs
git commit -m "feat: IPC GetConfig/SaveConfig with validation + atomic persist"
```

---

### Task 2: aetheris-ui crate scaffold + dialog shell

**Files:**
- Create: `crates/aetheris-ui/Cargo.toml`
- Create: `crates/aetheris-ui/src/main.rs`
- Modify: root `Cargo.toml` (add workspace member)
- Test: build + manual launch

**Interfaces:**
- Consumes: `aetheris-core::ipc::{client_call, Request, Response, DEFAULT_PIPE}`.
- Produces: a window that opens, shows a title/status line, and can be closed cleanly (exit 0). Placeholder content; real panels in Task 3.

- [ ] **Step 1: Write `Cargo.toml`**

```toml
[package]
name = "aetheris-ui"
version.workspace = true
edition.workspace = true
license.workspace = true

[dependencies]
aetheris-core = { path = "../aetheris-core", version = "1.0.0" }
windows = { version = "0.62", features = [
  "Win32_Foundation",
  "Win32_UI_WindowsAndMessaging",
  "Win32_UI_Controls",
  "Win32_UI_Input_KeyboardAndMouse",
  "Win32_Graphics_Gdi",
  "Win32_System_LibraryLoader",
] }

[profile.release]
inherits = "release"  # remove if workspace already provides it
```

> Note: `[profile.release]` in a member is ignored (v1 lesson) — DO NOT add it here; the workspace-root profile applies.

- [ ] **Step 2: Write a minimal dialog shell in `main.rs`** — register a window class, create the main window, run the message loop, handle `WM_DESTROY` → `PostQuitMessage`. Parse `--pipe <name>` (default `DEFAULT_PIPE`). Window title "aetheris". Show the pipe name in a status static.

This is a complete Win32 programmatic-dialog skeleton (window class + CreateWindowExW + standard controls created as children). Implement it so it compiles and opens an empty window; real panels land in Task 3.

- [ ] **Step 3: Add workspace member** to root `Cargo.toml`:

```toml
members = ["crates/aetheris-core", "crates/aetheris-service", "crates/aetheris-cli", "crates/aetheris-ui"]
```

- [ ] **Step 4: Build + manual smoke**

Run: `cargo build --workspace`; then `cargo run -p aetheris-ui` — a window titled "aetheris" opens; closing it exits cleanly (exit 0).

- [ ] **Step 5: Commit**

```bash
git add crates/aetheris-ui/Cargo.toml crates/aetheris-ui/src/main.rs Cargo.toml
git commit -m "feat: aetheris-ui dialog shell (Win32 programmatic)"
```

---

### Task 3: Status panel + rule editor + save flow

**Files:**
- Modify: `crates/aetheris-ui/src/main.rs`
- Test: manual verification (documented); core IPC paths already tested in Task 1

**Interfaces:**
- Consumes: `client_call`, `Request::{GetState, GetConfig, SaveConfig, ReloadConfig}`, `Response::{State, Config, SaveConfig, Reload}`, `Config`/`StateSnapshot`/`ProcessInfo` types.
- Produces: a working dialog with:
  - **Status panel** (top): mode, boosted-process count/names, last-reload result. A "Refresh" button re-pulls `GetState`.
  - **Rule editor** (middle): three lists (game processes, `[[background]]`, `[[rule]]`). For background/rule, fields: name, priority (combo), affinity (text "0,1"), qos_cpu_quota, suspend/trim (checkboxes). Add/Edit/Delete buttons mutate a local `Config` copy.
  - **Save / Reload / Exit** buttons (bottom): Save → `SaveConfig(local)`, show result; Reload → `ReloadConfig`; Exit → close.
  - Load once from `GetConfig` on startup; Refresh only re-pulls status.

**Implementation approach for the rule editor (keep it simple):**
- One `Edit` control per editable rule field, populated from the selected list row; "Apply" writes the edit controls back to the selected row. Add appends a new row. Delete removes the selected row.
- Lists are `ListView` (`SysListView32`) with columns. Manage row→rule-index mapping in the dialog proc's state (a `Vec<u32>` of "index into cfg.background/rule" or just display order).
- Dialogs: create a modal modeless window (WS_OVERLAPPEDWINDOW) with child controls; handle WM_COMMAND (button IDs), WM_NOTIFY (list selection), WM_CTLCOLOR* not needed.

This is a substantial Win32 UI task. Write it incrementally; a compile+manual-open + a documented manual test checklist is the deliverable. Keep controls programmatic (no resource IDs needed — use defines).

- [ ] **Step 1: Implement the dialog proc with the status panel + rule lists + save flow** (full code in `main.rs`). Wire the pipe via the `--pipe` arg.
- [ ] **Step 2: Build** — `cargo build --workspace` green.
- [ ] **Step 3: Manual verification (document in the report):** with the service running elevated, launch `aetheris-ui`:
  1. Status shows the live mode + boosted list after Refresh.
  2. Edit a background rule's name → Save → service reloads (`get-state` reflects it; the config file content changed).
  3. Enter an invalid config (qos 150) → Save → error shown, file unchanged.
  4. Close → process exits (no orphan).
- [ ] **Step 4: Commit**

```bash
git add crates/aetheris-ui/src/main.rs
git commit -m "feat: aetheris-ui status panel + rule editor + save flow"
```

---

### Task 4: v2-B integration + docs

**Files:**
- Modify: `README.md` (document `aetheris-ui` usage + SaveConfig IPC)
- Modify: `aetheris.toml` (no change needed — UI edits it)
- Test: full suite + cargo-deny

**Interfaces:**
- Consumes: everything.
- Produces: documented, shipped UI.

- [ ] **Step 1: README** — add a "Configuration UI" section: `aetheris-ui [--pipe NAME]`, launch on demand (optionally via `aetheris-cli ui` if a command is added — skip for now), what it shows/edits, and the save→reload flow.
- [ ] **Step 2: Full validation**

Run: `cargo test --workspace`, `cargo deny check licenses bans sources` (network permitting).

- [ ] **Step 3: Commit**

```bash
git add README.md
git commit -m "docs: aetheris-ui usage"
```

---

## Self-Review Notes

- v2 spec §4 (UI): Tasks 1-3 cover IPC + dialog + editor; Task 4 docs.
- Testability: the service-side IPC (the risky part) is fully integration-tested; the dialog is manual (Win32 GUI has no headless test in this stack).
- Known simplifications: rule editing is single-selection (no multi-select); priority is a combo with the 6 enum values; affinity text "0,1" parsed to `cores`; no live-refresh timer (manual Refresh button only) — matching the non-resident, low-effort design.
