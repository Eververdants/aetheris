# aetheris v2-C External Telemetry Overlay Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** A DirectComposition external overlay (`aetheris-overlay`) that shows live aetheris telemetry (mode, boosted processes, system load) over the screen, activated on demand, using zero injection and no game-process hooks.

**Architecture:** A new `aetheris-overlay` crate: an invisible/hidden DirectComposition visual tree composited by DWM, rendering telemetry text via DWrite, polling the service's `GetState` at ~1 Hz over the named pipe. Hotkey/CLI-launch to show, close to hide (process exits). Separate process — never touches the game.

**Tech Stack:** Rust, windows 0.62.2 (`Win32_Graphics_Direct3D11`, `Win32_Graphics_DirectComposition`, `Win32_Graphics_DirectWrite`, `Win32_Graphics_Dxgi`, `Win32_UI_WindowsAndMessaging`, `Win32_Foundation`), existing `aetheris-core::ipc`. Reference (clean-room, MIT): dcomp-overlay pattern.

## Global Constraints

- **No async runtime.** Overlay is a single-threaded DWM-composited render loop: ~1 Hz telemetry poll → render text.
- **Zero injection.** No DLL, no Present hook, no game-process handle. DirectComposition visual only (anti-cheat-safe).
- **Resource discipline:** GPU/DXGI/DWrite resources are created on show and released on close; the overlay process exits when hidden. No resident background cost.
- **Budget:** the overlay process, while showing, should stay small (tens of MB private worst case); idle-hidden = zero (process gone).
- **IPC is read-only:** only `GetState` (and optionally `GetConfig` for a settings line). Never SaveConfig/Reload.
- Dependencies locked; cargo-deny green.
- Every task ends green + committed.

---

### Task 1: aetheris-overlay crate scaffold + D3D11/DComp/DWrite init + text render

**Files:**
- Create: `crates/aetheris-overlay/Cargo.toml`
- Create: `crates/aetheris-overlay/src/main.rs`
- Modify: root `Cargo.toml` (workspace member)
- Test: build + manual open (a window/visual appears with placeholder text)

**Interfaces:**
- Consumes: `aetheris_core::ipc::{client_call, Request, DEFAULT_PIPE}` (next task).
- Produces:
  - A process that initializes: `CreateDXGIFactory2` → D3D11 device → a small swapchain (e.g. 800×120 top-left) → `CreateDirectCompositionDevice` → a `IDCompositionVisual` with the swapchain as content → `SetRoot` + `Commit`; renders a placeholder telemetry line via DWrite into the swapchain's back buffer. Message loop on the window. Clean exit on WM_DESTROY/ESC.
  - `--pipe <name>` arg (default `DEFAULT_PIPE`).

**Implementation notes (dcomp-overlay + DirectCompositionDirectX12Sample patterns, clean-room):**
- Hidden top-most window (`WS_EX_TOPMOST|WS_EX_NOACTIVATE|WS_EX_TRANSPARENT|WS_EX_LAYERED`, `WS_POPUP`, no visible region needed — DComp visual supplies pixels). Per-pixel transparency comes from the DComp visual + a `DXGI_ALPHA_MODE_PREMULTIPLIED` swapchain.
- Render loop: `GetMessageW`/`PeekMessageW` with a ~16 ms render tick (or render-on-demand when text changes). For v2-C v1, render once per telemetry update (1 Hz) is fine — no continuous loop needed.
- DWrite: `DWriteCreateFactory` → `IDWriteTextFormat` + `IDWriteTextLayout` → render into the swapchain back buffer via `ID2D1DeviceContext` (D2D over DXGI surface) or direct CPU rasterization. **Prefer D2D** (`ID2D1Factory`, `ID2D1DCRenderTarget`/`ID2D1DeviceContext` attached to the DXGI back buffer) — it's the standard text path. Features needed: `Win32_Graphics_Direct2D`.
- First validate the minimal DComp pipeline with a solid-color swapchain; then add text in the same task (text is the actual deliverable).
- Cross-check all signatures against the vendored `windows-0.62.2` crate; expect COM-object churn (interfaces via `windows::core::Interface`). Build incrementally.

**Verify:** `cargo build --workspace` green. Manual: `cargo run -p aetheris-overlay` shows a small semi-transparent panel with a placeholder line near the top-left; ESC/close exits cleanly (exit 0). Report what you observed.

**Commit:** `git add crates/aetheris-overlay/Cargo.toml crates/aetheris-overlay/src/main.rs Cargo.toml && git commit -m "feat: aetheris-overlay D3D11/DComp/DWrite text panel"`.

Write report to `D:\Eververdants\Projects\Code\aetheris\.superpowers\sdd\task-1-report.md`. Report back: Status, commits, one-line summary, concerns, report path.

---

### Task 2: Live telemetry — poll GetState, render mode/boosted/load

**Files:**
- Modify: `crates/aetheris-overlay/src/main.rs`
- Test: manual (with the service running elevated)

**Interfaces:**
- Consumes: `client_call`, `Request::GetState`, `Response::State(StateSnapshot)`, `Response`.
- Produces: the overlay renders, at ~1 Hz:
  - `Mode: Normal | GameBoost`
  - Boosted process list (names + pids)
  - `Last reload:` result (from `StateSnapshot.last_reload`)
  - The pipe name (for debugging)
  - A `--pipe` parse (already done in Task 1)
- The render loop becomes: every ~1 s (deadline-wait on the message queue with a timer), `client_call(GetState)` → format lines → render. On IPC failure, render `service unavailable` and keep trying (the overlay is a diagnostic surface; don't crash).

**Verify:** `cargo build --workspace` green; with the service running elevated + a game-mode active, the overlay shows the live mode + boosted processes, updating as state changes. Manual.

**Commit:** `git add crates/aetheris-overlay/src/main.rs && git commit -m "feat: overlay live telemetry (1 Hz GetState)"`.

Write report to `D:\Eververdants\Projects\Code\aetheris\.superpowers\sdd\task-2-report.md`. Report back: Status, commits, one-line summary, concerns, report path.

---

### Task 3: Hotkey activation + docs

**Files:**
- Modify: `crates/aetheris-core/src/service.rs` (register a global hotkey; on trigger, launch the overlay via `CreateProcessW` — or simpler: document that the user launches `aetheris-overlay` directly; decide based on what's cleanest)
- Modify: `crates/aetheris-service/src/main.rs` (if hotkey handled in service)
- Modify: `README.md`
- Test: manual + docs

**Interfaces:**
- Consumes: nothing new (hotkey via `RegisterHotKey` if implemented; overlay path already known).
- Produces:
  - **Decision point:** the v2 spec says "service receives hotkey → CreateProcess overlay". For v2-C v1, the simplest correct path is to let the USER launch `aetheris-overlay` (hotkey can be a v2.1 nicety). If a hotkey is cheap (a `RegisterHotKey` in the service's foreground thread + a CLI `--overlay` toggle), do it; otherwise document manual launch. **Recommend: manual launch in v2-C** (README documents it), hotkey as a documented follow-up — keeps scope tight and the overlay fully usable now.
- README: "Overlay" section — `aetheris-overlay [--pipe NAME]`, what it shows, anti-cheat-safe (no injection), resource behavior (exits on close).

**Verify:** `cargo test --workspace` green, `cargo deny check licenses bans sources` (network permitting).

**Commit:** `git add README.md && git commit -m "docs: aetheris-overlay usage"`.

Write report to `D:\Eververdants\Projects\Code\aetheris\.superpowers\sdd\task-3-report.md`. Report back: Status, commits, one-line summary, concerns, report path.

---

## Self-Review Notes

- v2 spec §5 (overlay): Tasks 1-3 cover DComp+DWrite render, live telemetry, and activation/docs.
- Scope decision recorded: hotkey activation deferred to v2.1 (manual launch documented). This is a deliberate scope trim that keeps v2-C shippable; the v2 spec §5 hotkey language is amended by this plan.
- Testability: the overlay is DWM-composited GUI — manual verification is the deliverable (consistent with the UI slice). The service-side IPC it consumes is already tested.
