//! Placeholder executor: always errors, used when a model has no real work.
use super::{Context, Executor, ExecutorOutput};
use crate::error::EngineError;

pub struct DummyExecutor;

impl DummyExecutor {
    pub fn new() -> Self {
        Self
    }
}

impl Executor for DummyExecutor {
    fn execute(&self, ctx: &Context) -> Result<ExecutorOutput, EngineError> {
        Err(EngineError::Prediction(format!(
            "Model '{}' uses a dummy executor and has no functionality.",
            ctx.model_name()
        )))
    }
}
