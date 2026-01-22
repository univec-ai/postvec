//! End-to-end tests for `shared::ErrorCode` — the cross-service error code
//! exchanged between ninference and aphex (via gRPC metadata).

use shared::ErrorCode;

const ALL: &[ErrorCode] = &[
    ErrorCode::InternalError,
    ErrorCode::InvalidInput,
    ErrorCode::Timeout,
    ErrorCode::ModelNotFound,
    ErrorCode::ModelNotLoaded,
    ErrorCode::ModelDisabled,
    ErrorCode::ContextLengthExceeded,
    ErrorCode::GpuOutOfMemory,
    ErrorCode::CpuOverload,
    ErrorCode::BridgePathNotFound,
    ErrorCode::ConverterNotFound,
    ErrorCode::UpstreamServiceUnavailable,
];

#[test]
fn as_str_from_str_roundtrip_for_all_variants() {
    for &code in ALL {
        let s = code.as_str();
        assert_eq!(
            ErrorCode::from_str(s),
            Some(code),
            "round-trip failed for {code:?} via wire string {s:?}"
        );
    }
}

#[test]
fn wire_strings_are_screaming_snake_case() {
    assert_eq!(ErrorCode::InternalError.as_str(), "INTERNAL_ERROR");
    assert_eq!(
        ErrorCode::BridgePathNotFound.as_str(),
        "BRIDGE_PATH_NOT_FOUND"
    );
    assert_eq!(
        ErrorCode::UpstreamServiceUnavailable.as_str(),
        "UPSTREAM_SERVICE_UNAVAILABLE"
    );
}

#[test]
fn wire_strings_are_unique() {
    let mut strings: Vec<&str> = ALL.iter().map(|c| c.as_str()).collect();
    strings.sort_unstable();
    let count = strings.len();
    strings.dedup();
    assert_eq!(strings.len(), count, "duplicate wire strings detected");
}

#[test]
fn from_str_unknown_is_none() {
    assert_eq!(ErrorCode::from_str("NOPE"), None);
    assert_eq!(ErrorCode::from_str(""), None);
    // The Debug/Display form is NOT a valid wire string for `from_str`.
    assert_eq!(ErrorCode::from_str("InternalError"), None);
}

#[test]
fn display_uses_debug_variant_name() {
    // `Display` is the variant name (Debug), distinct from the wire `as_str`.
    assert_eq!(ErrorCode::InternalError.to_string(), "InternalError");
    assert_eq!(ErrorCode::Timeout.to_string(), "Timeout");
}

#[test]
fn serde_uses_variant_name_not_wire_string() {
    // serde derive serialises the enum as its variant name. This is a separate
    // representation from the gRPC wire string (`as_str`); documenting it guards
    // against accidentally conflating the two encodings.
    let json = serde_json::to_string(&ErrorCode::ModelNotFound).unwrap();
    assert_eq!(json, "\"ModelNotFound\"");
    let back: ErrorCode = serde_json::from_str(&json).unwrap();
    assert_eq!(back, ErrorCode::ModelNotFound);
}

#[test]
fn error_code_is_copy_and_eq() {
    let a = ErrorCode::Timeout;
    let b = a; // Copy
    assert_eq!(a, b);
    assert_ne!(ErrorCode::Timeout, ErrorCode::InternalError);
}
