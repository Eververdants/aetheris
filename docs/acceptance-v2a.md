# aetheris v2-A acceptance measurement: Job Object CPU QoS cap

Date: 2026-08-09 (two elevated runs)
Host: 36 logical CPUs, Windows 11, measured **elevated** (UAC auto-approved
`Start-Process -Verb RunAs`; `elevated=True` recorded in-script).
Method: a single busy `dummy_proc` (one spin thread, ~100 % of one core) is
launched under the elevated aetheris-service. GameBoost is entered by starting a
matching `dummy_game` process (`boost_on_start = true`), which applies the
`qos_cpu_quota` background rule to the dummy; the game is then killed to exit
GameBoost. CPU % of one core is computed from `TotalProcessorTime` deltas over a
6 s window (`Δcpu / Δt × 100`).

Config under test:

```toml
[game]
boost_on_start = true
processes = ["dummy_game.exe"]

[[background]]
name = "dummy_proc.exe"
qos_cpu_quota = 1   # run 1;  run 2 used = 2
```

## Results

CPU % of one core for the busy dummy, over three phases of each run:

| Phase | Run 1 (quota = 1) | Run 2 (quota = 2) |
|---|---|---|
| Baseline (no game, uncapped) | **100.5 %** | **100.5 %** |
| GameBoost (job-capped)      | **36.2 %** | **74.2 %** |
| After game exit (cap cleared) | **100.3 %** | **100.5 %** |

Theory: `qos_cpu_quota` is stored as `CpuRate = quota × 100` permyriad of the
machine's **total** CPU. On this 36-logical-CPU host that is
`quota % of 36 cores` = `0.36 cores` (quota 1) and `0.72 cores` (quota 2), i.e.
**36.0 %** and **72.0 %** of one core. Measured: **36.2 %** and **74.2 %**.

## Verdict (spec §8 item 1)

- **Job-capped busy dummy is CPU-limited — PASS.** Busy % falls from ~100 % of
  one core to a level that matches the configured quota almost exactly
  (36.2 % vs 36.0 %; 74.2 % vs 72.0 %). The hard cap binds.
- **Cap clears on game exit — PASS.** After the game process exits, the dummy
  returns to ~100 % of one core (100.3 % / 100.5 %), i.e. `clear_all_qos`
  disabled rate control and released the Job Object.
- **Cap scales with the quota — PASS.** Doubling the quota roughly doubles the
  allowed CPU (36 % → 74 %), confirming the permyriad-of-total-CPU unit.

## Honest notes

- **The dummy was already inside a job object for the whole run**
  (`IsProcessInJob` = True for harness and dummy, before and after the game),
  inherited from the measurement harness. aetheris's `AssignProcessToJobObject`
  nevertheless succeeded — nested-job assignment (Windows 8+) — and the cap
  bound exactly as theory predicts. This is a real, working hard cap; it does
  not contradict the known attach limitation (a process whose immediate job
  forbids nesting still fails the attach and degrades to no-cap with a warn).
- Numbers are single 6 s samples per phase (two independent runs, fresh service
  each time); the +0.2 / +2.2 pp deltas over theory are scheduler enforcement
  granularity, not drift.
- Only the CPU-quota action was configured for `dummy_proc`; no priority /
  affinity / suspend interfered with the measurement.

## How it was measured

`measure-qos-cap.ps1` (in `.superpowers/sdd/`, git-ignored) runs the flow
elevated: launch service with the config above → start `dummy_proc` → baseline
window → start `dummy_game` → settle → capped window → kill `dummy_game` →
settle → post window → cleanup. Raw results and the service banner are in
`.superpowers/sdd/acceptance-qos-results.txt` / `acceptance-qos-service.log`.
The committed binaries (`target/release/aetheris-service.exe`,
`dummy_proc.exe`) were rebuilt before the run.
