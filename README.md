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
- **Named-pipe IPC + CLI.** `aetheris-cli get-state | reload | query <name>`
  over `\\.\pipe\aetheris`.
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

The release profile uses thin LTO + strip + abort-on-panic (`Cargo.toml`).
`Cargo.lock` is committed; this is a binary workspace.

## Run (elevated)

```sh
# In an elevated (admin) terminal:
aetheris-service --config aetheris.toml
```

The service prints a startup banner, then runs until you press Ctrl-C. When it
stops, boosted processes are restored. If `EtwMonitor::start()` fails — e.g.
not elevated, or the kernel trace session cannot be opened — the service
prints `service error: StartTraceW failed: status ...` and exits with code 1.
That is the intended fail-safe.

## CLI usage

```sh
# All against \\.\pipe\aetheris
aetheris-cli get-state            # print current mode + boosted list
aetheris-cli reload               # reload aetheris.toml from disk
aetheris-cli query <name>         # query one process (name / pid / is_game)
aetheris-cli --pipe <NAME> ...    # override the pipe name
```

Example output:

```
mode: GameBoost
boosted:
  chrome.exe (pid 1234)
```

## Config reference

See the committed `aetheris.toml` at the repo root. Sections:

- `[game]` — `boost_on_start` (activate GameBoost on process start, not just
  foreground) and `processes` (Aho-Corasick patterns matched against image
  names).
- `[[background]]` — processes throttled while a game runs. `name` is a
  case-insensitive substring; optional `suspend`, `priority` (`idle`,
  `below_normal`, ...), `qos_cpu_quota` (Background-Processing-Mode CPU throttle;
  see Known v1 gaps), `trim_memory` (working-set flush). Suspend and trim are
  explicit opt-in and default to false — they are only applied to non-critical
  apps.
- `[[rule]]` — always-on rules applied in any mode.
- `protected_extra` — additional names added to the hardcoded protected list.

## Known gaps

Genuine remaining debt, documented so it is not mistaken for a bug:

- **`qos_cpu_quota` does not throttle external processes.** Background Processing
  Mode (`SetPriorityClass`) is documented current-process-only, so applying it to
  another process (the only case aetheris has) fails with `ERROR_INVALID_PARAMETER`
  and is logged as a warning; `qos_cpu_quota` is a documented no-op for external
  processes. This is *not* because Job Object handle-close kills processes (that
  only happens when `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` is set, which it never
  was); a real cross-process CPU cap needs Job Objects with clear-on-stop
  semantics, deferred to v2. Priority / affinity / suspend still apply.
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

Previously documented as v1 gaps, **closed in v1.1**:

- **`system_load_percent`** — the stub now samples
  `NtQuerySystemInformation(SystemProcessorPerformanceInformation)` for real, so
  the graceful-degradation hook (defer actions above 85 % load) self-throttles
  correctly.
- **`get-state` empty snapshot** — the shared IPC snapshot is now live, so
  `aetheris-cli get-state` prints the current mode and boosted list.
- **`query` always "not found"** — `QueryProcess` now searches live process
  state.
- **CLI must run elevated** — the service pipe now carries an Interactive Users
  DACL, so a non-elevated `aetheris-cli` can reach the elevated service.
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
- **v2 (planned):** kernel driver (monitor-only), DXGI overlay, Win32 config
  UI, network QoS.
