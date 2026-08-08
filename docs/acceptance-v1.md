# aetheris v1 acceptance measurement
Date: 2026-08-08T22:55:24.4206081+08:00
Duration: 60s idle
Memory (WorkingSet64): min=18.32MB avg=18.54MB max=18.57MB
CPU: avg=0.000% max=0.000%
Samples: 30 mem / 30 cpu (expected ~30)
Targets: mem<=5MB avg, CPU<0.1% avg

## Result (spec §8)

- Memory: **FAIL** — avg WorkingSet64 18.54 MB > 5 MB target (min 18.32, max 18.57)
- CPU: **PASS** — avg 0.000% < 0.1% target (max 0.000%)
- Verdict: **FAIL** (memory criterion not met; measurement is the deliverable)

## Deviation note (what to tune)

Measured elevated (UAC), service ETW-active and alive for the full 60 s window
(30/30 samples); numbers are real and stable. Independent cross-check of the
running service: WorkingSet64 18.11 MB, PrivateBytes ~10.93 MB,
TotalProcessorTime 0.047 s cumulative (startup-inclusive); idle-window deltas
~0.000%. The authoritative CPU evidence is the 60 s idle-window deltas above
(avg 0.000% < 0.1% target), not the cumulative TotalProcessorTime figure. The
footprint is
dominated by the process runtime and loaded DLLs (shared working-set pages
≈ 7 MB of the 18.5 MB) — the windows-rs feature set plus ETW pulls in
user32/advapi32/etc. — not by the service's own data structures (process table,
rule matchers, config are KB-scale). The 5 MB WorkingSet64 target is not
achievable for this Windows service without aggressive measures; honest floor is
PrivateBytes ~11 MB. Tune options if the 5 MB spec is firm: (1) add an idle
self-trim (`SetProcessWorkingSetSize` on an idle timer — the service already has
this machinery for background processes); (2) trim the windows-rs feature set to
drop GUI/system DLLs from the working set; (3) re-baseline the spec on
PrivateBytes instead of WorkingSet64. CPU criterion is comfortably met.
