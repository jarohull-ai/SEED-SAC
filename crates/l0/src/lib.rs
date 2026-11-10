//! L0 Intent Classifier — Sub-millisecond deterministic intent routing for user queries.
//!
//! L0 analyzes user input and classifies it into one of three categories:
//! - **Command**: Deterministic system actions (parsed by router L0 grammar)
//! - **Analysis**: Requests requiring LLM context (escalated to L1/LM Studio with DLP sanitization)
//! - **Unknown**: Ambiguous intent (prompts user to clarify via HUD)
//!
//! All classification happens in <10ms, ensuring minimal latency on the HUD thread.

pub mod backend;

#[cfg(feature = "onnx")]
use backend::OnnxBackend;
use backend::{ClassificationBackend, HeuristicBackend};
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
    Unknown { input: String, reason: String },
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
            Intent::Analysis {
                topic,
                ambiguity_score,
            } => {
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
    /// Threshold for confidence score (0.0-1.0). Below this, intent is Unknown.
    pub confidence_threshold: f32,
    /// Enable verbose logging of classification decisions.
    pub verbose: bool,
}

impl Default for ClassifierConfig {
    fn default() -> Self {
        Self {
            use_model: false, // Fallback to regex-only in Stage 2.0
            model_path: None,
            confidence_threshold: 0.65,
            verbose: false,
        }
    }
}

/// L0 Intent Classifier — stateless, thread-safe resolver.
pub struct IntentClassifier {
    #[allow(dead_code)]
    config: ClassifierConfig,
    backend: Box<dyn ClassificationBackend>,
}

impl IntentClassifier {
    /// Create a new intent classifier with the given configuration.
    pub fn new(config: ClassifierConfig) -> Self {
        #[cfg(feature = "onnx")]
        let backend: Box<dyn ClassificationBackend> = if config.use_model {
            let model_path = config
                .model_path
                .clone()
                .map(Into::into)
                .unwrap_or_else(OnnxBackend::default_model_path);
            Box::new(OnnxBackend::with_confidence_threshold(
                model_path,
                config.confidence_threshold,
            ))
        } else {
            Box::new(HeuristicBackend)
        };

        #[cfg(not(feature = "onnx"))]
        let backend: Box<dyn ClassificationBackend> = Box::new(HeuristicBackend);
        Self { config, backend }
    }

    /// Classify user input string into an Intent.
    ///
    /// # Performance Guarantee
    /// Classification completes in <10ms (heuristic) or with model inference via backend.
    pub fn classify(&self, input: &str) -> Intent {
        let (intent, latency_ms) = self.backend.classify(input);

        if self.config.verbose {
            eprintln!("[L0] {} (latency: {:.2}ms)", intent, latency_ms);
        }

        intent
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
                assert_eq!(subject.as_deref(), Some("chrome"));
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
                assert_eq!(subject.as_deref(), Some("5432"));
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

    #[test]
    fn test_command_kill_polish() {
        let classifier = IntentClassifier::new(ClassifierConfig::default());
        let result = classifier.classify("ubij chrome");
        match result {
            Intent::Command { verb, subject } => {
                assert_eq!(verb, "kill");
                assert_eq!(subject.as_deref(), Some("chrome"));
            }
            _ => panic!("expected Command intent"),
        }
    }

    #[test]
    fn test_command_freeze() {
        let classifier = IntentClassifier::new(ClassifierConfig::default());
        let result = classifier.classify("freeze firefox");
        match result {
            Intent::Command { verb, subject } => {
                assert_eq!(verb, "freeze");
                assert_eq!(subject.as_deref(), Some("firefox"));
            }
            _ => panic!("expected Command intent"),
        }
    }

    #[test]
    fn test_command_freeze_polish() {
        let classifier = IntentClassifier::new(ClassifierConfig::default());
        let result = classifier.classify("zamroź cargo");
        match result {
            Intent::Command { verb, subject } => {
                assert_eq!(verb, "freeze");
                assert_eq!(subject.as_deref(), Some("cargo"));
            }
            _ => panic!("expected Command intent"),
        }
    }

    #[test]
    fn test_command_pause() {
        let classifier = IntentClassifier::new(ClassifierConfig::default());
        let result = classifier.classify("pause java");
        match result {
            Intent::Command { verb, subject } => {
                assert_eq!(verb, "freeze");
                assert_eq!(subject.as_deref(), Some("java"));
            }
            _ => panic!("expected Command intent"),
        }
    }

    #[test]
    fn test_analysis_dlaczego() {
        let classifier = IntentClassifier::new(ClassifierConfig::default());
        let result = classifier.classify("dlaczego CPU jest wysoki?");
        assert!(result.requires_l1());
        assert!(!result.requires_clarification());
    }

    #[test]
    fn test_analysis_wyjasnij() {
        let classifier = IntentClassifier::new(ClassifierConfig::default());
        let result = classifier.classify("wyjaśnij zmiany VRAM");
        assert!(result.requires_l1());
    }

    #[test]
    fn test_analysis_how() {
        let classifier = IntentClassifier::new(ClassifierConfig::default());
        let result = classifier.classify("How much memory is used?");
        assert!(result.requires_l1());
    }

    #[test]
    fn test_command_drop_caches_variant() {
        let classifier = IntentClassifier::new(ClassifierConfig::default());
        let result = classifier.classify("czyszcz cache");
        assert!(matches!(result, Intent::Command { verb, .. } if verb == "drop_caches"));
    }
}
