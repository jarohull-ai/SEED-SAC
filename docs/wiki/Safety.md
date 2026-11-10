# Safety model

SEED SAC treats resource pressure and process actions as separate steps: the sentinel evaluates local measurements, policy selects an action, and process identity is checked before a signal is delivered.

## Process actions

- Critical pressure can freeze a selected process with `SIGSTOP`, preserving its state while stopping its execution.
- The HUD offers explicit resume or terminate actions for a frozen process.
- Destructive actions require a second confirmation and expire after a short timeout.
- Linux process signaling uses pidfd identity binding where available; unsupported identity-safe operations fail closed.
- On application shutdown, the guard resumes processes that SAC froze so they are not left stopped.

## Data handling

- Telemetry and event history remain in the local process and SQLite database.
- The L1 client is configured for a local OpenAI-compatible endpoint by default.
- The database writer uses a separate thread; UI and sampler callers enqueue records instead of opening or writing the database.
- L0/DLP filtering is planned. Do not treat the planned filtering stages as implemented protections.

See the code in [`crates/sentinel/src/guard.rs`](https://github.com/jarohull-ai/SEED-SAC/blob/main/crates/sentinel/src/guard.rs) and [`crates/router/src/executor.rs`](https://github.com/jarohull-ai/SEED-SAC/blob/main/crates/router/src/executor.rs).
