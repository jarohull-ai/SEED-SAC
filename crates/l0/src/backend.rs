//! Trait-based abstraction for intent classification backends.
//!
//! Supports both heuristic (regex) and model-based (ONNX/Candle) implementations.

use crate::Intent;
use std::time::Instant;

/// Backend trait for intent classification.
///
/// Implementations must guarantee <10ms latency for classification.
pub trait ClassificationBackend: Send + Sync {
    /// Classify user input into an Intent.
    ///
    /// Returns the intent and elapsed time in milliseconds.
    fn classify(&self, input: &str) -> (Intent, f32);
}

/// Heuristic backend using regex patterns (default, no ML dependencies).
pub struct HeuristicBackend;

impl ClassificationBackend for HeuristicBackend {
    fn classify(&self, input: &str) -> (Intent, f32) {
        let start = Instant::now();
        let trimmed = input.trim().to_lowercase();

        let intent = self
            .classify_internal(&trimmed)
            .unwrap_or_else(|reason| Intent::Unknown {
                input: input.to_owned(),
                reason,
            });

        let latency_ms = start.elapsed().as_secs_f32() * 1000.0;
        (intent, latency_ms)
    }
}

impl HeuristicBackend {
    fn classify_internal(&self, input: &str) -> Result<Intent, String> {
        // Command patterns (EN + PL)
        if self.matches_command_kill(input) {
            return self.extract_kill_target(input);
        }
        if self.matches_command_freeze(input) {
            return self.extract_freeze_target(input);
        }
        if self.matches_command_resume(input) {
            return self.extract_resume_target(input);
        }
        if self.matches_command_drop_caches(input) {
            return Ok(Intent::Command {
                verb: "drop_caches".to_owned(),
                subject: None,
            });
        }

        // Analysis patterns (EN + PL)
        if self.matches_analysis(input) {
            let topic = input.chars().take(80).collect::<String>();
            return Ok(Intent::Analysis {
                topic,
                ambiguity_score: 50,
            });
        }

        Err("no matching pattern".to_owned())
    }

    fn matches_command_kill(&self, input: &str) -> bool {
        input.starts_with("kill ") || input.starts_with("ubij ")
    }

    fn extract_kill_target(&self, input: &str) -> Result<Intent, String> {
        let target = if let Some(t) = input.strip_prefix("kill ") {
            t
        } else if let Some(t) = input.strip_prefix("ubij ") {
            t
        } else {
            return Err("kill: missing target".to_owned());
        };

        let subject = target.trim().to_owned();
        if subject.is_empty() {
            return Err("kill: process name required".to_owned());
        }

        Ok(Intent::Command {
            verb: "kill".to_owned(),
            subject: Some(subject),
        })
    }

    fn matches_command_freeze(&self, input: &str) -> bool {
        input.starts_with("freeze ") || input.starts_with("zamroź ") || input.starts_with("pause ")
    }

    fn extract_freeze_target(&self, input: &str) -> Result<Intent, String> {
        let target = if let Some(t) = input.strip_prefix("freeze ") {
            t
        } else if let Some(t) = input.strip_prefix("zamroź ") {
            t
        } else if let Some(t) = input.strip_prefix("pause ") {
            t
        } else {
            return Err("freeze: missing target".to_owned());
        };

        let subject = target.trim().to_owned();
        if subject.is_empty() {
            return Err("freeze: process name required".to_owned());
        }

        Ok(Intent::Command {
            verb: "freeze".to_owned(),
            subject: Some(subject),
        })
    }

    fn matches_command_resume(&self, input: &str) -> bool {
        input.starts_with("resume ") || input.starts_with("wznów ") || input.starts_with("unpause ")
    }

    fn extract_resume_target(&self, input: &str) -> Result<Intent, String> {
        let target = if let Some(t) = input.strip_prefix("resume ") {
            t
        } else if let Some(t) = input.strip_prefix("wznów ") {
            t
        } else if let Some(t) = input.strip_prefix("unpause ") {
            t
        } else {
            return Err("resume: missing PID".to_owned());
        };

        let subject = target.trim().to_owned();
        if subject.is_empty() {
            return Err("resume: PID required".to_owned());
        }

        Ok(Intent::Command {
            verb: "resume".to_owned(),
            subject: Some(subject),
        })
    }

    fn matches_command_drop_caches(&self, input: &str) -> bool {
        input.contains("drop cache")
            || input.contains("czyszcz cache")
            || input.contains("clear cache")
    }

    fn matches_analysis(&self, input: &str) -> bool {
        // Question mark or analysis keywords (EN + PL)
        input.contains('?')
            || input.starts_with("what ")
            || input.starts_with("how ")
            || input.starts_with("why ")
            || input.starts_with("explain ")
            || input.starts_with("dlaczego ")
            || input.starts_with("co to ")
            || input.starts_with("jak ")
            || input.starts_with("wyjaśnij ")
            || input.starts_with("analyze ")
            || input.starts_with("analiz")
            || input.starts_with("ask ")
    }
}

// ONNX/Candle backend stub (feature-gated)
#[cfg(feature = "onnx")]
pub struct OnnxBackend {
    // Model session will go here
    _model_path: String,
}

#[cfg(feature = "onnx")]
impl ClassificationBackend for OnnxBackend {
    fn classify(&self, input: &str) -> (Intent, f32) {
        let start = Instant::now();

        // TODO: Implement actual ONNX inference
        // For now, fallback to heuristic
        let (intent, _) = HeuristicBackend.classify(input);

        let latency_ms = start.elapsed().as_secs_f32() * 1000.0;
        (intent, latency_ms)
    }
}
