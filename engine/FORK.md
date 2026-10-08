# Provenance

Forked from `univec-ai/stack` at `667e53c78b31d371f3e0341fb55c83e8e5b21b87` (2026-08-18).

The private original stays in that repository and is not modified. This copy is
trimmed **by deletion only**: modules, dependencies, match arms and parameters
are removed, no inference logic is rewritten. A `diff -r` against the private
`engine/` should therefore show absences, not differences — that is what keeps
a future backport mechanical.

## Known / deliberate

- `axum` is still a dependency, for the `HeaderMap` type in the execution
  context. Replacing it with `http` changes a shared signature for cosmetic
  benefit and would make every future backport diff noisy. Left as is.
- The `ort-cuda` / `ort-tensorrt` features and `get_pool_size`'s ONNX
  GPU-availability check survive: they are ONNX execution-provider plumbing,
  not the Candle backend, and deleting them would edit live code paths.

## Synced from upstream

Upstream changes picked up after the fork point:

- `univec-ai/stack@273d335d` (2026-10-04), "vector converters normalizing
  inputs": `VectorEmbeddingExecutor` L2-normalizes input rows before the
  model runs (`executor.params.normalize_input`, default `true`) - univec
  converters are trained on unit-length vectors. Affects:
  `src/executors/vector_embedding.rs`
  `src/executors/mod.rs`; tests included.
- `univec-ai/stack@a427885a` (2026-10-07), "tokenization fixes", plus the
  engine half of `@21d72c76`: the tokenizer truncates at an explicit
  `tokenizer.max_length`, else the model's `params.sequence_len`, else 512,
  clamped to the engine-wide ceiling `HostPolicy::max_sequence_len`
  (default 8192). The embedding executor splits a call when the token
  count or rows times length squared would be too large. A text that
  still does not fit runs alone, and only one such text runs at a time.
  Usage counts real tokens from the attention mask. Truncating a unit
  vector re-normalizes it. Dynamic quantization runs one text per batch.
  A loaded `tokenizer.json` has its truncation and padding overwritten
  from the descriptor. `Executor::max_input_tokens` and
  `InferenceEngine::max_input_tokens` report the length actually applied.
  Merged three-way, so postvec's shortened comments are kept; the hunks for
  the deleted executors were dropped. Affects: `src/config.rs`, `src/lib.rs`,
  `src/models/configuration.rs`, `src/tokenizers/{config,huggingface}.rs`,
  `src/executors/{mod,transformer_sequence_embedding}.rs`,
  `tests/configuration.rs`; tests included.
- `univec-ai/stack@435c7ab4` (2026-10-07), "grpc fixes", engine part only:
  `VectorEmbeddingExecutor` rejects a batch whose width differs from the
  graph input's static last dimension (fallback: `params.source_dim`) with
  `InputTypeError` → `INVALID_INPUT`, before normalization or inference;
  before, ONNX Runtime failed with an internal error. Affects:
  `src/executors/vector_embedding.rs`; test included.

## Known deferred lint

`engine/src/lib.rs` carries one `clippy::nonminimal_bool` warning under
`--all-features --all-targets`. It is deliberately NOT fixed here: the fork
rule is that a diff against upstream shows absences, not differences, and a
lint-appeasing edit would be a difference with no functional content. Fix it
upstream and pick it up at the next fork sync. CI therefore runs the engine's
tests but does not lint the engine with `-D warnings`
(.github/workflows/postvec-server-ci.yml, the "test (engine, shared)" step).
