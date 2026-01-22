//! Tests for `engine::error::EngineError::to_error_code` — the mapping from
//! internal engine errors to the cross-service `shared::ErrorCode` carried back
//! to aphex (and ultimately the client) via gRPC metadata.

use engine::error::EngineError;
use shared::ErrorCode;

#[test]
fn not_found_maps_to_model_not_found() {
    assert_eq!(
        EngineError::NotFound("nope".into()).to_error_code(),
        ErrorCode::ModelNotFound
    );
}

#[test]
fn input_type_and_json_map_to_invalid_input() {
    assert_eq!(
        EngineError::InputTypeError("bad".into()).to_error_code(),
        ErrorCode::InvalidInput
    );
    let json_err: EngineError = serde_json::from_str::<serde_json::Value>("not json")
        .unwrap_err()
        .into();
    assert_eq!(json_err.to_error_code(), ErrorCode::InvalidInput);
}

#[test]
fn configuration_and_io_map_to_internal() {
    assert_eq!(
        EngineError::Configuration("x".into()).to_error_code(),
        ErrorCode::InternalError
    );
    let io: EngineError = std::io::Error::other("disk").into();
    assert_eq!(io.to_error_code(), ErrorCode::InternalError);
}

#[test]
fn prediction_context_length_detected() {
    // Case-insensitive substring match on "context" + "length".
    assert_eq!(
        EngineError::Prediction("Context Length exceeded for input".into()).to_error_code(),
        ErrorCode::ContextLengthExceeded
    );
}

#[test]
fn prediction_gpu_oom_detected() {
    assert_eq!(
        EngineError::Prediction("CUDA out of memory while allocating".into()).to_error_code(),
        ErrorCode::GpuOutOfMemory
    );
}

#[test]
fn generic_prediction_maps_to_internal() {
    assert_eq!(
        EngineError::Prediction("something else broke".into()).to_error_code(),
        ErrorCode::InternalError
    );
}

#[test]
fn prediction_partial_keywords_do_not_misfire() {
    // "context" without "length" must NOT map to ContextLengthExceeded.
    assert_eq!(
        EngineError::Prediction("invalid context window config".into()).to_error_code(),
        ErrorCode::InternalError
    );
}
