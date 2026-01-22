//! The `/config` discovery endpoint over HTTPS.
//!
//! The envelope is lifted from postvec's embedded loopback listener
//! (postvec/src/client/embedded/http.rs::config_envelope) and is
//! byte-compatible with what postvec's `discovery::parse_config` expects:
//! `configuration.enabled` gates inclusion parser-side and
//! `configuration.params` carries model_type/source/target/dims.
//!
//! Served over TLS with a self-signed certificate because that is what a
//! production ninference node does (`ssl.active` in ninference.json) and
//! postvec's discovery accepts such certificates on a trusted network —
//! plain HTTP here would leave that path untested.

use axum::routing::get;
use axum::{Json, Router};
use engine::{InferenceEngine, ModelConfiguration};
use serde_json::{json, Value};
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

/// Render the `/config` envelope for a set of model configurations.
fn config_envelope(configs: &[ModelConfiguration]) -> Value {
    let models: Vec<Value> = configs
        .iter()
        .map(|cfg| {
            json!({
                "name": cfg.name,
                "status": "local",
                "configuration": serde_json::to_value(cfg).unwrap_or(Value::Null),
            })
        })
        .collect();
    json!({ "success": true, "data": { "models": models } })
}

fn engine_envelope(engine: &InferenceEngine) -> Value {
    let configs: Vec<ModelConfiguration> = engine
        .get_active_models()
        .into_iter()
        .filter_map(|name| engine.get_model(&name).ok())
        .map(|model| model.configuration().clone())
        .collect();
    config_envelope(&configs)
}

pub async fn serve(
    engine: Arc<InferenceEngine>,
    addr: SocketAddr,
    cert: &Path,
    key: &Path,
) -> Result<(), String> {
    let app = Router::new().route(
        "/config",
        get(move || {
            let engine = engine.clone();
            async move { Json(engine_envelope(&engine)) }
        }),
    );
    let tls = axum_server::tls_openssl::OpenSSLConfig::from_pem_file(cert, key).map_err(|e| {
        format!(
            "TLS config from {} / {}: {e}",
            cert.display(),
            key.display()
        )
    })?;
    log::info!("fixture /config listening on https://{addr}");
    axum_server::bind_openssl(addr, tls)
        .serve(app.into_make_service())
        .await
        .map_err(|e| format!("/config server exited: {e}"))
}
