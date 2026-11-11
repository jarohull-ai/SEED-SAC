# Changelog

## [0.4.0-alpha] - Sovereign Diagnostics

### Added

- Deterministic diagnostic frame generated from Sentinel telemetry.
- Rust-owned `Diagnoza:`, `Przyczyna:` and `Sugerowana komenda:` fields.
- L1 explanation-only contract with optional `Objaśnienie L1:` output.
- Entity validation for PIDs and process names mentioned by L1.
- Interactive Live Console action latch for deterministic Kill/Freeze suggestions.
- Revalidation of the current process identity before arming an action.
- Existing two-step confirmation remains mandatory before execution.
- ONNX INT8 intent backend and tokenizer bridge documentation in the release scope.

### Safety

- The model cannot replace or execute a suggested command.
- PID reuse, immune-process policy and current Sentinel snapshots remain authoritative.
- A stale or changed diagnosis blocks the interactive action.

### Verification

- Full workspace tests with all features pass.
- HUD egui interaction tests cover suggestion arming.
- The real ONNX E2E test remains opt-in because it requires external model artifacts and runtime.
