//! Golden path against a real model root. Ignored by default; needs ONNX
//! and a model.
//!
//! ```console
//! POSTVEC_SERVER_TEST_ROOT=/var/lib/postvec-server \
//! POSTVEC_SERVER_TEST_MODEL=sentence-transformers-all-minilm-l6-v2 \
//!   cargo test -p postvec-server --test live_engine -- --ignored --nocapture
//! ```
//!
//! Both variables default to a stock install, so `-- --ignored` is enough
//! on a machine that already runs a node.

use postvec_server::metrics::Metrics;
use postvec_server::proto::ninference_service_client::NinferenceServiceClient;
use postvec_server::proto::EmbedTextsRequest;
use prost_types::value::Kind;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tonic::Request;

fn root() -> PathBuf {
    PathBuf::from(
        std::env::var("POSTVEC_SERVER_TEST_ROOT").unwrap_or_else(|_| "/engine".to_string()),
    )
}

fn model() -> String {
    std::env::var("POSTVEC_SERVER_TEST_MODEL")
        .unwrap_or_else(|_| "sentence-transformers-all-minilm-l6-v2".to_string())
}

/// `initialize_onnx` dlopens the shared library into the process. Tests run
/// concurrently in one process, so it happens exactly once.
fn init_onnx(root: &std::path::Path) {
    static ONCE: std::sync::Once = std::sync::Once::new();
    static mut RESULT: Option<String> = None;
    ONCE.call_once(|| {
        let outcome = engine::initialize_onnx(root).err().map(|e| e.to_string());
        // SAFETY: written once inside `call_once`, read only afterwards.
        unsafe { RESULT = outcome };
    });
    #[allow(static_mut_refs)]
    if let Some(e) = unsafe { RESULT.as_ref() } {
        panic!(
            "cannot initialise ONNX Runtime from {}/libs: {e}\n\
             (set POSTVEC_SERVER_TEST_ROOT to a tree that has one)",
            root.display()
        );
    }
}

struct Live {
    endpoint: String,
    model: String,
    engine: Arc<engine::InferenceEngine>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<Result<(), String>>,
}

impl Live {
    async fn start() -> Self {
        let root = root();
        let model = model();
        init_onnx(&root);

        let engine = Arc::new(engine::InferenceEngine::new(Arc::new(
            engine::EngineConfig {
                root_path: root.clone(),
                host_policy: Default::default(),
            },
        )));
        engine
            .load_model(&model)
            .await
            .unwrap_or_else(|e| panic!("cannot load {model:?} from {}: {e}", root.display()));
        assert!(
            engine.is_model_ready(&model),
            "{model:?} loaded but is not ready"
        );

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let bound = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(postvec_server::grpc::serve(
            engine.clone(),
            Arc::new(Metrics::new()),
            listener,
            Duration::from_secs(120),
            4,
            Arc::new(providers::gateway::Gateway::empty()),
            async {
                let _ = rx.await;
            },
        ));

        let live = Self {
            endpoint: format!("http://{bound}"),
            model,
            engine,
            shutdown: Some(tx),
            task,
        };
        for attempt in 0..300 {
            if NinferenceServiceClient::connect(live.endpoint.clone())
                .await
                .is_ok()
            {
                break;
            }
            assert!(attempt < 299, "gRPC listener never came up");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        live
    }

    async fn embed(&self, texts: &[&str]) -> Vec<Vec<f64>> {
        let mut client = NinferenceServiceClient::connect(self.endpoint.clone())
            .await
            .expect("connect");
        let response = client
            .embed_texts(Request::new(EmbedTextsRequest {
                model: self.model.clone(),
                texts: texts.iter().map(|t| t.to_string()).collect(),
                ..Default::default()
            }))
            .await
            .expect("EmbedTexts")
            .into_inner();

        response
            .embeddings
            .expect("embeddings present")
            .values
            .into_iter()
            .map(|row| match row.kind {
                Some(Kind::ListValue(inner)) => inner
                    .values
                    .into_iter()
                    .map(|v| match v.kind {
                        Some(Kind::NumberValue(n)) => n,
                        other => panic!("expected a number, got {other:?}"),
                    })
                    .collect(),
                other => panic!("expected a nested list, got {other:?}"),
            })
            .collect()
    }

    async fn stop(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        let _ = tokio::time::timeout(Duration::from_secs(10), self.task).await;
    }
}

fn cosine(a: &[f64], b: &[f64]) -> f64 {
    let dot: f64 = a.iter().zip(b).map(|(x, y)| x * y).sum();
    let na: f64 = a.iter().map(|x| x * x).sum::<f64>().sqrt();
    let nb: f64 = b.iter().map(|x| x * x).sum::<f64>().sqrt();
    dot / (na * nb)
}

/// The whole point of the process: text in over gRPC, vectors out.
#[tokio::test]
#[ignore = "needs a real model root; see the module docs"]
async fn embeddings_come_back_over_grpc() {
    let live = Live::start().await;
    let vectors = live
        .embed(&[
            "quarterly revenue guidance was raised",
            "embedding model migration without source re-embedding",
        ])
        .await;

    assert_eq!(vectors.len(), 2, "one vector per input, in order");
    let dim = vectors[0].len();
    assert!(dim > 0, "empty embedding");
    assert_eq!(vectors[1].len(), dim, "ragged output");
    assert!(
        vectors.iter().flatten().all(|v| v.is_finite()),
        "a non-finite component would poison every downstream distance"
    );
    assert!(
        vectors[0].iter().any(|v| *v != 0.0),
        "an all-zero vector means the model ran but produced nothing"
    );

    // Two unrelated sentences must not land on top of each other. This is a
    // sanity floor, not a quality claim — it catches a pooling or template
    // path that collapsed, which is the failure mode a shape check misses.
    let similarity = cosine(&vectors[0], &vectors[1]);
    assert!(
        similarity < 0.95,
        "unrelated sentences at cosine {similarity:.4}; the embedding path has collapsed"
    );
    println!("dim {dim}, cross-sentence cosine {similarity:.4}");

    live.stop().await;
}

/// The same text must produce the same vector. A node that is not
/// deterministic cannot be part of a fleet: postvec round-robins, so
/// per-node variation becomes per-request variation in a customer's index.
#[tokio::test]
#[ignore = "needs a real model root; see the module docs"]
async fn embeddings_are_deterministic() {
    let live = Live::start().await;
    let text = "postvec keeps a shadow vector column in sync";
    let first = live.embed(&[text]).await;
    let second = live.embed(&[text]).await;
    assert_eq!(first[0].len(), second[0].len());
    let similarity = cosine(&first[0], &second[0]);
    assert!(
        similarity > 0.999_999,
        "the same text produced different vectors (cosine {similarity})"
    );
    live.stop().await;
}

/// Batching must not change the answer: a two-text request has to return what
/// two one-text requests would. postvec sub-batches, so a batch-dependent
/// result would make a column's vectors depend on how the queue happened to
/// group its rows.
#[tokio::test]
#[ignore = "needs a real model root; see the module docs"]
async fn batching_does_not_change_the_result() {
    let live = Live::start().await;
    let a = "hybrid search over a text column";
    let b = "migrating stored vectors between embedding models";

    let batched = live.embed(&[a, b]).await;
    let single_a = live.embed(&[a]).await;
    let single_b = live.embed(&[b]).await;

    for (label, batched, single) in [
        ("first", &batched[0], &single_a[0]),
        ("second", &batched[1], &single_b[0]),
    ] {
        let similarity = cosine(batched, single);
        assert!(
            similarity > 0.999_99,
            "the {label} vector differs between a batch and a single request (cosine \
             {similarity}); batching must not be observable"
        );
    }
    live.stop().await;
}

/// The discovery envelope, rendered from a real loaded model — the shape
/// postvec's `refresh_models()` reads to populate its SQL cache.
#[tokio::test]
#[ignore = "needs a real model root; see the module docs"]
async fn config_advertises_the_loaded_model_with_its_parameters() {
    let live = Live::start().await;
    let configs: Vec<engine::ModelConfiguration> = live
        .engine
        .get_active_models()
        .into_iter()
        .filter_map(|name| live.engine.get_model(&name).ok())
        .map(|m| m.configuration().clone())
        .collect();
    let rendered = postvec_server::http::config_models(&configs);

    let entry = rendered
        .iter()
        .find(|m| m["name"] == serde_json::json!(live.model))
        .unwrap_or_else(|| panic!("{} not advertised: {rendered:?}", live.model));
    assert_eq!(entry["status"], serde_json::json!("local"));
    assert_eq!(
        entry["configuration"]["enabled"],
        serde_json::json!(true),
        "a loaded model must advertise as enabled, or postvec's parser filters it out"
    );
    println!("{}", serde_json::to_string_pretty(entry).unwrap());
    live.stop().await;
}

/// HTTP native + OpenAI adaptor against a real model. Same engine as the
/// gRPC tests; a second listener on an ephemeral port.
#[tokio::test]
#[ignore = "needs a real model root; see the module docs"]
async fn embeddings_come_back_over_http() {
    use postvec_server::cli::ServeArgs;
    use postvec_server::config::{self, FileConfig};
    use postvec_server::metrics::Metrics;
    use postvec_server::net;
    use postvec_server::state::{NodeIdentity, ServerState};
    use serde_json::{json, Value};
    use std::collections::BTreeMap;
    use std::sync::Arc;

    let root = root();
    let model = model();
    init_onnx(&root);

    let engine = Arc::new(engine::InferenceEngine::new(Arc::new(
        engine::EngineConfig {
            root_path: root.clone(),
            host_policy: Default::default(),
        },
    )));
    engine
        .load_model(&model)
        .await
        .unwrap_or_else(|e| panic!("cannot load {model:?}: {e}"));

    let flags = ServeArgs {
        insecure: true,
        bind: Some("127.0.0.1".into()),
        ..Default::default()
    };
    let settings = Arc::new(
        config::resolve(
            &flags,
            &FileConfig::default(),
            &BTreeMap::new(),
            root.clone(),
            None,
        )
        .expect("settings"),
    );
    let advertise = net::resolve_advertise(&settings).unwrap();
    let identity = NodeIdentity::new(&settings, advertise);
    let state = ServerState::new(
        engine,
        settings.clone(),
        identity,
        Arc::new(Metrics::new()),
        None,
        Arc::new(providers::gateway::Gateway::empty()),
    );
    let socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let public = postvec_server::http::spawn(state, socket).unwrap();
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(60))
        .build()
        .unwrap();
    let base = format!("http://{}", public.bound);
    for attempt in 0..200 {
        if client.get(format!("{base}/health")).send().await.is_ok() {
            break;
        }
        assert!(attempt < 199, "HTTP listener never came up");
        tokio::time::sleep(Duration::from_millis(10)).await;
    }

    let native: Value = client
        .post(format!("{base}/api/{model}"))
        .json(&json!({ "texts": ["quarterly revenue guidance was raised"] }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(native["success"], json!(true), "{native}");
    let embeddings = native["data"]["embeddings"]
        .as_array()
        .expect("native embeddings");
    assert_eq!(embeddings.len(), 1);
    assert!(!embeddings[0].as_array().unwrap().is_empty());

    let openai: Value = client
        .post(format!("{base}/api/openai/embeddings"))
        .json(&json!({ "model": format!("postvec/{model}"), "input": "hello" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(openai["object"], json!("list"), "{openai}");
    assert_eq!(openai["model"], json!(format!("postvec/{model}")));
    assert_eq!(openai["data"][0]["object"], json!("embedding"));
    assert!(!openai["data"][0]["embedding"]
        .as_array()
        .unwrap()
        .is_empty());
    assert!(openai["usage"]["prompt_tokens"].as_u64().unwrap() > 0);

    // Matryoshka truncation and base64 ride through to the executor.
    let short: Value = client
        .post(format!("{base}/api/openai/embeddings"))
        .json(&json!({ "model": model, "input": ["a", "b"], "dimensions": 8, "encoding_format": "base64" }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(short["data"].as_array().unwrap().len(), 2, "{short}");
    assert_eq!(short["data"][1]["index"], json!(1));
    // 8 little-endian f32 = 32 bytes = 44 base64 characters.
    assert_eq!(short["data"][0]["embedding"].as_str().unwrap().len(), 44);

    let over = client
        .post(format!("{base}/api/openai/embeddings"))
        .json(&json!({ "model": model, "input": "a", "dimensions": 1_000_000 }))
        .send()
        .await
        .unwrap();
    assert_eq!(over.status(), 400);
    let over: Value = over.json().await.unwrap();
    assert_eq!(
        over["error"]["type"],
        json!("invalid_request_error"),
        "{over}"
    );
}
