# aetheris

A zero-overhead Windows game-optimization service written in Rust. When a game
is in the foreground, aetheris throttles background processes (priority,
affinity, CPU-quota, suspend, memory trim) so the game gets the machine to
itself — then restores everything when the game exits.

## What it is

- **Native, headless, event-driven.** No async runtime, no polling loops. ETW
  `Microsoft-Windows-Kernel-Process` events and a `SetWinEventHook` foreground
  watcher drive a single-threaded policy engine.
- **Policy engine.** A state machine (Normal ↔ GameBoost) applies per-process
  rules compiled into an Aho-Corasick automaton, with an always-on protected
  list (`csrss.exe`, `services.exe`, `dwm.exe`, itself, ...) that no rule can
  override.
- **One binary, four faces.** `aetheris` is a single executable: `aetheris`
  (no args) opens the UI + tray, `aetheris service` is the headless engine,
  `aetheris overlay` is the telemetry panel, and `aetheris cli ...` is the
  command line — all talking over `\\.\pipe\aetheris`.
- **Fail-safe.** No kernel trace session is required to be present: if ETW is
  unavailable the service does **no** optimization and **exits** with a clear
  error. It never silently degrades to polling.

## Zero-overhead constraint

aetheris is built to cost nothing while idle:

| Metric | Target |
|---|---|
| Steady-state memory | 2–5 MB |
| Steady-state CPU | < 0.1 % (≈0 idle) |
| Cold start | < 100 ms |
| Game lifecycle response | event-driven, zero polling |

The hot path (event handling) performs zero heap allocation after init: the
rule automaton, a structure-of-arrays process table, and a ring-buffer log are
all pre-allocated. The service must be run **elevated** (admin) to open the
kernel ETW session and to take the `SeDebugPrivilege` / `SeIncreaseBasePriorityPrivilege`
tokens the actions need. Run it elevated or it will fail closed — by design.

## Build

```sh
cargo build --release
```

This produces a **single** binary, `target\release\aetheris.exe`, that carries
every face of the app (UI + tray, service, overlay, CLI — see Usage below).
The release profile uses thin LTO + strip + abort-on-panic (`Cargo.toml`);
`Cargo.lock` is committed; this is a binary workspace. Validate with
`cargo test --workspace` and `cargo deny check licenses bans sources` (the
cargo-deny gate is described under License & compliance).

## Usage

aetheris is **one executable**, `aetheris.exe`. The same binary is the UI + tray,
the headless engine, the telemetry panel, and the command line:

```sh
aetheris                 # open the configuration UI + tray icon (also: double-click)
aetheris service         # headless engine (elevated process)
aetheris overlay         # DirectComposition telemetry panel (hotkey / tray-launched)
aetheris cli ...         # one-shot pipe commands, see below
aetheris --version       # print aetheris <version>
```

### `aetheris` — UI + tray (no arguments)

Double-clicking `aetheris.exe` (or running `aetheris` with no arguments) opens
the configuration dialog and installs a resident tray icon. On the **first run**
it notices that no service is running, prompts once via UAC to start
`aetheris service` elevated, and auto-opens the dialog once the service answers
(see First run below).

The UI is **resident**: closing the dialog minimizes it to the tray instead of
exiting, so the tray keeps controlling the service. Right-click the tray icon
for the menu:

- **Start service** — UAC-launch `aetheris service` if it is not running.
- **Stop service** — ask the service to stop (it restores every boosted process
  and the elevated process exits).
- **Toggle overlay** — launch, or close, the telemetry panel.
- **Open UI** — restore the dialog window.
- **Exit** — quit the UI *only*; the service keeps running.

The tray icon shows the service status — green when it answers `get-state`,
gray when it is down (probed every 5 s).

### `aetheris service` — the headless engine

The engine must run **elevated** to open the kernel ETW session and to take the
`SeDebugPrivilege` / `SeIncreaseBasePriorityPrivilege` tokens the actions need.
Run it elevated or it fails closed — by design:

```sh
aetheris service                          # default config: %PROGRAMDATA%\aetheris\aetheris.toml
aetheris service --config path\to.toml    # custom config location
```

The service prints a startup banner, then runs until stopped (Ctrl-C, `aetheris cli stop`,
or the tray's "Stop service"). When it stops, boosted processes are restored. If
`EtwMonitor::start()` fails — e.g. not elevated, or the kernel trace session cannot
be opened — the service prints `service error: StartTraceW failed: status ...` and
exits with code 1. That is the intended fail-safe.

### First run

On the first run the service auto-creates the machine-wide config at
`%PROGRAMDATA%\aetheris\aetheris.toml` — a fully commented template, so nothing
is active until you edit it. The repo's committed `aetheris.toml` is exactly that
template. The service is elevated, so it can write the admin-owned `ProgramData`
file; the non-elevated UI and CLI cannot.

Saving the config writes to that admin-owned file, so **Save requires an
elevated UI**. If the UI is not elevated and the service refuses a save for lack
of elevation, the UI prompts once to relaunch `aetheris ui` as administrator
(UAC); the elevated instance takes over and completes the save. The old instance
quits (exit 0).

## CLI usage

All one-shot, against `\\.\pipe\aetheris`. The CLI reaches the elevated service
through a pipe DACL that grants Interactive Users access, so it does **not**
need to be elevated itself:

```sh
aetheris cli get-state            # print current mode + boosted list
aetheris cli reload               # reload the config file from disk
aetheris cli query <name>         # query one process (name / pid / is_game)
aetheris cli stop                 # stop the service (restores boosted processes)
aetheris cli toggle-overlay       # launch / close the telemetry panel
aetheris cli --pipe <NAME> ...    # override the pipe name
```

Example output:

```
mode: GameBoost
boosted:
  chrome.exe (pid 1234)
```

## Configuration UI

The dialog is the `aetheris` / `aetheris ui` mode — a programmatic Win32 dialog
(no `.rc`, no GUI framework — the window, controls, and lists are all built
programmatically) that reads and edits the running service's config over the
same named pipe as the CLI. It is **resident**: launch it (`aetheris`, or
`aetheris ui`), and closing the dialog minimizes it to the tray — the UI and its
tray icon stay running to control the service (see Usage above).

```sh
aetheris                # same as no-argument; against the default pipe \\.\pipe\aetheris
aetheris ui --pipe <NAME>  # against a specific pipe (match the service's --pipe)
```

The dialog is three parts:

- **Status panel** (top): the current mode (`Normal` / `GameBoost`), the boosted
  process list, and the last-reload result. **Refresh** re-pulls live status via
  `GetState`; status is also pulled once at startup.
- **Rule editor** (middle): three lists — game processes (`[game] processes`),
  `[[background]]` rules, and `[[rule]]` always-rules. Selecting a background or
  rule row loads its fields into a shared editor row (name, priority combo,
  affinity as `0,1`, `qos_cpu_quota`, suspend / trim checkboxes). **Add /
  Delete / Apply** mutate a *local* copy of the config; **Reload cfg** re-fetches
  the service's config back into the editor.
- **Save / Reload / Exit** (bottom): **Save** commits the editor and pushes the
  config to the service; **Reload** asks the service to re-read its config file
  from disk; **Exit** closes the dialog (minimize-to-tray is the other close
  path — use the tray **Exit** to quit the UI). **Save requires elevation**: the
  service only accepts config writes from an elevated client, so a non-elevated
  UI is prompted once to relaunch `aetheris ui` as administrator (UAC) when a
  save is refused.

Save runs the same validate-then-persist path the service uses internally: the
UI validates its working copy before anything leaves the process, then
`SaveConfig` has the service validate again, write the config to a temp file in
the same directory, atomically rename it over the real file, and reload the new
config into the live engine — an invalid config can never reach the file, and
the service keeps running its current config until a save succeeds. (If the
service was unreachable at startup so the config never loaded, Save is refused
until a successful "Reload cfg" — so an empty editor can't overwrite the real
config on disk.)

## Overlay

`aetheris overlay` is a small click-through telemetry panel pinned to the
top-left of the screen, showing live service state over the game. It is an
external DirectComposition window composited by DWM — **zero injection**: no DLL,
no hook, no game-process handle — so it is safe alongside anti-cheat. It reads
the service over the same named pipe as the CLI and never writes.

```sh
aetheris overlay                # against the default pipe \\.\pipe\aetheris
aetheris overlay --pipe <NAME>  # match the service's --pipe
```

The panel shows, refreshed at ~1 Hz:

- **Mode** — `Normal` / `GameBoost`.
- **Boosted processes** — names + pids currently throttled.
- **Last reload** — the service's most recent `reload` result.
- **Pipe name** — for debugging against a non-default pipe.

It is **non-resident**: launch it when you want to look, close it when done —
closing the window exits the process, so there is zero idle cost. While showing
it polls the service once per second and draws one text frame per tick
(measured ~0.5 % of a core); if the service is unreachable it renders
`service unavailable` and keeps retrying rather than crashing.

Known edge: the telemetry read blocks while waiting for the service's reply, so
a wedged-but-connected service could stall the panel for that read (rare — the
service is one-shot respond-then-disconnect, and a down service fails fast).
Activation is either **manual** (launch `aetheris overlay` yourself, or the
tray's "Toggle overlay") or via the service-side hotkey: with
`[overlay] hotkey = "ctrl+alt+o"` in the config, the service launches the
overlay when the hotkey is pressed. Close it with a right-click on the panel or
the window close path.

## Config reference

The default config is **auto-generated on first run** at
`%PROGRAMDATA%\aetheris\aetheris.toml` (the elevated service writes the template
below there). The committed `aetheris.toml` at the repo root is that template —
copy it over the generated file (or pass `--config <path>` to the service) to
customize. Sections:

- `[game]` — `boost_on_start` (activate GameBoost on process start, not just
  foreground), `processes` (Aho-Corasick patterns matched against image names),
  and `purge_standby_on_boost` (opt-in standby memory purge once per game entry;
  see v2-A engine features).
- `[[background]]` — processes throttled while a game runs. `name` is a
  case-insensitive substring; optional `suspend`, `priority` (`idle`,
  `below_normal`, ...), `qos_cpu_quota` (real Job Object CPU cap — a percentage
  of total machine CPU; see v2-A engine features), `trim_memory` (working-set
  flush). Suspend, trim, and QoS are explicit opt-in and default to false — they
  are only applied to non-critical apps.
- `[network]` — opt-in network QoS (see v2-A engine features): `enabled`, `nagle`
  (disable Nagle on every active adapter), `netbios` (disable NetBIOS over
  TCP/IP). All default to false; applied on game-mode entry and reverted on exit.
- `[[rule]]` — always-on rules applied in any mode.
- `protected_extra` — additional names added to the hardcoded protected list.

## v2-A engine features

Three engine features landed in the v2-A slice. All are **opt-in** — nothing
changes unless the config says so:

- **Job Object CPU QoS (`qos_cpu_quota`, now real).** v1's `qos_cpu_quota` was a
  documented no-op for external processes (Background Processing Mode is
  current-process-only). It now caps a background process's CPU with a real Job
  Object (`JOBOBJECT_CPU_RATE_CONTROL_ENABLE | JOB_OBJECT_CPU_RATE_CONTROL_HARD_CAP`),
  where `qos_cpu_quota` is a percentage of the machine's **total** CPU capacity
  (stored internally as `CpuRate = quota * 100`, 0.01 % units). **Reversible:** the
  cap is cleared (`ControlFlags = 0` → unlimited) and the job handle released on
  game exit, service stop, or process exit. The job is never created with
  `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`, so closing a handle never terminates the
  process — jobs simply stop capping. **Attach limitation:** a Job Object can
  only be assigned a process that is **not already in another job**. Processes
  that are already job-bound (browsers, and many apps aetheris did not launch)
  fail the attach with `ERROR_ACCESS_DENIED` and degrade to *no CPU cap* with a
  warn — priority / affinity / suspend still apply. The cap therefore binds
  cleanly on processes aetheris launches fresh, and not on already-job-bound
  ones.
- **Network QoS (`[network]`, opt-in).** On game-mode entry, disables Nagle on
  every active adapter (`TcpAckFrequency=1` + `TCPNoDelay=1` under
  `HKLM\SYSTEM\CurrentControlSet\Services\Tcpip\Parameters\Interfaces`) and
  optionally disables NetBIOS over TCP/IP (`DisableNetbiosOverTcpip`).
  **Reversible:** every value is backed up before it is written and written back
  on game exit (values that did not exist are deleted). Revert logs and swallows
  per-key failures so a registry hiccup never fails the game flow. Touches HKLM,
  so it needs the elevated service.
- **Standby memory purge (`[game] purge_standby_on_boost`, opt-in).** Purges the
  Windows standby list once per game entry
  (`NtSetSystemInformation(SystemMemoryListInformation, MemoryPurgeStandbyList)`,
  requires `SeProfileSingleProcessPrivilege`) so the game can grow its working
  set from free pages. **Not reversible by design** — the OS rebuilds its standby
  list from free pages; the purge is a one-shot at game entry. Benefit is
  debatable on modern Win11 memory management; off unless asked for.

## Known gaps

Genuine remaining debt, documented so it is not mistaken for a bug:

- **Job QoS only caps processes that are not already in a job.** A process that
  is already job-bound (browsers, and many apps aetheris did not launch) fails
  the Job attach with `ERROR_ACCESS_DENIED` and degrades to *no CPU cap* (with a
  warn); priority / affinity / suspend still apply. The cap is a hard cap (no
  bursting) expressed as a percentage of the machine's total CPU, so on a
  many-core host a small quota is a small fraction of one core. This is the
  *only* attach path aetheris has; it never sets `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE`,
  so no handle close can terminate a process.
- **Config is loaded with `std::fs::read`.** `memmap2` is a listed dependency and
  the parse-from-string path is identical, so switching to mmap is a pure I/O swap
  (not yet used).
- **CPU-sets affinity on already-running threads is unverified.** The
  >64-logical-CPU path sets a default mask via `SetProcessDefaultCpuSetMasks`,
  which governs threads that were never explicitly pinned; its effect on threads
  already running when the mask is applied is unverified (needs a dual-group host).
- **Snapshot refresh is throttled to 250 ms.** The integrated message path rebuilds
  the IPC snapshot at most every 250 ms (immediately on `reload` / stop), so
  `get-state` may lag live state by up to that window under event churn.
- **The overlay's private memory is not measured** and will exceed the v2 spec's
  aspirational <2 MB figure. The actual guarantee holds: the overlay is
  non-resident with zero idle cost — the process exits when the window closes.

Previously documented as v1 gaps, **closed in v1.1**:

- **`system_load_percent`** — the stub now samples
  `NtQuerySystemInformation(SystemProcessorPerformanceInformation)` for real, so
  the graceful-degradation hook (defer actions above 85 % load) self-throttles
  correctly.
- **`get-state` empty snapshot** — the shared IPC snapshot is now live, so
  `aetheris cli get-state` prints the current mode and boosted list.
- **`query` always "not found"** — `QueryProcess` now searches live process
  state.
- **CLI must run elevated** — the service pipe now carries an Interactive Users
  DACL, so a non-elevated `aetheris cli` can reach the elevated service.
- **Affinity skips >64-logical-CPU hosts** — best-effort CPU-sets path via
  `SetProcessDefaultCpuSetMasks` / group-affinity.

## License & compliance

- aetheris itself: **MIT** (`workspace.package.license`).
- All direct dependencies are permissive. See `THIRD_PARTY.md` for the full
  direct-dependency license table and the clean-room references statement.
- **`cargo-deny check` is a required CI gate.** `deny.toml` gates wildcard deps
  and unknown registries (`deny`), surfaces unmaintained/unsound advisories, and
  requires every license in the graph to be on the permissive allow list — any
  license not allowed (including copyleft) is denied. With cargo-deny ≥ 0.20,
  vulnerability advisories always fail the check. The full dependency graph must
  pass before merge. Run it locally with:

  ```sh
  cargo install cargo-deny --locked   # once, if not already installed
  cargo deny check                    # advisories check fetches the RustSec
                                      # advisory-db from GitHub — requires network
  ```

## Roadmap

- **v1 (shipped):** ETW process monitor, foreground detection, policy engine,
  five action types, named-pipe IPC, CLI, TOML config, protected list,
  graceful-degradation hook.
- **v1.1 (shipped):** real system-load sampling, live IPC state (`get-state` /
  `query`), non-elevated CLI via pipe DACL, best-effort CPU-sets affinity.
- **v2-A (engine features, shipped):** real Job Object CPU QoS (`qos_cpu_quota`),
  opt-in reversible network QoS (Nagle / NetBIOS), opt-in standby memory purge.
- **v2-B (configuration UI, shipped):** `GetConfig` / `SaveConfig` IPC with
  validation + atomic persist, and the `aetheris ui` config dialog.
- **v2-C (overlay, shipped):** `aetheris overlay` — DirectComposition telemetry
  panel, manual launch, zero injection (see "Overlay" above).
- **v2.1 (hardening, shipped):** least-privilege pipe DACL + elevated-only
  SaveConfig, hotkey-launched overlay, overlay DPI awareness + right-click
  close, non-blocking UI IPC, GameBoost re-entry after config save/reload,
  network-QoS crash reconciliation via marker, QoS PID-reuse protection, GUI
  window-subsystem polish.
- **v2.2 (shipped):** single `aetheris.exe` — the four binaries
  (service / cli / ui / overlay) folded into one binary with subcommands, a
  resident tray UI (start / stop service, toggle overlay, open UI, exit),
  UAC auto-elevation + first-run config auto-created at
  `%PROGRAMDATA%\aetheris\aetheris.toml`, save-as-admin relaunch, multi-game
  re-entry after config reload/save, and the network-QoS crash-reconciliation
  hardening (marker kept on partial revert).
- **v2.3 (planned):** kernel driver (monitor-only).
