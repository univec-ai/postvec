//! ## Dummy Executor
//!
//! A placeholder executor that performs no actual inference and always returns
//! an error. It is useful for disabling a model or as a default implementation
//! during development.
use super::{Context, Executor, ExecutorOutput};
use crate::error::EngineError;

/// An executor that performs no actual inference and always returns an error.
/// Useful as a placeholder or for disabled models.
pub struct DummyExecutor;

impl DummyExecutor {
    /// The constructor for the DummyExecutor.
    pub fn new() -> Self {
        Self
    }
}

impl Executor for DummyExecutor {
    /// Implementation of the execute method for the DummyExecutor.
    /// It ignores the input and immediately returns a prediction error,
    /// indicating that the associated model has no functionality.
    fn execute(&self, ctx: &Context) -> Result<ExecutorOutput, EngineError> {
        Err(EngineError::Prediction(format!(
            "Model '{}' uses a dummy executor and has no functionality.",
            ctx.model_name()
        )))
    }
}
