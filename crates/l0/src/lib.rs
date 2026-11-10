//! L0 Intent Classifier — Sub-millisecond deterministic intent routing for user queries.
//!
//! L0 analyzes user input and classifies it into one of three categories:
//! - **Command**: Deterministic system actions (parsed by router L0 grammar)
//! - **Analysis**: Requests requiring LLM context (escalated to L1/LM Studio with DLP sanitization)
//! - **Unknown**: Ambiguous intent (prompts user to clarify via HUD)
//!
//! All classification happens in <1ms, ensuring zero perceptible latency on the HUD thread.

use std::fmt;

/// Intent classification result from L0.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Intent {
    /// Recognized system command (deterministic action).
    Command {
        verb: String,
        subject: Option<String>,
    },
    /// Request requiring LLM analysis and context escalation.
    Analysis {
        topic: String,
        ambiguity_score: u8, // 0-100: 0=clear, 100=very ambiguous
    },
    /// Intent could not be reliably determined.
    Unknown {
        input: String,
        reason: String,
    },
}

impl Intent {
    /// Returns true if this intent should be routed to L1 (LM Studio).
    pub fn requires_l1(&self) -> bool {
        matches!(self, Intent::Analysis { .. })
    }

    /// Returns true if user clarification is needed.
    pub fn requires_clarification(&self) -> bool {
        matches!(self, Intent::Unknown { .. })
    }
}

impl fmt::Display for Intent {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Intent::Command { verb, subject } => {
                if let Some(s) = subject {
                    write!(f, "Command: {} {}", verb, s)
                } else {
                    write!(f, "Command: {}", verb)
                }
            }
            Intent::Analysis { topic, ambiguity_score } => {
                write!(f, "Analysis: {} (ambiguity: {}%)", topic, ambiguity_score)
            }
            Intent::Unknown { input, reason } => {
                write!(f, "Unknown: '{}' ({})", input, reason)
            }
        }
    }
}

/// L0 Intent Classifier configuration.
#[derive(Debug, Clone)]
pub struct ClassifierConfig {
    /// Enable model-based inference (vs. regex-only fallback).
    pub use_model: bool,
    /// Path to ONNX model weights (if use_model=true).
    pub model_path: Option<String>,
    /// Threshold for confidence score (0-100). Below this, intent is Unknown.
    pub confidence_threshold: u8,
    /// Enable verbose logging of classification decisions.
    pub verbose: bool,
}

impl Default for ClassifierConfig {
    fn default() -> Self {
        Self {
            use_model: false, // Fallback to regex-only in Stage 2.0
            model_path: None,
            confidence_threshold: 60,
            verbose: false,
        }
    }
}

/// L0 Intent Classifier — stateless, thread-safe resolver.
pub struct IntentClassifier {
    config: ClassifierConfig,
}

impl IntentClassifier {
    /// Create a new intent classifier with the given configuration.
    pub fn new(config: ClassifierConfig) -> Self {
        Self { config }
    }

    /// Classify user input string into an Intent.
    ///
    /// # Performance Guarantee
    /// Classification completes in <1ms (regex-only) or <10ms (with model inference).
    pub fn classify(&self, input: &str) -> Intent {
        let trimmed = input.trim().to_lowercase();

        // Stage 2.0: Regex-based fallback (deterministic, <1ms)
        self.classify_by_regex(&trimmed)
            .unwrap_or_else(|reason| Intent::Unknown {
                input: input.to_owned(),
                reason,
            })
    }

    /// Classify using regex patterns (fallback for Stage 2.0).
    fn classify_by_regex(&self, input: &str) -> Result<Intent, String> {
        // Command patterns: "kill <process>", "drop caches", "resume <pid>"
        if input.starts_with("kill ") {
            let subject = input[5..].trim().to_owned();
            if subject.is_empty() {
                return Err("kill: missing process name".to_owned());
            }
            return Ok(Intent::Command {
                verb: "kill".to_owned(),
                subject: Some(subject),
            });
        }

        if input.starts_with("drop") || input.contains("drop_caches") {
            return Ok(Intent::Command {
                verb: "drop_caches".to_owned(),
                subject: None,
            });
        }

        if input.starts_with("resume ") {
            let subject = input[7..].trim().to_owned();
            if subject.is_empty() {
                return Err("resume: missing PID".to_owned());
            }
            return Ok(Intent::Command {
                verb: "resume".to_owned(),
                subject: Some(subject),
            });
        }

        // Analysis patterns: question marks, "what", "how", "why", "explain"
        if input.contains('?')
            || input.starts_with("what ")
            || input.starts_with("how ")
            || input.starts_with("why ")
            || input.starts_with("explain ")
            || input.starts_with("tell me ")
        {
            let topic = input.chars().take(50).collect::<String>();
            return Ok(Intent::Analysis {
                topic,
                ambiguity_score: 50, // Default confidence for regex-only
            });
        }

        Err("no matching pattern".to_owned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_command_kill() {
        let classifier = IntentClassifier::new(ClassifierConfig::default());
        let result = classifier.classify("kill chrome");
        match result {
            Intent::Command { verb, subject } => {
                assert_eq!(verb, "kill");
                assert_eq!(subject.as_ref().map(|s| s.as_str()), Some("chrome"));
            }
            _ => panic!("expected Command intent"),
        }
    }

    #[test]
    fn test_command_drop_caches() {
        let classifier = IntentClassifier::new(ClassifierConfig::default());
        let result = classifier.classify("drop caches");
        assert!(matches!(result, Intent::Command { verb, .. } if verb == "drop_caches"));
    }

    #[test]
    fn test_command_resume() {
        let classifier = IntentClassifier::new(ClassifierConfig::default());
        let result = classifier.classify("resume 5432");
        match result {
            Intent::Command { verb, subject } => {
                assert_eq!(verb, "resume");
                assert_eq!(subject.as_ref().map(|s| s.as_str()), Some("5432"));
            }
            _ => panic!("expected Command intent"),
        }
    }

    #[test]
    fn test_analysis_question() {
        let classifier = IntentClassifier::new(ClassifierConfig::default());
        let result = classifier.classify("What is the memory usage?");
        assert!(result.requires_l1());
        assert!(!result.requires_clarification());
    }

    #[test]
    fn test_analysis_explain() {
        let classifier = IntentClassifier::new(ClassifierConfig::default());
        let result = classifier.classify("Explain the VRAM spike");
        assert!(result.requires_l1());
    }

    #[test]
    fn test_unknown_empty() {
        let classifier = IntentClassifier::new(ClassifierConfig::default());
        let result = classifier.classify("");
        assert!(result.requires_clarification());
    }

    #[test]
    fn test_unknown_ambiguous() {
        let classifier = IntentClassifier::new(ClassifierConfig::default());
        let result = classifier.classify("abc123xyz");
        assert!(result.requires_clarification());
    }

    #[test]
    fn test_intent_display() {
        let cmd = Intent::Command {
            verb: "kill".to_owned(),
            subject: Some("firefox".to_owned()),
        };
        assert_eq!(cmd.to_string(), "Command: kill firefox");

        let analysis = Intent::Analysis {
            topic: "memory".to_owned(),
            ambiguity_score: 45,
        };
        assert_eq!(analysis.to_string(), "Analysis: memory (ambiguity: 45%)");
    }
}
