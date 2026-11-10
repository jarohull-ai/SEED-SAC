//! Trait-based abstraction for intent classification backends.
//!
//! Supports both heuristic (regex) and model-based (ONNX/Candle) implementations.

use crate::Intent;
#[cfg(feature = "onnx")]
use std::path::{Path, PathBuf};
#[cfg(feature = "onnx")]
use std::sync::Mutex;
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

#[cfg(feature = "onnx")]
pub struct OnnxBackend {
    runtime: Option<OnnxRuntime>,
    model_path: PathBuf,
    tokenizer_path: PathBuf,
    confidence_threshold: f32,
}

#[cfg(feature = "onnx")]
struct OnnxRuntime {
    session: Mutex<ort::session::Session>,
    tokenizer: tokenizers::Tokenizer,
}

#[cfg(feature = "onnx")]
impl OnnxBackend {
    pub fn new(model_path: impl Into<PathBuf>) -> Self {
        let model_path = model_path.into();
        let tokenizer_path = model_path.with_file_name("tokenizer.json");
        let runtime = Self::load_runtime(&model_path, &tokenizer_path);
        Self {
            runtime,
            model_path,
            tokenizer_path,
            confidence_threshold: 0.65,
        }
    }

    pub fn default_model_path() -> PathBuf {
        if let Some(data_home) = std::env::var_os("XDG_DATA_HOME") {
            return PathBuf::from(data_home).join("seed-sac/models/intent.onnx");
        }
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home).join(".local/share/seed-sac/models/intent.onnx");
        }
        PathBuf::from("seed-sac/models/intent.onnx")
    }

    pub fn is_model_loaded(&self) -> bool {
        self.runtime.is_some()
    }

    pub fn model_path(&self) -> &Path {
        &self.model_path
    }

    pub fn tokenizer_path(&self) -> &Path {
        &self.tokenizer_path
    }

    fn load_runtime(model_path: &Path, tokenizer_path: &Path) -> Option<OnnxRuntime> {
        use tokenizers::TruncationParams;

        if !model_path.is_file() || !tokenizer_path.is_file() {
            return None;
        }
        let mut tokenizer = tokenizers::Tokenizer::from_file(tokenizer_path).ok()?;
        tokenizer
            .with_truncation(Some(TruncationParams {
                max_length: 128,
                ..Default::default()
            }))
            .ok()?;
        let environment = ort::init().with_name("seed-sac-l0").build().ok()?;
        let session = ort::session::Session::builder(&environment)
            .ok()?
            .commit_from_file(model_path)
            .ok()?;
        Some(OnnxRuntime {
            session: Mutex::new(session),
            tokenizer,
        })
    }

    fn infer(&self, input: &str) -> Option<Intent> {
        use ort::value::Tensor;

        let runtime = self.runtime.as_ref()?;
        let encoding = runtime.tokenizer.encode(input, true).ok()?;
        let sequence_len = encoding.get_ids().len();
        if sequence_len == 0
            || encoding.get_attention_mask().len() != sequence_len
            || encoding.get_type_ids().len() != sequence_len
        {
            return None;
        }

        let input_ids = encoding
            .get_ids()
            .iter()
            .map(|value| i64::from(*value))
            .collect::<Vec<_>>();
        let attention_mask = encoding
            .get_attention_mask()
            .iter()
            .map(|value| i64::from(*value))
            .collect::<Vec<_>>();
        let token_type_ids = encoding
            .get_type_ids()
            .iter()
            .map(|value| i64::from(*value))
            .collect::<Vec<_>>();

        let input_ids = Tensor::from_array(([1_usize, sequence_len], input_ids)).ok()?;
        let attention_mask = Tensor::from_array(([1_usize, sequence_len], attention_mask)).ok()?;
        let token_type_ids = Tensor::from_array(([1_usize, sequence_len], token_type_ids)).ok()?;

        let mut session = runtime.session.lock().ok()?;
        let outputs = session
            .run(ort::inputs![
                "input_ids" => input_ids,
                "attention_mask" => attention_mask,
                "token_type_ids" => token_type_ids,
            ])
            .ok()?;
        if outputs.len() == 0 {
            return None;
        }
        let (_, logits) = outputs[0].try_extract_tensor::<f32>().ok()?;
        Some(intent_from_logits(logits, self.confidence_threshold, input))
    }
}

#[cfg(feature = "onnx")]
impl ClassificationBackend for OnnxBackend {
    fn classify(&self, input: &str) -> (Intent, f32) {
        let start = Instant::now();
        let intent = self
            .infer(input)
            .unwrap_or_else(|| HeuristicBackend.classify(input).0);
        let latency_ms = start.elapsed().as_secs_f32() * 1000.0;
        (intent, latency_ms)
    }
}

#[cfg(feature = "onnx")]
fn intent_from_logits(logits: &[f32], threshold: f32, input: &str) -> Intent {
    if logits.len() < 3 || logits.iter().take(3).any(|value| !value.is_finite()) {
        return Intent::Unknown {
            input: input.to_owned(),
            reason: "invalid ONNX logits".to_owned(),
        };
    }

    let max = logits[..3]
        .iter()
        .copied()
        .fold(f32::NEG_INFINITY, f32::max);
    let exp = logits[..3]
        .iter()
        .map(|value| (*value - max).exp())
        .collect::<Vec<_>>();
    let sum = exp.iter().sum::<f32>();
    let (class, confidence) = exp
        .iter()
        .enumerate()
        .map(|(index, value)| (index, value / sum))
        .max_by(|left, right| left.1.total_cmp(&right.1))
        .unwrap_or((2, 0.0));

    if confidence < threshold || class == 2 {
        return Intent::Unknown {
            input: input.to_owned(),
            reason: format!("ONNX confidence {confidence:.3}"),
        };
    }

    match class {
        0 => Intent::Command {
            verb: "model_command".to_owned(),
            subject: Some(input.to_owned()),
        },
        1 => Intent::Analysis {
            topic: input.chars().take(80).collect(),
            ambiguity_score: ((1.0 - confidence) * 100.0).round() as u8,
        },
        _ => unreachable!(),
    }
}

#[cfg(all(test, feature = "onnx"))]
mod onnx_tests {
    use super::*;

    #[test]
    fn missing_model_uses_heuristic_fallback() {
        let backend = OnnxBackend::new("/definitely/missing/seed-sac-intent.onnx");
        assert!(!backend.is_model_loaded());
        assert_eq!(
            backend.model_path(),
            Path::new("/definitely/missing/seed-sac-intent.onnx")
        );
        assert!(matches!(
            backend.classify("kill firefox").0,
            Intent::Command { .. }
        ));
    }

    #[test]
    fn missing_tokenizer_uses_heuristic_fallback() {
        let directory = std::env::temp_dir().join(format!(
            "seed-sac-l0-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock")
                .as_nanos()
        ));
        std::fs::create_dir_all(&directory).expect("temporary model directory");
        let model_path = directory.join("intent.onnx");
        std::fs::write(&model_path, b"not loaded without tokenizer").expect("dummy model");

        let backend = OnnxBackend::new(&model_path);
        assert!(!backend.is_model_loaded());
        assert_eq!(backend.tokenizer_path(), directory.join("tokenizer.json"));
        assert!(matches!(
            backend.classify("why is memory high?").0,
            Intent::Analysis { .. }
        ));

        std::fs::remove_dir_all(directory).expect("remove temporary model directory");
    }

    #[test]
    fn logits_map_to_three_intents_and_low_confidence_is_unknown() {
        assert!(matches!(
            intent_from_logits(&[5.0, 0.0, 0.0], 0.65, "kill firefox"),
            Intent::Command { .. }
        ));
        assert!(matches!(
            intent_from_logits(&[0.0, 5.0, 0.0], 0.65, "why high ram"),
            Intent::Analysis { .. }
        ));
        assert!(matches!(
            intent_from_logits(&[0.0, 0.0, 5.0], 0.65, "maybe"),
            Intent::Unknown { .. }
        ));
        assert!(matches!(
            intent_from_logits(&[1.0, 1.0, 1.0], 0.65, "ambiguous"),
            Intent::Unknown { .. }
        ));
    }
}
