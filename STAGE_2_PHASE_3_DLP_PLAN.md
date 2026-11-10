# Stage 2 Phase 3: DLP Sanitizer (crates/dlp)

## Cel
Izolowany moduł filtrujący kontekst systemowy i user input **przed** wysłaniem do L1 (LM Studio). Chroni przed wycieka wrażliwych danych:
- Klucze SSH prywatne (`BEGIN RSA PRIVATE KEY`, `BEGIN OPENSSH PRIVATE KEY`)
- API tokeny (`ghp_*`, `glpat_*`, AWS secret keys)
- Hasła z linii poleceń (`--password=...`, `ssh -p...`)
- PII (home directory paths, usernames w context)

## Architektura

```
Input (user query + system context) 
    ↓
┌─────────────────────────────────────┐
│ DLP Sanitizer (crates/dlp)         │
├─────────────────────────────────────┤
│ 1. Pattern matching (regex/bytes)   │ <1ms
│ 2. Redaction (mask/remove)          │
│ 3. Confidence scoring               │
├─────────────────────────────────────┤
│ Output: Safe context for L1         │
└─────────────────────────────────────┘
    ↓
L1 (LM Studio on :1234) [SAFE]
```

## Cargo.toml Dependencies

```toml
[package]
name = "dlp"

[dependencies]
regex = "1"
bytes = "1"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
```

## Public API Design

### `SanitizationRule`
```rust
pub enum SanitizationRule {
    SSHKey,           // BEGIN RSA/OPENSSH PRIVATE KEY
    APIToken,         // ghp_*, glpat_*, aws_*
    CommandPassword,  // --password=, --pwd=, -p
    PathSensitive,    // /root/*, /home/*/.*
    None,
}
```

### `DLPFilter`
```rust
pub struct DLPFilter {
    rules: Vec<SanitizationRule>,
    strict_mode: bool,  // if true, reject on unconfident match
}

impl DLPFilter {
    pub fn new(rules: Vec<SanitizationRule>) -> Self;
    
    /// Sanitize text, return (cleaned_text, violations_found)
    pub fn sanitize(&self, text: &str) -> (String, Vec<DLPViolation>);
    
    /// Check if text contains sensitive data (no modification).
    pub fn scan(&self, text: &str) -> Vec<DLPViolation>;
}
```

### `DLPViolation`
```rust
pub struct DLPViolation {
    pub rule: SanitizationRule,
    pub pattern: String,       // e.g., "BEGIN RSA PRIVATE KEY"
    pub start_pos: usize,
    pub end_pos: usize,
    pub severity: Severity,    // Critical, High, Medium
}

pub enum Severity {
    Critical,  // Private keys, secrets
    High,      // API tokens, passwords
    Medium,    // PII, paths
}
```

## Implementation Plan

### Phase 3.1: Pattern Registry (Week 1)
- [ ] Define regex patterns for each rule
- [ ] Test regex accuracy (false positive rate <5%)
- [ ] Add common patterns:
  - `-----BEGIN RSA PRIVATE KEY-----`
  - `-----BEGIN OPENSSH PRIVATE KEY-----`
  - `ghp_[a-zA-Z0-9]{36,255}` (GitHub PAT)
  - `glpat-[a-zA-Z0-9_-]{20,}` (GitLab PAT)
  - `AKIA[0-9A-Z]{16}` (AWS Access Key)
  - `aws_secret_access_key\s*=\s*[^\s]+`
  - `--password=\S+`, `--pwd=\S+`
  - `/root/\S*`, `/home/[^/]+/\.ssh\S*`

### Phase 3.2: Sanitizer Core (Week 2)
- [ ] Implement `DLPFilter::sanitize()`
- [ ] Redaction strategies:
  - **Complete removal**: SSH keys, API tokens
  - **Masking**: `password=****`, `/home/USER/.ssh/id_rsa` → `/home/*/id_rsa`
  - **Summarization**: "Found 3 RSA keys, redacted"
- [ ] Confidence scoring:
  - High confidence: Multi-line PEM blocks (100%)
  - Medium confidence: Partial token pattern (70%)
  - Low confidence: Ambiguous passwords (30% — user clarification needed)
- [ ] Unit tests:
  - 50+ test cases covering all rule types
  - Regression tests for false positives
  - Performance tests (<1ms per 10KB text)

### Phase 3.3: HUD/L1 Integration (Week 3)
- [ ] Integrate in `seed-sac-bin::handle_command()`:
  ```rust
  let context = latest_snapshot.as_ref().ok_or(...)?;
  let (safe_context, violations) = dlp_filter.sanitize(&context);
  if !violations.is_empty() {
    tracing::warn!(?violations, "DLP violations detected, sanitized");
  }
  l1_worker.submit(safe_context, generation)?;
  ```
- [ ] HUD alert for critical violations:
  - Show count and types detected
  - Option to review redacted context
- [ ] Log all violations to audit trail

### Phase 3.4: Hardening & Validation (Week 4)
- [ ] Fuzz testing with random data + real SSH keys
- [ ] Performance profiling (<0.5ms per 100KB)
- [ ] Security audit: Check for bypass patterns
- [ ] Documentation:
  - DLP rule reference
  - Known limitations
  - False positive troubleshooting

## Integration Points

### In `seed-sac-bin/src/main.rs`
```rust
// After L1 context is assembled:
let dlp = crates::dlp::DLPFilter::new(vec![
    SanitizationRule::SSHKey,
    SanitizationRule::APIToken,
    SanitizationRule::CommandPassword,
]);

let (safe_context, violations) = dlp.sanitize(&user_query_with_context);
if violations.is_critical() {
    hud_banner = format!("⚠️ DLP: {} sensitive fields redacted", violations.len());
}

l1_worker.submit(safe_context, generation)?;
```

## Success Criteria

- ✅ All 50+ unit tests passing
- ✅ Zero crashes on malformed input
- ✅ <1ms latency for typical contexts (10-50KB)
- ✅ No false negatives on real SSH/API keys
- ✅ <10% false positive rate on benign data
- ✅ Audit log captures all violations
- ✅ Integration test with live L1 submission

## References

- OWASP Data Sanitization: https://owasp.org/www-community/attacks/Content_Spoofing
- PII Patterns: https://github.com/marekq/regexes-for-finding-secrets
- SSH Key Detection: RFC 4716, OpenSSH format

---

**Status**: Ready for implementation approval  
**Estimated effort**: 4 weeks (phased rollout)  
**Owner**: DLP Safety Working Group
