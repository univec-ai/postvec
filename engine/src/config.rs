//! Engine-side configuration, independent of the host that runs it.

use std::path::PathBuf;

/// Resource policy for the process hosting the engine.
///
/// Default is unbounded: no admission gate, ONNX Runtime threading left at
/// onnxruntime's defaults (one intra-op pool of about physical-core threads
/// per session, spin-wait on). That is what a dedicated inference node wants.
/// A host that sits next to another workload (the postvec PostgreSQL
/// launcher) opts into tighter bounds. Requests for different models can
/// enter blocking execution at the same time, and each session's intra-op
/// pool multiplies with the core count, so an unbounded engine would compete
/// with the database for every core.
#[derive(Debug, Clone, Default)]
pub struct HostPolicy {
    /// Cap on concurrently executing predictions across all models. `None`
    /// means unlimited. The permit is held for the native run itself: a
    /// caller whose deadline expires does not free capacity while onnxruntime
    /// is still computing.
    pub admission_limit: Option<usize>,
    /// Hold the load-commit gate across native instantiation as well as
    /// publication. `false` (default) lets concurrent loads of different
    /// models instantiate in parallel; publication and executor build still
    /// serialize. A shared host (the postvec embedded launcher) sets `true`
    /// so a cancelled load cannot start a second native instantiation while
    /// the first is still running on a blocking thread.
    pub serialized_model_loads: bool,
    /// Engine-wide ceiling on tokens fed to a model per input, applied on
    /// top of each descriptor's `tokenizer.max_length` or `params.sequence_len`.
    /// `None` (the default) uses [`DEFAULT_MAX_SEQUENCE_LEN`]. `Some(0)` means
    /// no ceiling. `Some(n)` means n tokens. Attention memory grows with the
    /// square of the length, so a descriptor that declares 32k tokens still
    /// cannot admit a 32k-token input on a host that cannot hold it.
    pub max_sequence_len: Option<usize>,
}

/// Ceiling used when [`HostPolicy::max_sequence_len`] is unset.
pub const DEFAULT_MAX_SEQUENCE_LEN: usize = 8192;

impl HostPolicy {
    /// The effective engine-wide token ceiling, or `None` when unlimited.
    pub fn sequence_len_cap(&self) -> Option<usize> {
        match self.max_sequence_len {
            None => Some(DEFAULT_MAX_SEQUENCE_LEN),
            Some(0) => None,
            Some(n) => Some(n),
        }
    }
}

/// Per-session ONNX Runtime threading policy, applied to every session built
/// after it is set. Process-global because model construction happens deep in
/// pool factories; set it once, before the first model loads. Standalone
/// ninference never sets it and keeps onnxruntime's defaults.
#[derive(Debug, Clone, Copy)]
pub struct SessionThreadPolicy {
    /// Intra-op thread count per session (onnxruntime default: ~physical cores).
    pub intra_op_threads: usize,
    /// Disable the intra-op pool's spin-wait. Spinning burns idle CPU for
    /// lower latency: right on a dedicated inference node, wrong on a
    /// shared database host (the post-inference spin looks like busy CPU).
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

/// Paths and host policy the `InferenceEngine` needs to locate models.
#[derive(Debug, Clone)]
pub struct EngineConfig {
    /// Root for the models directory and other relative paths.
    pub root_path: PathBuf,
    /// Resource bounds; `HostPolicy::default()` is unbounded.
    pub host_policy: HostPolicy,
}
