//! Data Loss Prevention (DLP) Sanitizer for SEED SAC.
//!
//! Provides deterministic, sub-millisecond filtering of sensitive credentials
//! (SSH private keys, API tokens, passwords, sensitive paths) before context is
//! escalated to L1 (local LLM).

use std::fmt;

/// Categories of sensitive data monitored by the DLP filter.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum SanitizationRule {
    /// Private cryptographic keys (e.g. RSA, OpenSSH, EC).
    SSHKey,
    /// GitHub Personal Access Tokens (ghp_*).
    GitHubToken,
    /// GitLab Personal Access Tokens (glpat-*).
    GitLabToken,
    /// AWS Access Key ID (AKIA*).
    AWSAccessKey,
    /// Command line password arguments (--password=*, --pwd=*).
    CommandPassword,
    /// Sensitive system paths (/root/*, /etc/shadow).
    PathSensitive,
}

impl SanitizationRule {
    /// Human-readable label for this rule category.
    pub const fn label(self) -> &'static str {
        match self {
            Self::SSHKey => "SSH Private Key",
            Self::GitHubToken => "GitHub Token",
            Self::GitLabToken => "GitLab Token",
            Self::AWSAccessKey => "AWS Access Key",
            Self::CommandPassword => "Command Password",
            Self::PathSensitive => "Sensitive System Path",
        }
    }

    /// Redaction placeholder tag substituted into sanitized text.
    pub const fn redaction_tag(self) -> &'static str {
        match self {
            Self::SSHKey => "[REDACTED:SSH_PRIVATE_KEY]",
            Self::GitHubToken => "[REDACTED:GITHUB_PAT]",
            Self::GitLabToken => "[REDACTED:GITLAB_PAT]",
            Self::AWSAccessKey => "[REDACTED:AWS_KEY]",
            Self::CommandPassword => "[REDACTED:PASSWORD]",
            Self::PathSensitive => "[REDACTED:SENSITIVE_PATH]",
        }
    }

    /// Default severity rating for this violation.
    pub const fn severity(self) -> Severity {
        match self {
            Self::SSHKey => Severity::Critical,
            Self::GitHubToken | Self::GitLabToken | Self::AWSAccessKey => Severity::High,
            Self::CommandPassword => Severity::High,
            Self::PathSensitive => Severity::Medium,
        }
    }
}

/// Severity classification for DLP policy violations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Severity {
    Medium,
    High,
    Critical,
}

impl fmt::Display for Severity {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Medium => f.write_str("MEDIUM"),
            Self::High => f.write_str("HIGH"),
            Self::Critical => f.write_str("CRITICAL"),
        }
    }
}

/// A single detected sensitive data violation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DLPViolation {
    pub rule: SanitizationRule,
    pub pattern: String,
    pub start_pos: usize,
    pub end_pos: usize,
    pub severity: Severity,
}

/// Configurable two-phase DLP filter.
///
/// Phase 1 conducts a fast byte-slice scan for potential trigger signatures.
/// If no trigger signature exists in the input, the text is returned untouched
/// with zero allocations and <0.01 ms latency.
///
/// Phase 2 verifies pattern structure, extracts byte boundaries, and replaces
/// secrets with structured redaction tags.
#[derive(Debug, Clone)]
pub struct DLPFilter {
    rules: Vec<SanitizationRule>,
}

impl Default for DLPFilter {
    fn default() -> Self {
        Self {
            rules: vec![
                SanitizationRule::SSHKey,
                SanitizationRule::GitHubToken,
                SanitizationRule::GitLabToken,
                SanitizationRule::AWSAccessKey,
                SanitizationRule::CommandPassword,
                SanitizationRule::PathSensitive,
            ],
        }
    }
}

impl DLPFilter {
    /// Create a new DLP filter with the default standard rule set.
    pub fn new() -> Self {
        Self::default()
    }

    /// Create a DLP filter with an explicit subset of rules.
    pub fn with_rules(rules: Vec<SanitizationRule>) -> Self {
        Self { rules }
    }

    /// Check if the text matches fast-path trigger markers.
    fn fast_path_trigger(text: &str) -> bool {
        const TRIGGERS: &[&str] = &[
            "BEGIN ",
            "ghp_",
            "glpat-",
            "AKIA",
            "password=",
            "password ",
            "pwd=",
            "/root/",
            "/etc/shadow",
        ];
        TRIGGERS.iter().any(|trigger| text.contains(trigger))
    }

    /// Scan text and return all detected violations without modifying input.
    pub fn scan(&self, text: &str) -> Vec<DLPViolation> {
        if !Self::fast_path_trigger(text) {
            return Vec::new();
        }

        let mut violations = Vec::new();
        for &rule in &self.rules {
            match rule {
                SanitizationRule::SSHKey => scan_ssh_keys(text, &mut violations),
                SanitizationRule::GitHubToken => scan_github_tokens(text, &mut violations),
                SanitizationRule::GitLabToken => scan_gitlab_tokens(text, &mut violations),
                SanitizationRule::AWSAccessKey => scan_aws_keys(text, &mut violations),
                SanitizationRule::CommandPassword => scan_passwords(text, &mut violations),
                SanitizationRule::PathSensitive => scan_sensitive_paths(text, &mut violations),
            }
        }

        violations.sort_by_key(|v| v.start_pos);
        violations
    }

    /// Sanitize text by replacing sensitive segments with structured tags.
    ///
    /// Returns `(sanitized_text, violations)`.
    pub fn sanitize(&self, text: &str) -> (String, Vec<DLPViolation>) {
        let violations = self.scan(text);
        if violations.is_empty() {
            return (text.to_owned(), Vec::new());
        }

        let mut result = String::with_capacity(text.len());
        let mut last_idx = 0;

        for v in &violations {
            if v.start_pos < last_idx {
                // Overlapping violation, skip
                continue;
            }
            result.push_str(&text[last_idx..v.start_pos]);
            result.push_str(v.rule.redaction_tag());
            last_idx = v.end_pos;
        }

        if last_idx < text.len() {
            result.push_str(&text[last_idx..]);
        }

        (result, violations)
    }
}

fn scan_ssh_keys(text: &str, violations: &mut Vec<DLPViolation>) {
    const HEADERS: &[&str] = &[
        "-----BEGIN RSA PRIVATE KEY-----",
        "-----BEGIN OPENSSH PRIVATE KEY-----",
        "-----BEGIN PRIVATE KEY-----",
        "-----BEGIN EC PRIVATE KEY-----",
        "-----BEGIN DSA PRIVATE KEY-----",
    ];

    for &header in HEADERS {
        let mut search_from = 0;
        while let Some(rel_start) = text[search_from..].find(header) {
            let start = search_from + rel_start;
            let end_marker = header.replace("BEGIN", "END");
            let end = match text[start..].find(&end_marker) {
                Some(rel_end) => start + rel_end + end_marker.len(),
                None => {
                    // Truncated key header
                    start + header.len()
                }
            };

            violations.push(DLPViolation {
                rule: SanitizationRule::SSHKey,
                pattern: header.to_owned(),
                start_pos: start,
                end_pos: end,
                severity: Severity::Critical,
            });

            search_from = end;
        }
    }
}

fn scan_github_tokens(text: &str, violations: &mut Vec<DLPViolation>) {
    let prefix = "ghp_";
    let mut search_from = 0;
    while let Some(rel_start) = text[search_from..].find(prefix) {
        let start = search_from + rel_start;
        let token_body = &text[start + prefix.len()..];
        let token_len = token_body
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric())
            .count();

        // Valid GitHub PAT has at least 30 alphanumeric characters
        if token_len >= 30 {
            let end = start + prefix.len() + token_len;
            violations.push(DLPViolation {
                rule: SanitizationRule::GitHubToken,
                pattern: "ghp_*".to_owned(),
                start_pos: start,
                end_pos: end,
                severity: Severity::High,
            });
            search_from = end;
        } else {
            search_from = start + prefix.len();
        }
    }
}

fn scan_gitlab_tokens(text: &str, violations: &mut Vec<DLPViolation>) {
    let prefix = "glpat-";
    let mut search_from = 0;
    while let Some(rel_start) = text[search_from..].find(prefix) {
        let start = search_from + rel_start;
        let token_body = &text[start + prefix.len()..];
        let token_len = token_body
            .chars()
            .take_while(|c| c.is_ascii_alphanumeric() || *c == '_' || *c == '-')
            .count();

        if token_len >= 20 {
            let end = start + prefix.len() + token_len;
            violations.push(DLPViolation {
                rule: SanitizationRule::GitLabToken,
                pattern: "glpat-*".to_owned(),
                start_pos: start,
                end_pos: end,
                severity: Severity::High,
            });
            search_from = end;
        } else {
            search_from = start + prefix.len();
        }
    }
}

fn scan_aws_keys(text: &str, violations: &mut Vec<DLPViolation>) {
    let prefix = "AKIA";
    let mut search_from = 0;
    while let Some(rel_start) = text[search_from..].find(prefix) {
        let start = search_from + rel_start;
        let key_body = &text[start + prefix.len()..];
        let key_chars: Vec<char> = key_body
            .chars()
            .take_while(|c| c.is_ascii_uppercase() || c.is_ascii_digit())
            .take(16)
            .collect();

        if key_chars.len() == 16 {
            let end = start + prefix.len() + 16;
            violations.push(DLPViolation {
                rule: SanitizationRule::AWSAccessKey,
                pattern: "AKIA*".to_owned(),
                start_pos: start,
                end_pos: end,
                severity: Severity::High,
            });
            search_from = end;
        } else {
            search_from = start + prefix.len();
        }
    }
}

fn scan_passwords(text: &str, violations: &mut Vec<DLPViolation>) {
    const PREFIXES: &[&str] = &["--password=", "--pwd="];

    for &prefix in PREFIXES {
        let mut search_from = 0;
        while let Some(rel_start) = text[search_from..].find(prefix) {
            let start = search_from + rel_start;
            let val_start = start + prefix.len();
            let val_len = text[val_start..]
                .chars()
                .take_while(|c| !c.is_whitespace() && *c != '"' && *c != '\'')
                .count();

            if val_len > 0 {
                let end = val_start + val_len;
                violations.push(DLPViolation {
                    rule: SanitizationRule::CommandPassword,
                    pattern: format!("{prefix}*"),
                    start_pos: start,
                    end_pos: end,
                    severity: Severity::High,
                });
                search_from = end;
            } else {
                search_from = val_start;
            }
        }
    }
}

fn scan_sensitive_paths(text: &str, violations: &mut Vec<DLPViolation>) {
    const SENSITIVE: &[&str] = &["/etc/shadow", "/root/"];

    for &pattern in SENSITIVE {
        let mut search_from = 0;
        while let Some(rel_start) = text[search_from..].find(pattern) {
            let start = search_from + rel_start;
            let end = if pattern.ends_with('/') {
                let path_len = text[start..]
                    .chars()
                    .take_while(|c| !c.is_whitespace() && *c != '"' && *c != '\'')
                    .count();
                start + path_len
            } else {
                start + pattern.len()
            };

            violations.push(DLPViolation {
                rule: SanitizationRule::PathSensitive,
                pattern: pattern.to_owned(),
                start_pos: start,
                end_pos: end,
                severity: Severity::Medium,
            });
            search_from = end;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_clean_input_returns_untouched() {
        let filter = DLPFilter::new();
        let input = "cargo test --workspace and check CPU usage";
        let (sanitized, violations) = filter.sanitize(input);
        assert_eq!(sanitized, input);
        assert!(violations.is_empty());
    }

    #[test]
    fn test_git_commit_hash_is_not_redacted() {
        let filter = DLPFilter::new();
        let input = "commit 7677a9cc52f63d6fb0e6a49aabd58b1266d60685 merged into main";
        let (sanitized, violations) = filter.sanitize(input);
        assert_eq!(sanitized, input);
        assert!(violations.is_empty());
    }

    #[test]
    fn test_redacts_github_token() {
        let filter = DLPFilter::new();
        let token = "ghp_123456789012345678901234567890123456";
        let input = format!("export GITHUB_TOKEN={token}");
        let (sanitized, violations) = filter.sanitize(&input);
        assert_eq!(sanitized, "export GITHUB_TOKEN=[REDACTED:GITHUB_PAT]");
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].rule, SanitizationRule::GitHubToken);
        assert_eq!(violations[0].severity, Severity::High);
    }

    #[test]
    fn test_redacts_gitlab_token() {
        let filter = DLPFilter::new();
        let token = "glpat-abcdef12345678901234";
        let input = format!("curl --header 'PRIVATE-TOKEN: {token}' https://gitlab.com");
        let (sanitized, violations) = filter.sanitize(&input);
        assert!(sanitized.contains("[REDACTED:GITLAB_PAT]"));
        assert!(!sanitized.contains(token));
        assert_eq!(violations.len(), 1);
    }

    #[test]
    fn test_redacts_aws_access_key() {
        let filter = DLPFilter::new();
        let key = "AKIAIOSFODNN7EXAMPLE";
        let input = format!("AWS_ACCESS_KEY_ID={key} s3 ls");
        let (sanitized, violations) = filter.sanitize(&input);
        assert_eq!(sanitized, "AWS_ACCESS_KEY_ID=[REDACTED:AWS_KEY] s3 ls");
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].rule, SanitizationRule::AWSAccessKey);
    }

    #[test]
    fn test_redacts_command_line_password() {
        let filter = DLPFilter::new();
        let input = "mysql -u root --password=SuperSecretPassword123 db";
        let (sanitized, violations) = filter.sanitize(input);
        assert_eq!(sanitized, "mysql -u root [REDACTED:PASSWORD] db");
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].rule, SanitizationRule::CommandPassword);
    }

    #[test]
    fn test_redacts_ssh_rsa_private_key() {
        let filter = DLPFilter::new();
        let input = "Key is:\n-----BEGIN RSA PRIVATE KEY-----\nMIIEowIBAAKCAQEA0\n-----END RSA PRIVATE KEY-----\nDone.";
        let (sanitized, violations) = filter.sanitize(input);
        assert_eq!(sanitized, "Key is:\n[REDACTED:SSH_PRIVATE_KEY]\nDone.");
        assert_eq!(violations.len(), 1);
        assert_eq!(violations[0].severity, Severity::Critical);
    }

    #[test]
    fn test_redacts_openssh_private_key() {
        let filter = DLPFilter::new();
        let input = "-----BEGIN OPENSSH PRIVATE KEY-----\nb3BlbnNzaC1rZXktdjE\n-----END OPENSSH PRIVATE KEY-----";
        let (sanitized, violations) = filter.sanitize(input);
        assert_eq!(sanitized, "[REDACTED:SSH_PRIVATE_KEY]");
        assert_eq!(violations.len(), 1);
    }

    #[test]
    fn test_redacts_sensitive_system_path() {
        let filter = DLPFilter::new();
        let input = "cat /etc/shadow and check /root/.ssh/id_rsa";
        let (sanitized, violations) = filter.sanitize(input);
        assert!(sanitized.contains("[REDACTED:SENSITIVE_PATH]"));
        assert!(!sanitized.contains("/etc/shadow"));
        assert!(!sanitized.contains("/root/.ssh/id_rsa"));
        assert_eq!(violations.len(), 2);
    }

    #[test]
    fn test_normal_user_paths_not_redacted() {
        let filter = DLPFilter::new();
        let input = "open /home/jaro/Desktop/project/file.rs";
        let (sanitized, violations) = filter.sanitize(input);
        assert_eq!(sanitized, input);
        assert!(violations.is_empty());
    }

    #[test]
    fn test_multiple_mixed_secrets_in_single_prompt() {
        let filter = DLPFilter::new();
        let input = "Setup: ghp_123456789012345678901234567890123456 with AKIAIOSFODNN7EXAMPLE and --password=Secret12345";
        let (sanitized, violations) = filter.sanitize(input);
        assert_eq!(
            sanitized,
            "Setup: [REDACTED:GITHUB_PAT] with [REDACTED:AWS_KEY] and [REDACTED:PASSWORD]"
        );
        assert_eq!(violations.len(), 3);
    }

    #[test]
    fn test_fast_path_latency_benchmark() {
        let filter = DLPFilter::new();
        let large_clean_text = "fn main() { println!(\"Hello, world!\"); }\n".repeat(500); // ~20KB text
        let start = std::time::Instant::now();
        let (sanitized, violations) = filter.sanitize(&large_clean_text);
        let elapsed = start.elapsed();

        assert_eq!(sanitized.len(), large_clean_text.len());
        assert!(violations.is_empty());
        // Must complete well under 1ms (target <1ms for 10KB)
        assert!(
            elapsed.as_micros() < 1000,
            "Fast path took too long: {} us",
            elapsed.as_micros()
        );
    }
}
