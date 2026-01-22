//! A real inference node for the packaging pipeline's remote-mode tests:
//! the canonical gRPC contract on one port, the `/config` discovery envelope
//! over HTTPS on another, serving whatever models sit under the engine root.
//!
//! Test-only. Never published, no compatibility promise. See
//! fixtures/inference-server/Cargo.toml for what is lifted from where.

mod config_http;
mod grpc;
mod limits;

pub mod proto {
    tonic::include_proto!("ninference");
}

use engine::{EngineConfig, InferenceEngine};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;

fn env_or(name: &str, default: &str) -> String {
    std::env::var(name).unwrap_or_else(|_| default.to_string())
}

/// Every enabled model name under `<root>/models/<backend>/<name>/`, from the
/// same descriptor filename the engine loads (`ninference.hub.json`). The
/// fixture's assets are the reviewed payloads, so a malformed descriptor is a
/// startup error, not something to skip quietly.
fn scan_enabled_models(root: &Path) -> anyhow::Result<Vec<String>> {
    #[derive(serde::Deserialize)]
    struct Descriptor {
        name: String,
        #[serde(default)]
        enabled: bool,
    }

    let models_dir = root.join("models");
    let mut names = Vec::new();
    for backend in std::fs::read_dir(&models_dir)? {
        let backend = backend?.path();
        if !backend.is_dir() {
            continue;
        }
        for model in std::fs::read_dir(&backend)? {
            let descriptor = model?.path().join("ninference.hub.json");
            if !descriptor.is_file() {
                continue;
            }
            let parsed: Descriptor =
                serde_json::from_str(&std::fs::read_to_string(&descriptor)?)
                    .map_err(|e| anyhow::anyhow!("{}: {e}", descriptor.display()))?;
            if parsed.enabled {
                names.push(parsed.name);
            }
        }
    }
    names.sort();
    Ok(names)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();

    let root = PathBuf::from(env_or("NINFERENCE_PATH", "/ninference"));
    let grpc_addr: SocketAddr = env_or("FIXTURE_GRPC_ADDR", "0.0.0.0:33333").parse()?;
    let http_addr: SocketAddr = env_or("FIXTURE_HTTP_ADDR", "0.0.0.0:22222").parse()?;
    let cert = PathBuf::from(env_or(
        "FIXTURE_TLS_CERT",
        &root.join("certs/ninference.crt").to_string_lossy(),
    ));
    let key = PathBuf::from(env_or(
        "FIXTURE_TLS_KEY",
        &root.join("certs/ninference.key").to_string_lossy(),
    ));
    let predict_timeout = Duration::from_millis(
        env_or("FIXTURE_PREDICT_TIMEOUT_MS", "30000")
            .parse::<u64>()
            .map_err(|e| anyhow::anyhow!("FIXTURE_PREDICT_TIMEOUT_MS: {e}"))?,
    );
    let max_inflight: usize = env_or("FIXTURE_MAX_INFLIGHT", "4")
        .parse()
        .map_err(|e| anyhow::anyhow!("FIXTURE_MAX_INFLIGHT: {e}"))?;

    engine::initialize_onnx(&root).map_err(|e| anyhow::anyhow!("ONNX Runtime init: {e}"))?;

    let models = scan_enabled_models(&root)?;
    anyhow::ensure!(
        !models.is_empty(),
        "no enabled model descriptors under {}/models",
        root.display()
    );
    log::info!("loading {} model(s): {}", models.len(), models.join(", "));

    // Standalone-node shape: no admission gate, throughput session defaults.
    // The host policy exists for engines sharing a database host, which this
    // fixture does not.
    let engine = Arc::new(
        InferenceEngine::default_with_models(
            Arc::new(EngineConfig {
                root_path: root.clone(),
                host_policy: Default::default(),
            }),
            &models,
        )
        .await
        .map_err(|e| anyhow::anyhow!("model preload: {e}"))?,
    );
    log::info!("active models: {}", engine.get_active_models().join(", "));

    let grpc = tokio::spawn(grpc::serve(
        engine.clone(),
        grpc_addr,
        predict_timeout,
        max_inflight,
    ));
    let http = tokio::spawn({
        let engine = engine.clone();
        async move { config_http::serve(engine, http_addr, &cert, &key).await }
    });

    tokio::select! {
        r = grpc => anyhow::bail!("gRPC server stopped: {:?}", r?),
        r = http => anyhow::bail!("/config server stopped: {:?}", r?),
        _ = tokio::signal::ctrl_c() => {
            log::info!("shutting down");
            Ok(())
        }
    }
}
