#![cfg(feature = "onnx")]

use l0::backend::{ClassificationBackend, OnnxBackend};
use l0::Intent;
use std::time::Instant;

#[test]
#[ignore = "requires intent.onnx, tokenizer.json and ONNX Runtime"]
fn real_model_classifies_three_intents_under_latency_budget() {
    let load_started = Instant::now();
    let backend = OnnxBackend::new(OnnxBackend::default_model_path());
    let load_ms = load_started.elapsed().as_secs_f64() * 1000.0;
    assert!(
        backend.is_model_loaded(),
        "L0 model artifacts are not loaded"
    );
    println!("onnx_load_ms={load_ms:.3}");

    let (_, warmup_ms) = backend.classify("warm up the intent classifier");
    println!("onnx_warmup_ms={warmup_ms:.3}");
    let cases = [
        ("kill process", "command"),
        ("why is CPU high?", "analysis"),
        ("maybe later", "unknown"),
    ];

    for (input, expected) in cases {
        let (intent, latency_ms) = backend.classify(input);
        println!("input={input:?} latency_ms={latency_ms:.3}");
        match (expected, intent) {
            ("command", Intent::Command { .. })
            | ("analysis", Intent::Analysis { .. })
            | ("unknown", Intent::Unknown { .. }) => {}
            (_, actual) => panic!("{input:?}: expected {expected}, got {actual}"),
        }
        assert!(
            latency_ms < 10.0,
            "{input:?}: L0 latency {latency_ms:.3} ms exceeds 10 ms"
        );
    }
}
