# Development

## Requirements

- Rust stable toolchain (edition 2024)
- Linux with X11 or Wayland for the HUD
- Optional local OpenAI-compatible model server for L1 requests

## Build and run

```sh
cargo build --workspace
cargo run -p seed-sac-bin
```

The binary target is named `sac`.

## Quality checks

```sh
cargo fmt --all -- --check
cargo test --workspace --all-targets
cargo clippy --workspace --all-targets -- -D warnings
cargo doc --workspace --no-deps
```

The same checks are defined in [`.github/workflows/ci.yml`](https://github.com/jarohull-ai/SEED-SAC/blob/main/.github/workflows/ci.yml).

## Contribution flow

Read [`CONTRIBUTING.md`](https://github.com/jarohull-ai/SEED-SAC/blob/main/CONTRIBUTING.md), make changes on a focused branch, run the relevant checks, and open a pull request. Do not include local databases, build output, or secrets in commits.
