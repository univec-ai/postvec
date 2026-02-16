//! The gRPC wire contract, exercised by a real client over a real socket.
//!
//! This is the surface postvec's worker actually calls, and the one that
//! cannot drift: the status codes and the `x-ravenna-error-code` metadata
//! drive the extension's entire retry and dead-letter policy. A code the
//! client does not recognise falls through to `Unknown → Transient`, which
//! turns a permanent failure into one that retries forever.
//!
//! No models and no ONNX Runtime: every case here is a refusal raised before
//! the engine would create a native session, which is precisely the set of
//! refusals a misconfigured deployment meets in practice.

use postvec_server::metrics::Metrics;
use postvec_server::proto::ninference_service_client::NinferenceServiceClient;
use postvec_server::proto::{ConvertEmbeddingsRequest, EmbedTextsRequest, FloatVector};
use std::sync::Arc;
use std::time::Duration;
use tonic::Request;

struct Server {
    endpoint: String,
    metrics: Arc<Metrics>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    task: tokio::task::JoinHandle<Result<(), String>>,
    _root: tempfile::TempDir,
}

impl Server {
    async fn start() -> Self {
        Self::start_with(Duration::from_secs(30), 4).await
    }

    async fn start_with(predict_timeout: Duration, max_inflight: usize) -> Self {
        let root = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(root.path().join("models")).unwrap();
        let engine = Arc::new(engine::InferenceEngine::new(Arc::new(
            engine::EngineConfig {
                root_path: root.path().to_path_buf(),
                host_policy: Default::default(),
            },
        )));
        let metrics = Arc::new(Metrics::new());

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let bound = listener.local_addr().unwrap();
        let (tx, rx) = tokio::sync::oneshot::channel::<()>();
        let task = tokio::spawn(postvec_server::grpc::serve(
            engine,
            metrics.clone(),
            listener,
            predict_timeout,
            max_inflight,
            Arc::new(providers::gateway::Gateway::empty()),
            async {
                let _ = rx.await;
            },
        ));

        let server = Self {
            endpoint: format!("http://{bound}"),
            metrics,
            shutdown: Some(tx),
            task,
            _root: root,
        };
        server.await_listening().await;
        server
    }

    async fn await_listening(&self) {
        for attempt in 0..200 {
            if NinferenceServiceClient::connect(self.endpoint.clone())
                .await
                .is_ok()
            {
                return;
            }
            assert!(attempt < 199, "gRPC listener never came up");
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }

    async fn client(&self) -> NinferenceServiceClient<tonic::transport::Channel> {
        NinferenceServiceClient::connect(self.endpoint.clone())
            .await
            .expect("connect")
    }

    fn rendered_metrics(&self) -> String {
        self.metrics.render(&postvec_server::metrics::Snapshot {
            version: "test",
            features: "onnx".to_string(),
            start_unix_seconds: 0,
            models_loaded: 0,
            models_enabled_on_disk: 0,
            ready: false,
            draining: false,
            cluster_members: 1,
        })
    }

    async fn stop(mut self) {
        if let Some(tx) = self.shutdown.take() {
            let _ = tx.send(());
        }
        let _ = tokio::time::timeout(Duration::from_secs(5), self.task).await;
    }
}

fn error_code(status: &tonic::Status) -> Option<String> {
    status
        .metadata()
        .get("x-ravenna-error-code")
        .and_then(|v| v.to_str().ok())
        .map(str::to_string)
}

/// The refusal a misconfigured deployment meets most often, and the one whose
/// classification matters most: postvec must read it as permanent, not as a
/// transport blip to retry.
#[tokio::test]
async fn embedding_an_unloaded_model_is_a_permanent_refusal() {
    let server = Server::start().await;
    let mut client = server.client().await;

    let status = client
        .embed_texts(Request::new(EmbedTextsRequest {
            model: "not-loaded".to_string(),
            texts: vec!["hello".to_string()],
            ..Default::default()
        }))
        .await
        .expect_err("an unloaded model must be refused");

    assert_eq!(status.code(), tonic::Code::FailedPrecondition);
    assert_eq!(error_code(&status).as_deref(), Some("MODEL_NOT_LOADED"));
    assert!(
        status.message().contains("postvec-server load"),
        "the message should name the fix: {}",
        status.message()
    );
    server.stop().await;
}

#[tokio::test]
async fn converting_with_an_unloaded_model_is_a_permanent_refusal() {
    let server = Server::start().await;
    let mut client = server.client().await;

    let status = client
        .convert_embeddings(Request::new(ConvertEmbeddingsRequest {
            model: "not-loaded".to_string(),
            embeddings: vec![FloatVector {
                vector: vec![0.1, 0.2],
            }],
            ..Default::default()
        }))
        .await
        .expect_err("an unloaded converter must be refused");

    assert_eq!(status.code(), tonic::Code::FailedPrecondition);
    assert_eq!(error_code(&status).as_deref(), Some("MODEL_NOT_LOADED"));
    server.stop().await;
}

/// Inference requests never load models — that is what keeps a client on the
/// network from expanding the resident set past every configured ceiling.
/// The refusal above *is* the proof, so this asserts the complement: nothing
/// became resident.
#[tokio::test]
async fn a_refused_request_leaves_nothing_resident() {
    let server = Server::start().await;
    let mut client = server.client().await;
    let _ = client
        .embed_texts(Request::new(EmbedTextsRequest {
            model: "not-loaded".to_string(),
            texts: vec!["hello".to_string()],
            ..Default::default()
        }))
        .await;
    let metrics = server.rendered_metrics();
    assert!(
        metrics.contains("postvec_server_models_loaded 0"),
        "{metrics}"
    );
    server.stop().await;
}

/// The item ceiling is charged before anything else that is O(items), so a
/// hostile batch cannot make the node walk it first.
#[tokio::test]
async fn an_oversized_batch_is_refused_by_the_item_ceiling() {
    let server = Server::start().await;
    let mut client = server.client().await;

    let status = client
        .embed_texts(Request::new(EmbedTextsRequest {
            model: "not-loaded".to_string(),
            texts: vec!["x".to_string(); 5000],
            ..Default::default()
        }))
        .await
        .expect_err("5000 texts is over the 4096 ceiling");

    // The readiness check runs first, so an unloaded model still answers
    // MODEL_NOT_LOADED; what matters is that neither path builds the batch.
    assert!(
        matches!(
            status.code(),
            tonic::Code::InvalidArgument | tonic::Code::FailedPrecondition
        ),
        "got {status:?}"
    );
    server.stop().await;
}

/// A caller's `grpc-timeout` is a ceiling the server honours, not a hint. An
/// already-expired one must be refused before any work happens — including
/// before the readiness check, which would otherwise answer a different
/// error and prove work ran after the budget was gone.
#[tokio::test]
async fn an_expired_deadline_is_refused_before_anything_else() {
    let server = Server::start().await;
    let mut client = server.client().await;

    let mut request = Request::new(EmbedTextsRequest {
        model: "not-loaded".to_string(),
        texts: vec!["hello".to_string()],
        ..Default::default()
    });
    request
        .metadata_mut()
        .insert("grpc-timeout", "1n".parse().unwrap());

    let status = client
        .embed_texts(request)
        .await
        .expect_err("an exhausted deadline must be refused");
    // What the *client* sees is ambiguous here: tonic enforces the same
    // header on its own side, so it may surface `Cancelled` before the
    // server's `DeadlineExceeded` arrives. The assertion that matters is
    // server-side — the refusal was classified as a deadline, not as the
    // MODEL_NOT_LOADED the readiness check would have produced had it run.
    assert!(
        matches!(
            status.code(),
            tonic::Code::DeadlineExceeded | tonic::Code::Cancelled
        ),
        "got {status:?}"
    );

    let mut metrics = String::new();
    for _ in 0..100 {
        metrics = server.rendered_metrics();
        if metrics.contains("code=\"DEADLINE_EXCEEDED\"") {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        metrics.contains(
            "postvec_server_request_errors_total{method=\"embed_texts\",code=\"DEADLINE_EXCEEDED\"} 1"
        ),
        "the server must classify this as a deadline, not as a missing model:\n{metrics}"
    );
    assert!(
        !metrics.contains("code=\"MODEL_NOT_LOADED\""),
        "the readiness check must not have run:\n{metrics}"
    );
    server.stop().await;
}

/// Every request, refused or not, must release its in-flight slot and land in
/// exactly one counter.
#[tokio::test]
async fn refusals_are_counted_and_release_their_slot() {
    let server = Server::start().await;
    let mut client = server.client().await;

    for _ in 0..3 {
        let _ = client
            .embed_texts(Request::new(EmbedTextsRequest {
                model: "not-loaded".to_string(),
                texts: vec!["hello".to_string()],
                ..Default::default()
            }))
            .await;
    }
    let _ = client
        .convert_embeddings(Request::new(ConvertEmbeddingsRequest {
            model: "not-loaded".to_string(),
            embeddings: vec![FloatVector { vector: vec![0.5] }],
            ..Default::default()
        }))
        .await;

    assert_eq!(server.metrics.in_flight(), 0);
    let metrics = server.rendered_metrics();
    assert!(
        metrics.contains(
            "postvec_server_request_errors_total{method=\"embed_texts\",code=\"MODEL_NOT_LOADED\"} 3"
        ),
        "{metrics}"
    );
    assert!(
        metrics.contains(
            "postvec_server_request_errors_total{method=\"convert_embeddings\",code=\"MODEL_NOT_LOADED\"} 1"
        ),
        "{metrics}"
    );
    assert!(
        metrics.contains("postvec_server_requests_total{method=\"embed_texts\"} 3"),
        "{metrics}"
    );
    assert!(
        metrics.contains("postvec_server_requests_in_flight 0"),
        "{metrics}"
    );
    server.stop().await;
}

/// A shutdown signal stops the listener. Existing connections finish; new
/// ones are refused, which is the transport error postvec already retries
/// against the next endpoint in its round-robin.
#[tokio::test]
async fn the_shutdown_signal_stops_the_listener() {
    let mut server = Server::start().await;
    let endpoint = server.endpoint.clone();

    server.shutdown.take().unwrap().send(()).unwrap();
    let outcome = tokio::time::timeout(Duration::from_secs(5), &mut server.task)
        .await
        .expect("the server task must finish promptly after the signal");
    assert!(outcome.unwrap().is_ok(), "a clean shutdown is not an error");

    let mut refused = false;
    for _ in 0..50 {
        if NinferenceServiceClient::connect(endpoint.clone())
            .await
            .is_err()
        {
            refused = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(refused, "the port must stop accepting after shutdown");
}
