# Third-Party Notices

aetheris is MIT-licensed (see the workspace `license` field in `Cargo.toml`).
This file documents every direct dependency of the project, its license, and
the third-party projects consulted while designing it.

All direct dependencies are permissive (MIT / Apache-2.0 / Unlicense). No
GPL, LGPL, or unlicensed code is copied into this project, either directly or
transitively. This is enforced by `cargo-deny` (see `deny.toml`) as a required
CI gate.

## Direct dependencies

| Crate | Version | License | Purpose |
|---|---|---|---|
| `windows` | 0.62 | MIT OR Apache-2.0 | Windows FFI surface: ETW, threading, processes, job objects, named pipes |
| `ntapi` | 0.4 | Apache-2.0 OR MIT | `NtSuspendProcess` / `NtResumeProcess` / `NtSetInformationProcess` |
| `aho-corasick` | 1.1 | Unlicense OR MIT | Pre-compiled rule matching automaton (hot path, zero allocation) |
| `memmap2` | 0.9 | MIT OR Apache-2.0 | Config mapping (listed dependency; config currently loads via `std::fs::read`) |
| `serde` | 1 | MIT OR Apache-2.0 | Serialization for config and IPC messages |
| `toml` | 0.8 | MIT OR Apache-2.0 | Config parsing |
| `bincode` | 1.3 | MIT OR Apache-2.0 | Length-prefixed IPC framing |
| `ctrlc` | 3 | MIT OR Apache-2.0 | Console-mode Ctrl-C handling for the v1 launcher |

Transitive dependencies are not individually listed here; their licenses are
enforced by `cargo-deny check` (bans, licenses, sources), which must pass before
merge. The full locked graph is in `Cargo.lock`.

## Clean-room references (architecture only — no copied code)

The following projects were consulted for architecture, API surface, and
algorithmic design only. Their implementations were not copied, and no code
expression from them appears in this repository. Where a design decision was
informed by one of these projects, the reference is recorded in the design
document (`docs/superpowers/specs/`).

| Project | License | How it informed aetheris |
|---|---|---|
| vnite | GPL-3.0 | ETW kernel-process consumption in a game scenario (session setup, event IDs). Architecture only. |
| Winderust | GPL-3.0 | Foreground detection, layered rules, game-mode orchestration. Architecture only. |
| SpecialK | GPL-3.0 | Game-mode feature surface (background throttling while a game runs). Architecture only. |
| ferrisetw | MIT OR Apache-2.0 | ETW session control + callback consumption + schema-cache decode architecture |
| system_monitor | MIT | Native-Rust ETW session/buffer setup and event decoding (clean-room rewrite) |
| windows-erg | MIT | ETW buffer tuning (128 KB, 5–25 buffers, 1 s flush), batched consumption |
| Process Governor | MIT | Service-mode rules + Job-Object CPU quota enforcement |
| Priority | MIT | CPU/memory/IO priority + affinity API surface and privilege bootstrap |
| gpu-power-limit-daemon | MIT | Service skeleton shape: install / run / console modes |
| uberdisplay | MIT | Zero-async, single-threaded named-pipe accept loop |
| shawl | MIT | Service lifecycle: Ctrl-C, restart policy, thin LTO + strip release profile |
| windows-service-rs | MIT OR Apache-2.0 | Official `windows-service` examples (future v2 service wrapper) |

**Compliance statement:** No GPL, LGPL, or unlicensed code was copied into
aetheris. All GPL-3.0 projects above (vnite, Winderust, SpecialK) were used
strictly as read-only architecture references. The implementation of every
subsystem (ETW consumer, policy engine, actions, IPC, config) is original code
written for this project.
