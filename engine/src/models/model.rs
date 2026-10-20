//! Shared contract for every model backend.
use super::configuration::{ModelBackend, ModelConfiguration, ModelOverview};
use super::error::ModelError;
use serde_json::Value;
use shared::vectors::GenericTensor;

/// Inference, configuration and metadata for a loaded model. `Send + Sync`
/// so instances can sit in the pool and be used from executor threads.
pub trait Model: Send + Sync {
    /// Ready for inference. False if files are missing, load failed or
    /// configuration is incomplete.
    fn valid(&self) -> bool;

    fn name(&self) -> &str;

    fn backend(&self) -> &ModelBackend;

    fn configuration(&self) -> &ModelConfiguration;

    /// Run inference. One tensor per input layer.
    fn query(&self, inputs: &[GenericTensor]) -> Result<Vec<GenericTensor>, ModelError>;

    /// Deadline-aware inference. The default calls [`Model::query`] and
    /// cannot stop mid-run. ONNX stops the native run so a timed-out caller
    /// gets its lease and admission permit back. The bound carries a tokio
    /// handle: queries run on Rayon workers with no ambient runtime.
    fn query_with_deadline(
        &self,
        inputs: &[GenericTensor],
        bound: Option<&QueryBound>,
    ) -> Result<Vec<GenericTensor>, ModelError> {
        let _ = bound;
        self.query(inputs)
    }

    fn overview(&self) -> Result<ModelOverview, ModelError>;

    fn param_int_or_default(&self, param: &str, default_value: i64) -> i64;
    fn param_f32_or_default(&self, param: &str, default_value: f32) -> f32;
    fn param_bool_or_default(&self, param: &str, default_value: bool) -> bool;
    fn param_string_or_default(&self, param: &str, default_value: &str) -> String;
    fn param_string_list_or_default(&self, param: &str, default_value: &[&str]) -> Vec<String>;
    fn param_list_or_default(&self, param: &str, default_value: &[Value]) -> Vec<Value>;
}

/// Deadline and watchdog runtime for every model call in a request,
/// including nested bridge work. Cloneable so it can be passed into Rayon
/// pool closures.
#[derive(Debug, Clone)]
pub struct QueryBound {
    /// Absolute deadline of the root request.
    pub deadline: std::time::Instant,
    /// Runtime that schedules the deadline watchdog. Carried explicitly:
    /// Rayon workers have no ambient tokio context.
    pub runtime: tokio::runtime::Handle,
}
