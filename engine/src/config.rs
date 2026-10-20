//! Engine-side configuration, independent of the host that runs it.

use std::path::PathBuf;

/// Resource policy for the process hosting the engine.
///
/// Default is unbounded, which suits a dedicated inference node. A host
/// that shares the machine with PostgreSQL sets tighter bounds.
#[derive(Debug, Clone, Default)]
pub struct HostPolicy {
    /// Cap on predictions in native execution. `None` is unlimited. The
    /// permit stays held until onnxruntime finishes, even after the deadline.
    pub admission_limit: Option<usize>,
    /// Hold the load-commit gate across native instantiation, not just
    /// publication. Default `false` lets different models instantiate in
    /// parallel. A shared host sets `true` so a cancelled load cannot start
    /// a second native instantiation while the first is still running.
    pub serialized_model_loads: bool,
    /// Engine-wide token ceiling per input, on top of each descriptor's
    /// `tokenizer.max_length` or `params.sequence_len`. `None` uses
    /// [`DEFAULT_MAX_SEQUENCE_LEN`]. `Some(0)` means no ceiling. Attention
    /// memory grows with the square of the length.
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

/// Per-session ONNX Runtime threading, applied to every session built
/// after it is set. Process-global, because construction happens inside
/// pool factories. Set it once, before the first load. Standalone
/// ninference leaves onnxruntime's defaults.
#[derive(Debug, Clone, Copy)]
pub struct SessionThreadPolicy {
    /// Intra-op thread count per session (onnxruntime default: ~physical cores).
    pub intra_op_threads: usize,
    /// Turn off the intra-op pool's spin-wait. Spinning buys latency with
    /// idle CPU: fine on a dedicated node, wrong next to PostgreSQL.
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
