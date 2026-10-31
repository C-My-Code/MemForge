# MemForge — Development Plan

## Phase 0 — Contracts (sequential, done first)
* Workspace `Cargo.toml`, crate skeletons, and the **public API of every crate**
  written as compiling stubs (`todo!()` bodies). These signatures are the
  contract between parallel workstreams; changing them requires coordination.

## Phase 1 — Parallel implementation (one owner per crate)
| Stream | Crate | Depends on (API only) | Done when |
|---|---|---|---|
| A | `memforge-core` | — | all tests implemented, unit tests incl. fault-injection detection, SIMD + scalar paths |
| B | `memforge-platform` | — | Windows alloc/lock/large pages, topology, affinity, priority, privileges, SMBIOS, WHEA, non-Windows fallback, tests |
| C | `memforge-engine` | A, B stubs | worker orchestration, rotation, stop conditions, status, events, report; tests with mock `MemTest` |
| D | `memforge-cli` | C stub | subcommands, live status line, JSON report, exit codes |
| E | `memforge-gui` | C stub | config panel, dashboard, histogram, error table, save report |

Streams develop against the stubs; each stream only edits files in its own
crate. Separate `CARGO_TARGET_DIR`s avoid build-lock contention.

## Phase 2 — Integration
* Full `cargo build --workspace --release`, `cargo test --workspace`,
  `cargo clippy`.
* End-to-end: `memforge self-test` (must detect injected faults),
  short real run (`--memory 1GiB --passes 1`), GUI launch smoke test.

## Phase 3 — Review & hardening
* Independent code review (correctness of algorithms, unsafe code, races).
* Fix findings; README, LICENSE (MIT/Apache-2.0), GitHub Actions CI.

## Future work
* Kernel driver for physical-address reporting / per-DIMM mapping.
* Per-DIMM/rank attribution using decoded AMD/Intel address maps.
* Temperature (SPD hub sensor) logging, HWiNFO shared-memory integration.
* Saved test profiles ("quick 15 min", "overnight", "tRFC hunt").
