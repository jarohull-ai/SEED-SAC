#!/usr/bin/env python3
import argparse
import os
import random
import statistics
import tempfile
import time
from pathlib import Path


LABELS = {"Command": 0, "Analysis": 1, "Unknown": 2}
TRAINING_EXAMPLES = [
    ("kill process", 0),
    ("kill firefox", 0),
    ("terminate chrome", 0),
    ("freeze compiler", 0),
    ("resume process 1234", 0),
    ("drop caches", 0),
    ("ubij proces", 0),
    ("zabij firefox", 0),
    ("zamroz kompilator", 0),
    ("wznow proces 1234", 0),
    ("wstrzymaj chrome", 0),
    ("wyczysc cache", 0),
    ("why is CPU high?", 1),
    ("why is memory usage growing?", 1),
    ("explain the VRAM spike", 1),
    ("analyze running processes", 1),
    ("what uses the most RAM?", 1),
    ("how can I reduce CPU load?", 1),
    ("dlaczego CPU jest wysokie?", 1),
    ("wyjasnij skok pamieci", 1),
    ("przeanalizuj procesy", 1),
    ("co zuzywa najwiecej RAM?", 1),
    ("jak zmniejszyc uzycie CPU?", 1),
    ("dlaczego system zwalnia?", 1),
    ("maybe later", 2),
    ("hello", 2),
    ("not sure", 2),
    ("do something", 2),
    ("abc123xyz", 2),
    ("please help", 2),
    ("moze pozniej", 2),
    ("czesc", 2),
    ("nie wiem", 2),
    ("zrob cos", 2),
    ("pomoz mi", 2),
    ("to zalezy", 2),
]
VALIDATION_EXAMPLES = [
    ("kill process", 0),
    ("why is CPU high?", 1),
    ("maybe later", 2),
]


def default_output_dir() -> Path:
    data_home = os.environ.get("XDG_DATA_HOME")
    if data_home:
        return Path(data_home) / "seed-sac" / "models"
    return Path.home() / ".local" / "share" / "seed-sac" / "models"


def dependencies():
    try:
        import numpy
        import onnx
        import onnxruntime
        import torch
        import transformers
        from onnxruntime.quantization import QuantType, quantize_dynamic
    except ImportError as error:
        raise SystemExit(
            "Missing Python dependencies. Install in a virtual environment: "
            "python -m pip install torch transformers onnx onnxruntime numpy"
        ) from error
    return numpy, onnx, onnxruntime, torch, transformers, QuantType, quantize_dynamic


def train_model(torch, transformers, model_id: str, epochs: int, seed: int):
    random.seed(seed)
    torch.manual_seed(seed)
    tokenizer = transformers.AutoTokenizer.from_pretrained(model_id, use_fast=True)
    model = transformers.AutoModelForSequenceClassification.from_pretrained(
        model_id,
        num_labels=3,
        id2label={value: key for key, value in LABELS.items()},
        label2id=LABELS,
    )
    model.train()

    texts = [text for text, _ in TRAINING_EXAMPLES]
    labels = torch.tensor([label for _, label in TRAINING_EXAMPLES], dtype=torch.long)
    encoded = tokenizer(
        texts,
        padding=True,
        truncation=True,
        max_length=128,
        return_tensors="pt",
    )
    if "token_type_ids" not in encoded:
        encoded["token_type_ids"] = torch.zeros_like(encoded["input_ids"])

    optimizer = torch.optim.AdamW(model.parameters(), lr=5e-5)
    batch_size = 12
    for _ in range(epochs):
        order = torch.randperm(len(labels))
        for start in range(0, len(labels), batch_size):
            indices = order[start : start + batch_size]
            batch = {name: value[indices] for name, value in encoded.items()}
            output = model(**batch, labels=labels[indices])
            optimizer.zero_grad(set_to_none=True)
            output.loss.backward()
            optimizer.step()

    model.eval()
    return tokenizer, model


def export_model(torch, tokenizer, model, destination: Path):
    class LogitsOnly(torch.nn.Module):
        def __init__(self, wrapped):
            super().__init__()
            self.wrapped = wrapped

        def forward(self, input_ids, attention_mask, token_type_ids):
            return self.wrapped(
                input_ids=input_ids,
                attention_mask=attention_mask,
                token_type_ids=token_type_ids,
            ).logits

    sample = tokenizer(
        "why is CPU high?",
        return_tensors="pt",
        truncation=True,
        max_length=128,
    )
    if "token_type_ids" not in sample:
        sample["token_type_ids"] = torch.zeros_like(sample["input_ids"])
    wrapper = LogitsOnly(model)
    torch.onnx.export(
        wrapper,
        (sample["input_ids"], sample["attention_mask"], sample["token_type_ids"]),
        destination,
        input_names=["input_ids", "attention_mask", "token_type_ids"],
        output_names=["logits"],
        dynamic_axes={
            "input_ids": {0: "batch", 1: "sequence"},
            "attention_mask": {0: "batch", 1: "sequence"},
            "token_type_ids": {0: "batch", 1: "sequence"},
            "logits": {0: "batch"},
        },
        opset_version=17,
        do_constant_folding=True,
        dynamo=False,
    )


def ort_inputs(tokenizer, text: str):
    encoded = tokenizer(
        text,
        return_tensors="np",
        truncation=True,
        max_length=128,
    )
    if "token_type_ids" not in encoded:
        encoded["token_type_ids"] = encoded["input_ids"] * 0
    return {
        "input_ids": encoded["input_ids"].astype("int64"),
        "attention_mask": encoded["attention_mask"].astype("int64"),
        "token_type_ids": encoded["token_type_ids"].astype("int64"),
    }


def verify_model(numpy, onnxruntime, tokenizer, model_path: Path):
    session = onnxruntime.InferenceSession(
        str(model_path), providers=["CPUExecutionProvider"]
    )
    observed = []
    for text, expected in VALIDATION_EXAMPLES:
        logits = session.run(["logits"], ort_inputs(tokenizer, text))[0][0]
        actual = int(numpy.argmax(logits))
        if actual != expected:
            raise RuntimeError(
                f"validation failed for {text!r}: expected {expected}, got {actual}"
            )
        observed.append((text, actual))

    warmup = ort_inputs(tokenizer, "why is CPU high?")
    session.run(["logits"], warmup)
    timings = []
    for _ in range(20):
        started = time.perf_counter()
        session.run(["logits"], warmup)
        timings.append((time.perf_counter() - started) * 1000.0)
    return observed, statistics.median(timings)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--output-dir", type=Path, default=default_output_dir())
    parser.add_argument("--model-id", default="prajjwal1/bert-tiny")
    parser.add_argument("--epochs", type=int, default=20)
    parser.add_argument("--seed", type=int, default=7)
    parser.add_argument("--max-size-mb", type=float, default=20.0)
    parser.add_argument("--force", action="store_true")
    args = parser.parse_args()

    if args.epochs < 1:
        raise SystemExit("--epochs must be positive")
    output_dir = args.output_dir.expanduser().resolve()
    model_path = output_dir / "intent.onnx"
    tokenizer_path = output_dir / "tokenizer.json"
    if not args.force and (model_path.exists() or tokenizer_path.exists()):
        raise SystemExit("model artifacts already exist; pass --force to replace them")

    numpy, onnx, onnxruntime, torch, transformers, QuantType, quantize_dynamic = (
        dependencies()
    )
    tokenizer, model = train_model(
        torch, transformers, args.model_id, args.epochs, args.seed
    )

    output_dir.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(
        prefix=".seed-sac-l0-", dir=output_dir
    ) as temporary:
        temporary = Path(temporary)
        fp32_path = temporary / "intent.fp32.onnx"
        int8_path = temporary / "intent.onnx"
        exported_tokenizer = temporary / "tokenizer.json"

        export_model(torch, tokenizer, model, fp32_path)
        onnx.checker.check_model(str(fp32_path))
        quantize_dynamic(str(fp32_path), str(int8_path), weight_type=QuantType.QInt8)
        onnx.checker.check_model(str(int8_path))
        tokenizer.backend_tokenizer.save(str(exported_tokenizer))

        size_mb = int8_path.stat().st_size / (1024 * 1024)
        if size_mb >= args.max_size_mb:
            raise RuntimeError(
                f"quantized model is {size_mb:.2f} MiB, limit is {args.max_size_mb:.2f} MiB"
            )
        observed, median_ms = verify_model(
            numpy, onnxruntime, tokenizer, int8_path
        )
        os.replace(int8_path, model_path)
        os.replace(exported_tokenizer, tokenizer_path)

    print(f"installed {model_path} ({size_mb:.2f} MiB)")
    print(f"installed {tokenizer_path}")
    print(f"validation={observed}")
    print(f"python_onnxruntime_median_ms={median_ms:.3f}")
    print(
        "Rust E2E also requires libonnxruntime.so discoverable by the dynamic loader "
        "or ORT_DYLIB_PATH."
    )


if __name__ == "__main__":
    main()
