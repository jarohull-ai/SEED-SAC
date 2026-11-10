# Contributing to SEED SAC

Thank you for your interest in contributing! This document outlines how to get involved.

## Code of Conduct

Be respectful, inclusive, and constructive. We're building a collaborative, welcoming community.

## How to Report Bugs

1. **Check existing issues** — avoid duplicates
2. **Create a detailed report** including:
   - System info: OS, Rust version, kernel version
   - Steps to reproduce
   - Expected vs. actual behavior
   - Logs (set `RUST_LOG=debug`)

## Security Vulnerabilities

**Do not** open a public issue. Email the maintainer privately:
- jarohull@gmail.com

Include:
- Description of the vulnerability
- Steps to reproduce
- Potential impact
- Suggested fix (optional)

We'll respond within 48 hours and coordinate a fix before public disclosure.

## Feature Requests

Open a GitHub issue with:
- Clear title and description
- Use case and motivation
- Proposed solution (if any)

Label as `enhancement`. We'll discuss before implementation.

## Development Setup

```bash
git clone https://github.com/jarohull-ai/SEED-SAC.git
cd SEED-SAC

# Copy example config
cp config.example.toml config/default.toml

# Build
cargo build

# Run tests
cargo test --workspace

# Run with debug logging
RUST_LOG=debug cargo run -p seed-sac-bin

# Check code quality
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
```

## Code Style

- Use `cargo fmt` — no manual formatting debates
- `cargo clippy` must pass with `-D warnings`
- Tests for all new public APIs
- Doc comments for public functions
- Meaningful commit messages (see below)

## Commit Messages

- **Imperative mood**: "add", "fix", "refactor" (not "added", "fixed")
- **First line**: <70 chars, no period
- **Blank line**, then detailed explanation if needed
- **Reference issues**: "Fixes #123"

Example:
```
fix: prevent Yellow state from emitting failed priority actions

Renice/Ionice were always failing with "identity-bound unavailable",
generating 24 errors/sec spam. Yellow state is now monitoring-only
until Stage 2 implements identity-bound setpriority via pidfd/cgroups.

Fixes #45
```

## Pull Request Process

1. **Fork** the repo
2. **Create a branch**: `git checkout -b feature/my-feature`
3. **Make changes** and test locally
4. **Push** to your fork
5. **Open PR** with:
   - Clear title and description
   - Reference related issues
   - Highlight any breaking changes

### PR Checklist
- [ ] Tests added/updated
- [ ] `cargo fmt` applied
- [ ] `cargo clippy -D warnings` passes
- [ ] `cargo test --workspace` passes
- [ ] Commit messages are clear
- [ ] CHANGELOG.md updated (if user-facing)

## Where to Contribute

### Good First Issues
- Label: `good-first-issue`
- Isolated, well-scoped tasks
- Mentorship available

### Areas Needing Help
- **Documentation** — improve README, add examples
- **Tests** — increase coverage, add edge cases
- **Platform support** — test on FreeBSD, macOS
- **Performance** — profile and optimize hot paths
- **Stage 2** — memory layer (SQLite), micro-models (Needle/MiniLM)

## Stage 2 Work (Memory + Micro-Models)

If you're interested in contributing to the next phase:

### Memory Layer (crates/memory)
- SQLite schema design for events/transitions
- Async worker thread + channel integration
- Event serialization for Markov analysis

### Micro-Models (crates/inference or crates/l0)
- Needle/MiniLM integration (ort/candle bindings)
- Intent classification pipeline
- DLP/Prompt-Guard filtering for LLM context

### Integration
- Route L0 output through router
- Memory events for anomaly detection
- Latency benchmarks

## Review Process

- Maintainer will review PRs within 1 week
- Feedback given constructively
- Multiple passes OK — iterative improvement expected
- CI must pass before merge

## Release Process

Releases follow [Semantic Versioning](https://semver.org/):
- **Patch (0.0.x)**: Bug fixes
- **Minor (0.x.0)**: New features, backward-compatible
- **Major (x.0.0)**: Breaking changes

Changelog entries required for user-facing changes.

## Questions?

- **Development**: GitHub Issues (label: `question`)
- **Chat**: GitHub Discussions (if enabled)
- **Email**: jarohull@gmail.com

---

Thank you for making SEED SAC better! 🚀
