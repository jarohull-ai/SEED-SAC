# Stage 2 Architecture: Memory Layer + Edge-ML Integration

**Target**: v0.3–0.5 releases  
**Scope**: SQLite memory backend + Needle/MiniLM micro-models + DLP filtering  
**Constraint**: Zero impact on HUD responsiveness, sentinel sampling latency < 250ms

---

## Overview

Stage 1 established real-time hardware monitoring and safe action execution. Stage 2 adds **memory** (event history + behavior learning) and **edge intelligence** (L0 micro-models for intent classification and DLP filtering).

```
┌─────────────────────────────────────────────────────────────────┐
│                      Stage 2 Architecture                        │
├─────────────────────────────────────────────────────────────────┤
│                                                                  │
│  ┌──────────────┐     ┌────────────┐      ┌──────────────────┐ │
│  │  Sentinel    │────▶│  Memory    │      │  L0 Inference    │ │
│  │  (monitors)  │     │  (SQLite   │     │  (Needle/MiniLM) │ │
│  │              │     │   async)   │      │  (async worker)  │ │
│  └──────────────┘     └────────────┘      └──────────────────┘ │
│         │                   │                      │             │
│         └───────────────────┴──────────────────────┘             │
│                             │                                    │
│                    ┌────────▼─────────┐                          │
│                    │  L0 Router       │                          │
│                    │  (enhanced by ML) │                          │
│                    └────────┬─────────┘                          │
│                             │                                    │
│                    ┌────────▼──────────┐                         │
│                    │  DLP Filter       │                         │
│                    │  (Prompt-Guard)   │                         │
│                    └────────┬──────────┘                         │
│                             │                                    │
│                    ┌────────▼──────────┐                         │
│                    │  L1 Upload        │                         │
│                    │  (LM Studio)      │                         │
│                    └───────────────────┘                         │
│                                                                  │
└─────────────────────────────────────────────────────────────────┘
```

---

## 1. Memory Layer (crates/memory)

### 1.1 Scope

Track system events in a persistent SQLite database for:
- **Forensics**: What happened before an OOM incident?
- **Behavior learning**: Which processes tend to spike RAM at certain times?
- **Anomaly detection** (L0): Detect unusual patterns via Markov chains
- **Accountability**: Audit trail of freezes, kills, policy changes

### 1.2 Database Schema

```sql
-- events_log: raw incidents
CREATE TABLE events_log (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    timestamp INTEGER NOT NULL,        -- Unix timestamp (ns)
    event_type TEXT NOT NULL,           -- 'yellow', 'red', 'freeze', 'kill', 'resume', 'thaw'
    process_id INTEGER NOT NULL,        -- PID at incident time
    process_name TEXT NOT NULL,         -- Name
    process_start_time INTEGER,         -- Kernel start_time for identity
    memory_mb REAL,                     -- RSS or delta_mb/sec
    cpu_pct REAL,                       -- CPU % at incident
    vram_mb REAL,                       -- VRAM usage if applicable
    policy_state TEXT,                  -- 'green', 'yellow', 'red'
    action_taken TEXT,                  -- What SAC did ('freeze', 'none', etc.)
    details TEXT                        -- JSON: additional context
);
CREATE INDEX idx_events_timestamp ON events_log(timestamp);
CREATE INDEX idx_events_process ON events_log(process_name);

-- transitions: state transitions for Markov analysis
CREATE TABLE transitions (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    timestamp INTEGER NOT NULL,
    from_state TEXT NOT NULL,           -- 'green', 'yellow', 'red'
    to_state TEXT NOT NULL,
    duration_ms INTEGER,                -- How long in previous state
    memory_pct REAL,                    -- Memory % at transition
    cpu_pct REAL,                       -- CPU % at transition
    trigger_process TEXT                -- What caused the transition
);
CREATE INDEX idx_transitions_timestamp ON transitions(timestamp);

-- behavior_patterns: learned sequences
CREATE TABLE behavior_patterns (
    id INTEGER PRIMARY KEY,
    process_name TEXT NOT NULL,
    hour_utc INTEGER,                   -- 0-23 (optional: group by time of day)
    sequence_hash TEXT,                 -- Hash of observed state sequence
    occurrence_count INTEGER,           -- Times seen
    avg_duration_sec REAL,              -- Average incident duration
    last_seen INTEGER                   -- Timestamp
);
CREATE INDEX idx_patterns_process ON behavior_patterns(process_name);
```

### 1.3 Integration Points

**Sentinel writes events**:
```rust
// In crates/sentinel/src/sampler.rs or guard.rs
pub fn record_event(
    event: Event,
    db_tx: &Sender<DbWrite>,  // Channel to memory worker
) {
    let _ = db_tx.try_send(DbWrite::LogEvent(event));
    // Non-blocking; memory worker handles async SQLite write
}
```

**Memory worker thread** (new):
```rust
// crates/memory/src/lib.rs
pub fn spawn_db_worker(db_path: &Path) -> Sender<DbWrite> {
    let (tx, rx) = mpsc::channel(1024);
    thread::spawn(move || {
        let conn = rusqlite::Connection::open(db_path).unwrap();
        conn.execute_batch("PRAGMA journal_mode = WAL;").unwrap();
        while let Ok(write) = rx.recv() {
            match write {
                DbWrite::LogEvent(event) => log_event(&conn, event),
                DbWrite::Query(q, resp_tx) => {
                    let result = query(&conn, q);
                    let _ = resp_tx.send(result);
                }
            }
        }
    });
    tx
}
```

### 1.4 Configuration

```toml
[memory]
enabled = true
db_path = "/home/user/.local/share/seed-sac/events.db"
retention_days = 90              # Keep 90 days of history
batch_size = 10                  # Write to disk every 10 events or 5 seconds
```

---

## 2. L0 Inference (crates/inference or crates/l0)

### 2.1 Scope

Lightweight on-device ML for:
- **Intent classification**: What is the user asking?
- **Anomaly detection**: Is this process behaving oddly?
- **Keyword extraction**: Which system entities are mentioned?

**Model**: Needle 2K or MiniLM (~100MB, runs in <5ms)

### 2.2 Crate Structure

```
crates/l0/
├── Cargo.toml
├── src/
│   ├── lib.rs
│   ├── intent.rs          # Intent classifier
│   ├── embeddings.rs      # Embedding extraction
│   ├── anomaly.rs         # Behavior anomaly detection
│   └── runner.rs          # ONNX Runtime or Candle executor
└── tests/
    └── integration_tests.rs
```

### 2.3 Key Functions

**Intent Classification**:
```rust
pub struct IntentClassifier {
    model: OnnxModel,  // or Candle backend
}

impl IntentClassifier {
    pub fn classify(&self, user_input: &str) -> IntentScore {
        // Tokenize input
        // Run model
        // Return top-k intents: [("kill_process", 0.94), ("freeze_process", 0.03), ...]
    }
}
```

**Anomaly Detection** (using memory events):
```rust
pub fn detect_anomalies(
    db: &Connection,
    process_name: &str,
    window_sec: u64,
) -> Vec<Anomaly> {
    // Query recent events for this process
    // Compute embedding of behavior sequence
    // Compare to Markov baseline
    // Return deviation score
}
```

### 2.4 Integration with Router

```rust
// In crates/router/src/executor.rs (enhanced)

pub fn execute_with_l0(
    user_input: &str,
    l0: &IntentClassifier,
    config: &Config,
) -> (Action, f32) {  // Action + confidence
    let intent = l0.classify(user_input);
    let action = route_by_intent(intent, config);
    (action, intent.confidence)
}
```

If confidence < threshold (0.5), forward to L1 instead of executing immediately.

---

## 3. DLP / Prompt-Guard (crates/dlp)

### 3.1 Scope

Filter sensitive data before sending context to L1:
- SSH keys, credentials from `/etc/`
- Process environment variables (may contain tokens)
- Memory dumps or kernel data
- User home directories (privacy)

### 3.2 Crate Structure

```
crates/dlp/
├── Cargo.toml
├── src/
│   ├── lib.rs
│   ├── rules.rs           # Regex + heuristic patterns
│   ├── sanitize.rs        # Clean context before L1
│   └── config.rs          # DLP policy TOML
└── tests/
    └── unit_tests.rs
```

### 3.3 Sanitization Rules

```rust
pub struct DlpFilter {
    rules: Vec<SanitizationRule>,
}

pub enum SanitizationRule {
    Regex { pattern: String, replacement: String },
    PatternType { category: &'static str },  // "ssh_key", "bearer_token", etc.
    Path { exclude_patterns: Vec<String> },  // "/etc/shadow", "/root/.ssh", ...
}

impl DlpFilter {
    pub fn sanitize(&self, context: &str) -> String {
        let mut result = context.to_string();
        for rule in &self.rules {
            result = rule.apply(&result);
        }
        result
    }
}
```

### 3.4 Configuration

```toml
[dlp]
enabled = true
level = "strict"  # or "moderate", "permissive"

# Patterns to redact
redact_patterns = [
    "(?i)bearer\\s+[a-z0-9]+",
    "(?i)password\\s*[:=]\\s*[^\\s]+",
    "(?i)ssh-rsa\\s+[A-Za-z0-9/+=]+",
]

# Paths to exclude from context
exclude_paths = [
    "/etc/shadow",
    "/root/.ssh",
    "/home/*/.ssh",
    "/proc/*/environ",
]
```

---

## 4. Integration Timeline (Phases)

### Phase 1: Memory (v0.3-alpha)
- [ ] Implement crates/memory with SQLite schema
- [ ] Record events from sentinel
- [ ] Query event history from HUD
- [ ] Test async writes don't block sampling

### Phase 2: L0 Inference (v0.4-alpha)
- [ ] Select model (Needle 2K or MiniLM)
- [ ] Implement intent classifier
- [ ] Route high-confidence intents directly
- [ ] Low-confidence → fallback to L1
- [ ] Benchmark latency < 10ms per inference

### Phase 3: DLP Filtering (v0.5-alpha)
- [ ] Implement sanitization rules engine
- [ ] Test pattern coverage (SSH keys, bearers, etc.)
- [ ] Integration point: before L1 upload
- [ ] Configuration UI in HUD (optional)

### Phase 4: Behavior Learning (v0.6+)
- [ ] Anomaly detection from events
- [ ] Markov chain modeling
- [ ] L0 adaptive routing based on learned patterns

---

## 5. Threading & Channel Model

**Thread layout (enhanced from Stage 1)**:

```
┌──────────────────────────────────────────────────────────┐
│                    Main Thread                            │
│                    (egui HUD loop)                        │
└──────────────────────────────────────────────────────────┘
              ▲                              ▲
              │                              │
    ┌─────────┴────────────┐        ┌───────┴──────────────┐
    │   Sentinel Thread    │        │   Memory Thread      │
    │   (sampling, guard)  │        │   (SQLite writes)    │
    └─────────────────────┘        └─────────────────────┘
              │                              │
    ┌─────────┴────────────┐        ┌───────┴──────────────┐
    │   L1 Worker Thread   │        │   L0 Inference Thread│
    │   (LLM requests)     │        │   (Needle/MiniLM)    │
    └─────────────────────┘        └─────────────────────┘

Channels:
- Sentinel → Memory: DbWrite events (non-blocking send)
- Router → L0: IntentQuery (with response channel)
- L0 → Router: IntentScore + Action recommendation
- Router → L1: (unchanged)
```

---

## 6. Testing Strategy

### Unit Tests
- L0 intent classification (mock tokenizer/model)
- DLP sanitization (pattern coverage)
- Memory schema queries

### Integration Tests
- Sentinel → Memory → L0 → Router → Action (end-to-end)
- Latency benchmarks (sampling + L0 < 250ms overhead)
- Memory growth over 1000 events

### Behavioral Tests
- Train L0 on synthetic process behavior
- Verify anomaly detection triggers correctly
- Verify DLP doesn't over-redact needed context

---

## 7. Deployment Readiness Checklist

- [ ] crates/memory compiles, tests pass, clippy -D warnings OK
- [ ] crates/l0 with Needle/MiniLM model (<100MB, <10ms latency)
- [ ] crates/dlp with configurable pattern rules
- [ ] Integration test: Sentinel → Memory → L0 → Router
- [ ] Latency regression: sampling still < 250ms with all layers
- [ ] Documentation: updated README with Stage 2 features
- [ ] Example config with DLP settings + L0 thresholds
- [ ] Performance profile: memory usage, CPU overhead, DB disk usage
- [ ] User guide: how to interpret L0 confidence scores, tune DLP rules

---

## 8. Success Metrics

| Metric | Target |
|--------|--------|
| Intent classification accuracy | >90% on known patterns |
| L0 latency (per inference) | <10ms |
| Total sampling cycle overhead | <50ms (sentinel + L0) |
| Memory DB size (90-day window) | <500MB |
| DLP false-positive rate | <5% (over-redaction) |
| Anomaly detection precision | >85% |

---

## 9. Open Questions for Contributors

1. **Model selection**: Should we start with Needle (2K context) or MiniLM (better semantics)?
2. **ONNX vs. Candle**: Which inference backend (ort crate vs. candle)?
3. **Markov order**: 1st-order chains or higher for behavior modeling?
4. **DLP strictness**: Should default config be "moderate" or "strict"?
5. **HUD UX**: How to expose L0 confidence + anomaly alerts in the UI?

---

## 10. References

- **SQLite WAL**: https://www.sqlite.org/wal.html
- **Needle Model**: https://huggingface.co/Xenova/gte-small (or equivalent MiniLM)
- **ONNX Runtime Rust**: https://github.com/pykeio/ort
- **Candle**: https://github.com/huggingface/candle
- **Markov Chains**: https://en.wikipedia.org/wiki/Markov_chain

---

## Next Steps

1. **Finalize crate structure** — sentinel, hud, router, memory, l0, dlp
2. **Select inference backend** — ONNX Runtime or Candle
3. **Draft SQLite schema** — review with team
4. **Create Phase 1 (Memory) issues** — ready for contributors
5. **Set up CI for new crates** — ensure clippy, tests, bench runs

---

*Last updated: 2026-11-10*
