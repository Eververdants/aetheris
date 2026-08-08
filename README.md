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

(illustrative — v1 `get-state` returns an empty snapshot, so the real output is
an empty `mode:` and a `(none)` boosted list; see Known v1 gaps)

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

## Known v1 gaps

Documented so they are not mistaken for bugs:

- **`system_load_percent` is a v1 stub.** The graceful-degradation hook (defer
  actions when load > 85 %) is wired but the sampler returns 0 until v1.1 adds
  real `NtQuerySystemInformation(SystemProcessorPerformanceInformation)`
  sampling. Because the stub returns 0 the hook never self-throttles.
- **IPC `GetState` returns an empty snapshot.** The service currently serves
  `StateSnapshot::default()`, so `aetheris-cli get-state` may print an empty
  `mode:` and `(none)` for the boosted list even while a game is running. Live
  state is deferred (spec acceptance item 8).
- **`aetheris-cli query` always returns "not found".** IPC `QueryProcess` is a
  v1 stub — the service always answers `Process(None)`.
- **`aetheris-cli` must ALSO run elevated.** The named pipe is created with the
  default pipe DACL, so a non-elevated CLI is denied access to the elevated
  service's pipe. Run the CLI from an elevated terminal too.
- **`qos_cpu_quota` is a safe v1 no-op for cross-process targets.** v1 throttles
  via Background Processing Mode (`SetPriorityClass`), not Job Objects — a
  console service holding a Job Object handle across a game session would
  terminate still-capped processes on Ctrl-C. MSDN documents
  `PROCESS_MODE_BACKGROUND_BEGIN/END` as current-process-only, so applying it to
  another process (the only case aetheris has) fails with
  `ERROR_INVALID_PARAMETER` and is logged as a warning. This is safe and
  reversible; priority / affinity / suspend still apply. A real cross-process
  CPU cap is deferred (v2).
- **Affinity skips >64-logical-CPU hosts.** Classic `SetProcessAffinityMask`
  is a single `ULONG_PTR` and silently no-ops above one processor group.
  Group-aware affinity (`SetProcessDefaultCpuSetMasks` / `NtSetInformationProcess`)
  is the documented v1 limitation.
- **Config is loaded with `std::fs::read`.** `memmap2` is a listed dependency
  and the parse-from-string path is identical, so switching to mmap is a pure
  I/O swap.

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
- **v2 (planned):** kernel driver (monitor-only), DXGI overlay, Win32 config
  UI, network QoS, real system-load sampling, live IPC state.
