# MemForge

**An open-source memory stress tester for overclocked DDR4/DDR5 on Windows.**

MemForge stresses your RAM from inside Windows, on every core at once, with a
set of patterns designed to expose marginal frequency, timing and voltage
settings. It runs under real-world thermals and power draw, which is where
memory overclocks that pass a boot-time tester often fail.

> ⚠️ A pass does not prove your memory is stable. It proves MemForge didn't find
> an error in the time it ran. Run long (hours), run at operating temperature,
> and combine tools.

## Features

* 11 test algorithms: own-address, walking ones/zeros, solid bits &
  checkerboard, moving inversions (fixed + random), random pattern, block move,
  modulo-20, AVX2/AVX-512 non-temporal streaming, row stress, bit fade.
* All logical CPUs pinned (processor-group aware), memory chunks rotated
  between cores/CCDs each pass.
* Locked memory (`VirtualLock`) and optional 2 MiB large pages so the memory
  under test stays in RAM.
* Detailed errors: address, expected/actual, XOR mask, per-bit (DQ lane)
  histogram.
* Captures **WHEA** hardware-error events from the Windows event log during the run.
* DIMM inventory from SMBIOS (part number, rated/configured speed, voltage).
* Reproducible: seeded RNG, seed recorded in the JSON report.
* Built-in **self-test** that injects simulated bit flips to prove the
  detection pipeline works.
* CLI (`memforge`) and GUI (`memforge-gui`).

## Quick start

```powershell
cargo build --release

# What does the machine look like?
target\release\memforge info

# Prove the detector works (simulated faults must be caught)
target\release\memforge self-test

# Test 90% of free RAM, 4 passes, write a report
target\release\memforge run --memory 90% --passes 4 --json report.json

# Overnight soak
target\release\memforge run --memory 90% --passes 0 --duration 8h --json overnight.json

# GUI
target\release\memforge-gui
```

Run from an **elevated** prompt for best results. To use large pages, the
account also needs the *Lock pages in memory* right (`secpol.msc` → Local
Policies → User Rights Assignment → Lock pages in memory; sign out and back in).

### Exit codes (CLI)

| Code | Meaning |
|---|---|
| 0 | Pass: ≥1 full pass, no errors (`self-test`: injected faults detected) |
| 1 | Errors found (`self-test`: detection broken) |
| 2 | Incomplete (stopped before a full pass); for `self-test`, inconclusive |
| 3 | Setup or usage error (bad arguments, allocation failure, report file not writable) |
| 130 | Second Ctrl+C: immediate exit (the first Ctrl+C stops gracefully) |

## Interpreting errors

* **Same bit(s) repeatedly**, clustered in one byte lane → likely a single
  DQ group: often a signal-integrity issue (try more VDD/VDDQ, ProcODT/RTT,
  lower frequency).
* **Random bits across lanes**, mostly in random/moving-inversion tests →
  timings too tight (tRFC, tRRDS/L, tFAW, tWR, tRTP…) or VSOC/VDDIO_MEM.
* **Errors only in bit-fade** → refresh/retention problems (tREFI too high,
  temperature).
* **WHEA warnings without MemForge errors** → corrected errors (ECC, or cache
  hierarchy / Infinity Fabric issues); still means the configuration is unstable.

## Architecture

See [docs/ARCHITECTURE.md](docs/ARCHITECTURE.md) and
[docs/DEVELOPMENT_PLAN.md](docs/DEVELOPMENT_PLAN.md).

```
memforge-cli / memforge-gui  →  memforge-engine  →  memforge-core (algorithms)
                                                 →  memforge-platform (Windows)
```

## Limitations

User-mode testing cannot reach memory used by the OS, drivers or other
programs, and cannot report physical addresses or which DIMM failed. Close
other applications to maximise coverage.

## License

Dual-licensed under MIT or Apache-2.0, at your option.
