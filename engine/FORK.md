# Fork provenance

Forked from `univec-ai/stack` at `667e53c78b31d371f3e0341fb55c83e8e5b21b87` (2026-08-18).

The private original stays in that repository and is not modified. This copy is
trimmed **by deletion only**: modules, dependencies, match arms and parameters
are removed, no inference logic is rewritten. A `diff -r` against the private
`engine/` should therefore show absences, not differences — that is what keeps
a future backport mechanical.

## Deleted from this copy

Orphaned files (tracked, declared in no `mod` statement, compiled into
nothing):

- `src/executors/encoder_diffusion_decoder.rs`
- `src/executors/unet.rs`

The `hub` dependency (postvec always ran the engine with `hub: None`; the
"model must already be on disk" branch each site already had is the one that
survives):

- `hub = { path = "../hub" }` in `Cargo.toml`
- the `hub` field on `InferenceEngine`, the `hub` parameter of
  `InferenceEngine::new` / `default_with_models`, and the `ensure_local`
  branches in `load_model`'s dependency phase
- the `hub` parameter of `Executor::build` and of
  `executors::build_dependencies`, and that helper's `ensure_local` branch

Executors postvec cannot reach (the factory in `src/executors/mod.rs` is a
string match, so the reachable set is exact — `passthru`, `dummy`,
`vector-embedding`, `embed-bridge`, `convert-bridge`,
`transformer-sequence-embedding` survive):

- `src/executors/transformer_sequence_generation_encoder.rs`
- `src/executors/transformer_sequence_generation_decoder.rs`
- `src/executors/transformer_sequence_generation_decoder_only.rs`
- `src/executors/transformer_sequence_classification.rs`
- `src/executors/transformer_token_classification.rs`
- `src/transformers/` (beam search, encoder, decoder — generation support)
- `tests/beam_search.rs`

The Candle backend (the packaged artifact is ONNX-only and CPU-only):

- `src/models/candle.rs`, `src/models/candle_models/**`
- the eight optional candle dependencies, `hf-hub`, `safetensors`,
  `nohash-hasher`, `memmap2`, `accelerate-src`, `intel-mkl-src`, and the
  `candle` / `cuda` / `flash-attn` / `flash-attn-v1` / `accelerate` / `metal`
  / `mkl` features in `Cargo.toml`
- the `ModelBackend::Candle` instantiation arm and the candle halves of the
  GPU-availability checks (`cfg(all(feature = "candle", feature = "cuda"))`)

The runtime-mutation surface behind ninference's debug UI (rewrites
`ninference.hub.json` on disk; nothing in postvec calls it):

- `get_model_runtime_info`, `resolve_execution_provider`,
  `resize_model_pool_in_memory`, `resize_model_pool`, `gpu_supported`,
  `get_model_runtime_info_from_config`, `set_model_runtime`
- the `ModelRuntimeInfo` and `ModelRuntimeChange` types

The Hugging Face Hub tokenizer download path (a hidden network dependency an
embedded database extension must not have):

- the `HfTokenizer::from_pretrained` branch in
  `src/tokenizers/huggingface.rs` (a `HuggingFacePretrained` tokenizer whose
  `pretrained_vocab_file` is empty is now a typed error telling the operator
  to package the tokenizer file with the model; the local-file branch is
  unchanged)
- the `http` feature of the `tokenizers` dependency (which is what pulled
  `hf-hub` into every build)

Unused dependencies the deletions stranded:

- `rand`, `rand_distr` (used only by the deleted diffusion executor)

## Known and deliberate

- `axum` is still a dependency, for the `HeaderMap` type in the execution
  context. Replacing it with `http` changes a shared signature for cosmetic
  benefit and would make every future backport diff noisy. Left as is.
- The `ort-cuda` / `ort-tensorrt` features and `get_pool_size`'s ONNX
  GPU-availability check survive: they are ONNX execution-provider plumbing,
  not the Candle backend, and deleting them would edit live code paths.

## Added in this copy

The fork is trimmed by deletion only, with two recorded amendments:

- `engine/src/lib.rs`: the ONNX-Runtime-not-found error no longer tells the
  operator to check `NINFERENCE_PATH` (an environment variable this stack
  dropped, 2026-08-31); it says "ensure the engine root is set correctly".
  One string literal; a backport diff shows it as the fork's only edited line
  in this file.

- `shared/src/error.rs`: the `UpstreamAuthFailed` error code (wire string
  `UPSTREAM_AUTH_FAILED`), added 2026-08-19 for postvec's external embedding
  providers (docs/external-providers.md §6.4). A provider HTTP 401/403 is an
  operator problem, not a transient blip, and needed a code the extension can
  classify as configuration. A backport diff will show this variant (and its
  `as_str`/`from_str` arms and round-trip test row) as the fork's only `+`
  lines; upstream may adopt the same variant verbatim.

- `src/executors/transformer_sequence_embedding.rs`: one
  `#[allow(clippy::single_range_in_vec_init)]` on `plan_sub_batches`
  (2026-10-08). The lint arrived with the tokenization sync below;
  `vec![0..n]` is the intended value (one sub-batch), and the server CI's
  `cargo clippy -p postvec-server -p postvec-core -- -D warnings` lints the
  engine as a workspace member, so leaving it would fail CI. Upstream can
  take the same attribute.

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
