// File: engine/src/config.rs

//! ## Engine Configuration
//!
//! This module defines the structures for engine-specific configuration.
//! This allows the engine to be configured independently of the server that runs it.
//! It uses `serde` for deserialization from JSON.

use std::path::PathBuf;

/// Resource policy for the process hosting the engine.
///
/// `Default` preserves the historical throughput-oriented behavior — no
/// global admission gate, ONNX Runtime session threading left at
/// onnxruntime's defaults (one intra-op pool of ~physical-core threads per
/// session, spin-wait enabled) — which is what standalone ninference nodes
/// want. A host that embeds the engine *next to another workload* (the
/// postvec PostgreSQL launcher) opts into conservative bounds so inference
/// cannot oversubscribe the machine: requests for different models can enter
/// blocking execution concurrently, and each session's intra-op pool
/// multiplies with the core count, so an unbounded embedded engine competes
/// with the database for every core.
#[derive(Debug, Clone, Default)]
pub struct HostPolicy {
    /// Global cap on concurrently *executing* predictions across all models
    /// and routes. `None` = unlimited (historical behavior). The permit is
    /// held for the true duration of the native execution — a caller whose
    /// deadline expires does not release capacity while onnxruntime is still
    /// running its computation.
    pub admission_limit: Option<usize>,
    /// Serialize the *native instantiation* phase of dynamic model loads
    /// behind the load-commit gate. `false` (default) lets concurrent loads
    /// of different models instantiate natively in parallel — the standalone
    /// throughput behavior; publication + executor build still serialize. A
    /// shared host (the postvec embedded launcher) sets `true` so a
    /// cancelled load cannot admit a second native instantiation while the
    /// first is still running on a blocking thread.
    pub serialized_model_loads: bool,
}

/// Per-session ONNX Runtime threading policy, applied to every session built
/// after it is set. Process-global because model construction happens deep in
/// pool factories; set it once, before the first model loads. Standalone
/// ninference never sets it and keeps onnxruntime's defaults.
#[derive(Debug, Clone, Copy)]
pub struct SessionThreadPolicy {
    /// Intra-op thread count per session (onnxruntime default: ~physical cores).
    pub intra_op_threads: usize,
    /// Disable the intra-op pool's spin-wait. Spinning trades idle CPU burn
    /// for lower inference latency — the right trade on a dedicated
    /// inference node, the wrong one on a shared database host, where the
    /// post-inference spin tail reads as "CPU busy while doing nothing".
    pub disable_spinning: bool,
}

static SESSION_THREAD_POLICY: std::sync::OnceLock<SessionThreadPolicy> = std::sync::OnceLock::new();

/// Install the process-wide session threading policy. Call before any model
/// loads; later calls are ignored (sessions already built keep their pools).
pub fn set_session_thread_policy(policy: SessionThreadPolicy) {
    let _ = SESSION_THREAD_POLICY.set(policy);
}

/// The installed policy, if any.
pub fn session_thread_policy() -> Option<SessionThreadPolicy> {
    SESSION_THREAD_POLICY.get().copied()
}

/// Represents the engine's configuration.
///
/// This struct holds all the settings necessary for the `InferenceEngine` to
/// locate models and their assets.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// The root path from which all other paths (like the models directory) are resolved.
    pub root_path: PathBuf,
    /// Resource bounds for the hosting process; `HostPolicy::default()` keeps
    /// the historical unbounded behavior.
    pub host_policy: HostPolicy,
}
