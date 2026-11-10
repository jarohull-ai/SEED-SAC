# Architecture

SEED SAC separates telemetry and mitigation from the graphical event loop. The sampler and policy run on the sentinel thread; the HUD renders a shared display snapshot and sends user actions through channels. Event persistence runs on its own worker.

```text
Linux /proc + GPU telemetry
          │
          ▼
  Sentinel sampler ──► policy ──► guarded actions
          │                         │
          ├── display snapshot ──► egui HUD
          │
          └── event channel ─────► memory-writer ──► SQLite (WAL)

HUD commands ──► router ──► sentinel-side executor
```

## Crate responsibilities

- `sentinel`: samples CPU/RAM/VRAM and processes, evaluates Green/Yellow/Red policy, and performs guarded actions.
- `hud`: displays resource state and accepts user input. It does not access SQLite.
- `router`: parses supported commands and coordinates actions.
- `memory`: owns the SQLite schema and writer thread. Callers enqueue `SystemEvent` values over `std::sync::mpsc`.
- `seed-sac-bin`: creates the workers and coordinates channels and shutdown.

## Event log

The current event categories are `YELLOW_ENTER`, `RED_ENTER`, `SIGSTOP_SENT`, `SIGCONT_SENT` and `TERMINATED`. Rows include a Unix timestamp in milliseconds, optional process identity and resource measurements, and optional details. The database uses SQLite WAL mode and is stored under the user's XDG data directory.

The writer initializes the database during startup. Event writes are queued from the sentinel path; they do not perform SQLite calls on the HUD or sampler thread.

## Source references

- [`crates/memory/src/lib.rs`](https://github.com/jarohull-ai/SEED-SAC/blob/main/crates/memory/src/lib.rs)
- [`crates/seed-sac-bin/src/main.rs`](https://github.com/jarohull-ai/SEED-SAC/blob/main/crates/seed-sac-bin/src/main.rs)
- [`STAGE2_ARCHITECTURE.md`](https://github.com/jarohull-ai/SEED-SAC/blob/main/STAGE2_ARCHITECTURE.md)
