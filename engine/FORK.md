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
