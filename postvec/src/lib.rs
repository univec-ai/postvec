use pgrx::prelude::*;

::pgrx::pg_module_magic!(name, version);

/// Generated tonic/prost stubs for the vendored inference proto.
// Generated code returns `Result<_, tonic::Status>`; newer clippy flags the
// error variant as large, and that signature is tonic's to choose.
#[allow(clippy::result_large_err)]
pub mod proto {
    tonic::include_proto!("ninference");
}

pub mod api;
pub mod chunking;
pub mod client;
pub mod gucs;
pub mod jobs;
pub mod registry;
pub mod runtime;
pub mod schema;
pub mod worker;

#[cfg(feature = "pg_test_concurrency")]
mod tests_concurrency;
#[cfg(any(test, feature = "pg_test"))]
mod tests_triggers;

/// Called on library load. GUCs must be registered here; static background
/// workers can only be registered while shared_preload_libraries is being
/// processed in the postmaster.
#[pg_guard]
pub extern "C-unwind" fn _PG_init() {
    gucs::register();
    if unsafe { pg_sys::process_shared_preload_libraries_in_progress } {
        worker::register_static_worker();
    }
}

#[pg_extern]
fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Machine-readable build metadata for tooling that needs to know what this
/// `postvec.so` can actually do.
///
/// The load-bearing field is `features.embedded`: `postvec.mode = 'embedded'`
/// only works when the library was compiled with the `embedded` feature. A
/// thin artifact asked for embedded mode parks its worker with a warning.
/// File size, symbols and server logs are all guesses, so the library reports
/// this itself.
///
/// `diagnostics_api` versions this JSON contract independently of the
/// extension version. Bump it only when the shape of the document changes
/// incompatibly.
#[pg_extern]
fn build_info() -> pgrx::JsonB {
    // Backends this build's embedded engine can load. The registry client
    // refuses to download a model for a backend that is not listed here.
    // A thin build hosts no engine, so the list is empty.
    let model_backends: Vec<&str> = if cfg!(feature = "embedded") {
        vec!["onnx-runtime", "generic"]
    } else {
        vec![]
    };
    pgrx::JsonB(serde_json::json!({
        "version": env!("CARGO_PKG_VERSION"),
        "diagnostics_api": 1,
        "features": {
            "embedded": cfg!(feature = "embedded"),
            "model_backends": model_backends,
        },
    }))
}

#[cfg(any(test, feature = "pg_test"))]
#[pg_schema]
mod tests {
    use pgrx::prelude::*;

    #[pg_test]
    fn test_version() {
        assert_eq!(env!("CARGO_PKG_VERSION"), crate::version());
    }

    /// The report must match how this build was compiled. CI runs this on
    /// both the thin and `--features embedded` artifacts.
    #[pg_test]
    fn build_info_matches_features() {
        let info = crate::build_info().0;
        assert_eq!(info["version"], env!("CARGO_PKG_VERSION"));
        assert_eq!(info["diagnostics_api"], 1);
        assert_eq!(info["features"]["embedded"], cfg!(feature = "embedded"));
        let backends = info["features"]["model_backends"]
            .as_array()
            .expect("model_backends is a list");
        if cfg!(feature = "embedded") {
            assert!(backends.iter().any(|b| b == "onnx-runtime"));
            assert!(backends.iter().any(|b| b == "generic"));
        } else {
            assert!(backends.is_empty(), "a thin build loads nothing in-process");
        }
    }

    /// Tooling reads this through SQL, so the function has to be callable.
    #[pg_test]
    fn build_info_is_callable_from_sql() {
        let embedded = Spi::get_one::<bool>(
            "SELECT (postvec.build_info() -> 'features' ->> 'embedded')::bool",
        )
        .unwrap();
        assert_eq!(embedded, Some(cfg!(feature = "embedded")));
    }

    /// The vendored proto must stay byte-identical to the canonical copy at
    /// the repository root. postvec compiles its own stubs, but the wire
    /// contract is the canonical one.
    #[pg_test]
    fn test_proto_drift() {
        let vendored = include_str!("../proto/ninference.proto");
        let canonical = std::fs::read_to_string(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../proto/ninference.proto"
        ))
        .expect(
            "canonical proto missing — postvec must live inside the postvec repo for this test",
        );
        assert_eq!(
            vendored, canonical,
            "postvec/proto/ninference.proto has drifted from proto/ninference.proto"
        );
    }
}

/// Required by `cargo pgrx test`; must live at the crate root.
#[cfg(test)]
pub mod pg_test {
    pub fn setup(_options: Vec<&str>) {}

    /// The suite runs against a mock inference client and `/config` fixtures —
    /// no engine, no model assets, no reachable node. `postvec.mode` is pinned
    /// to that reality rather than inherited from the extension's default,
    /// which is `embedded`: a thin build would park its workers, and an
    /// `--features embedded` build would spend every test retrying an engine
    /// init against a root the test cluster does not have.
    ///
    /// Pinning it also decouples the suite from the default, so changing the
    /// default is a decision about products rather than a decision about
    /// whether several hundred tests still pass.
    #[must_use]
    pub fn postgresql_conf_options() -> Vec<&'static str> {
        vec!["postvec.mode = 'grpc'"]
    }
}
