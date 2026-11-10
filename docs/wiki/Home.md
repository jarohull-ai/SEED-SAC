# SEED SAC Wiki

<p align="center">
  <img src="https://raw.githubusercontent.com/jarohull-ai/SEED-SAC/main/assets/seed-sac-logo.jpeg" alt="SEED SAC logo" width="440">
</p>

SEED SAC is a Linux workstation monitor and workload guard. Its Rust runtime samples system resources, applies deterministic policy, presents status in an egui HUD, and records selected events locally.

## Start here

- [[Architecture]] — runtime threads, data flow and crate responsibilities
- [[Safety]] — process identity checks, confirmations and recovery behavior
- [[Configuration]] — local configuration and database location
- [[Development]] — build, format, lint and test commands

## Current project map

| Crate | Role |
|---|---|
| `sentinel` | CPU, RAM and VRAM sampling, policy evaluation and guarded process actions |
| `hud` | egui display and user actions |
| `router` | Deterministic command grammar and action coordination |
| `memory` | Local SQLite event log with WAL and a dedicated writer thread |
| `seed-sac-bin` | Application startup and thread coordination |

## Project documents

- [Repository README](https://github.com/jarohull-ai/SEED-SAC#readme)
- [Stage 2 architecture plan](https://github.com/jarohull-ai/SEED-SAC/blob/main/STAGE2_ARCHITECTURE.md)
- [Technical specification](https://github.com/jarohull-ai/SEED-SAC/blob/main/seed_sac_technical_specification.md)
- [Contribution guide](https://github.com/jarohull-ai/SEED-SAC/blob/main/CONTRIBUTING.md)

The implementation and documents describe different maturity levels. The SQLite event log is implemented; L0 micro-models and DLP filtering remain planned work.
