//! HTTP surfaces over real sockets, not handler-level calls.
//! Protects which routes exist on which listener and the discovery envelope.
//! No ONNX: admission and rendering only.

use postvec_server::cli::ServeArgs;
use postvec_server::config::{self, FileConfig, Settings};
use postvec_server::metrics::Metrics;
use postvec_server::net;
use postvec_server::state::{NodeIdentity, ServerState};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

// ---- Harness -----------------------------------------------------------

struct Node {
    admin: SocketAddr,
    public: SocketAddr,
    state: Arc<ServerState>,
    client: reqwest::Client,
    // Held so the listeners stay alive for the life of the test.
    _root: tempfile::TempDir,
}

fn settings_for(root: &Path, flags: ServeArgs) -> Settings {
    config::resolve(
        &flags,
        &FileConfig::default(),
        &BTreeMap::new(),
        root.to_path_buf(),
        None,
    )
    .expect("settings resolve")
}

fn write_descriptor(root: &Path, backend: &str, name: &str, body: Value) {
    let dir = root.join("models").join(backend).join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(dir.join("ninference.hub.json"), body.to_string()).unwrap();
}

async fn start(flags: ServeArgs) -> Node {
    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("models")).unwrap();
    start_in(root, flags).await
}

async fn start_in(root: tempfile::TempDir, flags: ServeArgs) -> Node {
    start_with_gateway(root, flags, Arc::new(providers::gateway::Gateway::empty())).await
}

async fn start_with_gateway(
    root: tempfile::TempDir,
    mut flags: ServeArgs,
    gateway: Arc<providers::gateway::Gateway>,
) -> Node {
    // Plain HTTP and ephemeral ports: these tests are about routing and
    // status codes, not about TLS or about the well-known port numbers.
    flags.insecure = true;
    flags.bind = Some("127.0.0.1".to_string());
    let settings = Arc::new(settings_for(root.path(), flags));

    let advertise = net::resolve_advertise(&settings).unwrap();
    let identity = NodeIdentity::new(&settings, advertise);
    let engine = Arc::new(engine::InferenceEngine::new(Arc::new(
        engine::EngineConfig {
            root_path: settings.root.clone(),
            host_policy: Default::default(),
        },
    )));
    let state = ServerState::new(
        engine,
        settings,
        identity,
        Arc::new(Metrics::new()),
        None,
        gateway,
    );

    let admin_socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let public_socket = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let admin = postvec_server::admin::spawn(state.clone(), admin_socket).unwrap();
    let public = postvec_server::http::spawn(state.clone(), public_socket).unwrap();

    let node = Node {
        admin: admin.bound,
        public: public.bound,
        state,
        client: reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(10))
            .build()
            .unwrap(),
        _root: root,
    };
    // axum-server binds lazily inside `serve`; wait until both answer so a
    // fast test does not race the accept loop.
    node.await_listening().await;
    node
}

impl Node {
    async fn await_listening(&self) {
        for addr in [self.admin, self.public] {
            for attempt in 0..200 {
                if self
                    .client
                    .get(format!("http://{addr}/health"))
                    .send()
                    .await
                    .is_ok()
                {
                    break;
                }
                assert!(attempt < 199, "listener on {addr} never came up");
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        }
    }

    async fn get(&self, addr: SocketAddr, path: &str) -> (reqwest::StatusCode, String) {
        let response = self
            .client
            .get(format!("http://{addr}{path}"))
            .send()
            .await
            .unwrap();
        (response.status(), response.text().await.unwrap())
    }

    async fn get_json(&self, addr: SocketAddr, path: &str) -> (reqwest::StatusCode, Value) {
        let (status, body) = self.get(addr, path).await;
        (
            status,
            serde_json::from_str(&body).unwrap_or_else(|e| panic!("{path}: {e}\n{body}")),
        )
    }

    async fn post_json(
        &self,
        addr: SocketAddr,
        path: &str,
        body: Value,
    ) -> (reqwest::StatusCode, Value) {
        let response = self
            .client
            .post(format!("http://{addr}{path}"))
            .json(&body)
            .send()
            .await
            .unwrap();
        let status = response.status();
        let text = response.text().await.unwrap();
        (
            status,
            serde_json::from_str(&text).unwrap_or_else(|e| panic!("{path}: {e}\n{text}")),
        )
    }
}

// ---- The discovery contract -------------------------------------------
//
// A transcription of the structs `postvec/src/client/discovery.rs` actually
// deserializes. postvec is a pgrx extension and workspace-excluded, so it
// cannot be linked here; this is the next best gate on the one part of the
// envelope that is a compatibility promise.

#[derive(Debug, Deserialize)]
struct Envelope {
    #[serde(default)]
    success: bool,
    data: Option<ConfigData>,
}

#[derive(Debug, Deserialize)]
struct ConfigData {
    #[serde(default)]
    models: Vec<HubModel>,
}

#[derive(Debug, Deserialize)]
struct HubModel {
    #[allow(dead_code)]
    name: String,
    #[allow(dead_code)]
    configuration: Option<Value>,
}

// ---- Read-only surfaces ------------------------------------------------

#[tokio::test]
async fn health_is_green_before_any_model_is_loaded() {
    let node = start(ServeArgs::default()).await;
    let (status, body) = node.get_json(node.public, "/health").await;
    assert_eq!(status, 200);
    assert_eq!(body["data"]["status"], json!("ok"));
    assert_eq!(body["data"]["draining"], json!(false));
}

/// Liveness and readiness answer different questions, and an empty node is
/// the case that proves it: the process is up, and nothing should route to
/// it yet.
#[tokio::test]
async fn readiness_is_503_until_a_model_can_answer() {
    let node = start(ServeArgs::default()).await;
    let (status, body) = node.get_json(node.public, "/ready").await;
    assert_eq!(status, 503);
    assert_eq!(body["data"]["ready"], json!(false));
    assert_eq!(
        body["data"]["reason"],
        json!("no model is loaded and ready, and no external provider is configured")
    );
    assert_eq!(body["data"]["models"], json!([]));
}

/// A node configured purely as a provider gateway holds no engine models.
/// Counting only those left it answering 503 forever while it served
/// `EmbedTexts` perfectly well — a load balancer would never route to a node
/// that works.
///
/// Driven through the real `/ready` route over a real socket, not a copy
/// of the predicate.
#[tokio::test]
async fn a_provider_only_node_is_ready() {
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir().unwrap();
    std::fs::create_dir_all(root.path().join("models")).unwrap();
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
    let providers_dir = root.path().join("providers.d");
    std::fs::create_dir_all(&providers_dir).unwrap();
    std::fs::set_permissions(&providers_dir, std::fs::Permissions::from_mode(0o700)).unwrap();
    let file = providers_dir.join("openai.toml");
    std::fs::write(
        &file,
        "provider = \"openai\"\napi_key = \"sk-test\"\n\n[[models]]\n\
         name = \"openai-text-embedding-3-small\"\n\
         provider_model_id = \"text-embedding-3-small\"\ndim = 1536\n",
    )
    .unwrap();
    std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o600)).unwrap();

    let gateway = Arc::new(providers::gateway::Gateway::load(
        &providers_dir,
        &Default::default(),
    ));
    assert!(!gateway.is_empty(), "the provider file must have loaded");

    let node = start_with_gateway(root, ServeArgs::default(), gateway).await;
    let (status, body) = node.get_json(node.public, "/ready").await;
    assert_eq!(status, 200, "a provider-only node must be routable: {body}");
    assert_eq!(body["data"]["ready"], json!(true));
    assert_eq!(
        body["data"]["models"],
        json!(["openai-text-embedding-3-small"])
    );

    // And a drain still takes it out, provider models or not.
    node.state.begin_drain();
    let (status, body) = node.get_json(node.public, "/ready").await;
    assert_eq!(status, 503);
    assert_eq!(body["data"]["reason"], json!("draining"));
}

#[tokio::test]
async fn config_is_parseable_by_postvecs_discovery_client() {
    let node = start(ServeArgs::default()).await;
    let (status, body) = node.get(node.public, "/config").await;
    assert_eq!(status, 200);

    let envelope: Envelope = serde_json::from_str(&body).expect("postvec must parse /config");
    assert!(envelope.success);
    assert!(
        envelope.data.expect("data").models.is_empty(),
        "an empty node advertises no models"
    );

    // The additions are present and additive.
    let raw: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(raw["data"]["server"]["name"], json!("postvec-server"));
    assert!(raw["data"]["server"]["root"].is_string());
    assert_eq!(raw["data"]["server"]["draining"], json!(false));
    assert_eq!(raw["data"]["cluster"]["group"], json!("postvec"));
    assert_eq!(raw["data"]["cluster"]["enabled"], json!(false));
    assert!(raw["data"]["system"]["memory_total_bytes"].is_number());
}

#[tokio::test]
async fn metrics_render_in_prometheus_text_format() {
    let node = start(ServeArgs::default()).await;
    let response = node
        .client
        .get(format!("http://{}/metrics", node.public))
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 200);
    assert!(response
        .headers()
        .get("content-type")
        .unwrap()
        .to_str()
        .unwrap()
        .starts_with("text/plain"));
    let body = response.text().await.unwrap();
    assert!(
        body.contains("postvec_server_build_info{version="),
        "{body}"
    );
    assert!(body.contains("postvec_server_ready 0"), "{body}");
    assert!(body.contains("postvec_server_models_loaded 0"), "{body}");
}

#[tokio::test]
async fn metrics_can_be_switched_off() {
    let node = start(ServeArgs {
        no_metrics: true,
        ..Default::default()
    })
    .await;
    let (status, _) = node.get(node.public, "/metrics").await;
    assert_eq!(status, 404);
    // The other read-only routes are unaffected.
    assert_eq!(node.get(node.public, "/health").await.0, 200);
}

// ---- The listener split ------------------------------------------------

/// The property the two-listener design exists for: the socket that is
/// published to the network carries no route that can mutate the engine.
#[tokio::test]
async fn the_public_listener_has_no_admin_routes() {
    let node = start(ServeArgs::default()).await;
    for path in ["/admin/load", "/admin/unload"] {
        let response = node
            .client
            .post(format!("http://{}{path}", node.public))
            .json(&json!({"models": ["anything"]}))
            .send()
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            404,
            "{path} must not exist on the public listener"
        );
    }
}

#[tokio::test]
async fn the_admin_listener_binds_loopback_only() {
    let node = start(ServeArgs::default()).await;
    assert!(
        node.admin.ip().is_loopback(),
        "admin bound {}, which is routable",
        node.admin
    );
}

/// Refusing a routable admin bind is a boot failure, not a warning.
#[tokio::test]
async fn a_routable_admin_bind_is_refused() {
    let node = start(ServeArgs::default()).await;
    let routable = std::net::TcpListener::bind("0.0.0.0:0").unwrap();
    let err = match postvec_server::admin::spawn(node.state.clone(), routable) {
        Ok(_) => panic!("a routable admin bind must be refused"),
        Err(e) => e,
    };
    assert!(err.contains("not loopback"), "{err}");
}

/// The node-local CLI reads `/config` over plain loopback HTTP rather than
/// negotiating TLS with a self-signed certificate against the public port,
/// which only works because the admin listener mirrors the read-only routes.
#[tokio::test]
async fn the_admin_listener_mirrors_the_read_only_routes() {
    let node = start(ServeArgs::default()).await;
    assert_eq!(node.get(node.admin, "/health").await.0, 200);
    assert_eq!(node.get(node.admin, "/ready").await.0, 503);
    let (status, body) = node.get(node.admin, "/config").await;
    assert_eq!(status, 200);
    serde_json::from_str::<Envelope>(&body).expect("the same envelope");
}

// ---- Admin admission ---------------------------------------------------

#[tokio::test]
async fn loading_a_model_that_is_not_on_disk_is_a_per_model_error() {
    let node = start(ServeArgs::default()).await;
    let (status, body) = node
        .post_json(node.admin, "/admin/load", json!({"models": ["absent"]}))
        .await;
    assert_eq!(status, 200, "per-model outcomes ride a 200");
    let result = &body["data"]["results"][0];
    assert_eq!(result["model"], json!("absent"));
    assert_eq!(result["status"], json!("error"));
    assert!(
        result["error"]
            .as_str()
            .unwrap()
            .contains("postvec model pull"),
        "{result}"
    );
}

/// The switch `postvec model activate` throws has to mean the same thing on
/// a node as it does in a database.
#[tokio::test]
async fn loading_a_deactivated_descriptor_is_refused() {
    let root = tempfile::tempdir().unwrap();
    write_descriptor(
        root.path(),
        "onnx-runtime",
        "off",
        json!({"name": "off", "enabled": false}),
    );
    let node = start_in(root, ServeArgs::default()).await;

    let (status, body) = node
        .post_json(node.admin, "/admin/load", json!({"models": ["off"]}))
        .await;
    assert_eq!(status, 200);
    let result = &body["data"]["results"][0];
    assert_eq!(result["status"], json!("error"));
    assert!(
        result["error"]
            .as_str()
            .unwrap()
            .contains("postvec model activate off"),
        "{result}"
    );
}

/// Two backends, one name: the engine would resolve by directory order, so
/// the admin route refuses to pick.
#[tokio::test]
async fn loading_an_ambiguous_name_is_refused() {
    let root = tempfile::tempdir().unwrap();
    for backend in ["onnx-runtime", "candle"] {
        write_descriptor(
            root.path(),
            backend,
            "dup",
            json!({"name": "dup", "enabled": true}),
        );
    }
    let node = start_in(root, ServeArgs::default()).await;

    let (_, body) = node
        .post_json(node.admin, "/admin/load", json!({"models": ["dup"]}))
        .await;
    let result = &body["data"]["results"][0];
    assert_eq!(result["status"], json!("error"));
    assert!(
        result["error"]
            .as_str()
            .unwrap()
            .contains("multiple backends"),
        "{result}"
    );
}

/// `--models` is the eligibility policy for every load path, not only for
/// the startup preload.
#[tokio::test]
async fn the_models_allow_list_gates_the_admin_route_too() {
    let root = tempfile::tempdir().unwrap();
    write_descriptor(
        root.path(),
        "onnx-runtime",
        "allowed",
        json!({"name": "allowed", "enabled": true}),
    );
    write_descriptor(
        root.path(),
        "onnx-runtime",
        "other",
        json!({"name": "other", "enabled": true}),
    );
    let node = start_in(
        root,
        ServeArgs {
            models: Some(vec!["allowed".to_string()]),
            ..Default::default()
        },
    )
    .await;

    let (_, body) = node
        .post_json(node.admin, "/admin/load", json!({"models": ["other"]}))
        .await;
    let result = &body["data"]["results"][0];
    assert_eq!(result["status"], json!("error"));
    assert!(
        result["error"].as_str().unwrap().contains("--models"),
        "{result}"
    );
}

#[tokio::test]
async fn unloading_something_that_is_not_loaded_is_not_an_error() {
    let node = start(ServeArgs::default()).await;
    let (status, body) = node
        .post_json(node.admin, "/admin/unload", json!({"models": ["absent"]}))
        .await;
    assert_eq!(status, 200);
    assert_eq!(body["data"]["results"][0]["status"], json!("not-loaded"));
}

#[tokio::test]
async fn admin_requests_are_validated_at_the_request_level() {
    let node = start(ServeArgs::default()).await;

    // A path-like name never reaches the filesystem.
    let (status, body) = node
        .post_json(
            node.admin,
            "/admin/load",
            json!({"models": ["../../etc/passwd"]}),
        )
        .await;
    assert_eq!(status, 400);
    assert_eq!(body["success"], json!(false));

    // Empty and oversized requests.
    assert_eq!(
        node.post_json(node.admin, "/admin/load", json!({"models": []}))
            .await
            .0,
        400
    );
    let many: Vec<String> = (0..33).map(|i| format!("m{i}")).collect();
    assert_eq!(
        node.post_json(node.admin, "/admin/load", json!({"models": many}))
            .await
            .0,
        400
    );

    // A body that is not the expected shape.
    let response = node
        .client
        .post(format!("http://{}/admin/load", node.admin))
        .body("not json")
        .send()
        .await
        .unwrap();
    assert_eq!(response.status(), 400);
}

/// 64 KiB is the cap, enforced by the extractor rather than by reading an
/// unbounded body first.
#[tokio::test]
async fn oversized_admin_bodies_are_rejected() {
    let node = start(ServeArgs::default()).await;
    let response = node
        .client
        .post(format!("http://{}/admin/load", node.admin))
        .body("x".repeat(128 * 1024))
        .send()
        .await
        .unwrap();
    assert!(
        response.status() == 413 || response.status() == 400,
        "got {}",
        response.status()
    );
}

// ---- Drain -------------------------------------------------------------

/// What a rolling restart looks like from outside: readiness drops so
/// nothing new is routed here, liveness holds so a supervisor does not kill
/// the process mid-request, and discovery keeps its model list so a
/// single-node deployment's SQL cache is not pruned during its own restart.
#[tokio::test]
async fn draining_changes_readiness_but_not_liveness_or_discovery() {
    let node = start(ServeArgs::default()).await;
    let (before_status, before) = node.get_json(node.public, "/config").await;
    assert_eq!(before_status, 200);

    node.state.begin_drain();

    let (status, body) = node.get_json(node.public, "/ready").await;
    assert_eq!(status, 503);
    assert_eq!(body["data"]["reason"], json!("draining"));

    let (status, body) = node.get_json(node.public, "/health").await;
    assert_eq!(status, 200, "liveness holds through the drain");
    assert_eq!(body["data"]["draining"], json!(true));

    let (status, after) = node.get_json(node.public, "/config").await;
    assert_eq!(status, 200);
    assert_eq!(
        after["data"]["models"], before["data"]["models"],
        "a draining node must keep advertising its models"
    );
    assert_eq!(after["data"]["server"]["draining"], json!(true));

    let (_, metrics) = node.get(node.public, "/metrics").await;
    assert!(metrics.contains("postvec_server_draining 1"), "{metrics}");
}

// ---- Counters ----------------------------------------------------------

#[tokio::test]
async fn admin_refusals_and_config_reads_are_counted() {
    let node = start(ServeArgs::default()).await;
    node.get(node.public, "/config").await;
    node.post_json(node.admin, "/admin/load", json!({"models": ["BAD NAME"]}))
        .await;

    let (_, metrics) = node.get(node.public, "/metrics").await;
    assert!(
        metrics.contains("postvec_server_admin_refusals_total 1"),
        "{metrics}"
    );
    // Two reads by the time this scrape happens: the one above plus any the
    // harness made. Assert presence and a non-zero value rather than a count
    // the harness could change.
    let line = metrics
        .lines()
        .find(|l| l.starts_with("postvec_server_config_requests_total "))
        .expect("config counter present");
    let value: u64 = line.rsplit(' ').next().unwrap().parse().unwrap();
    assert!(value >= 1, "{line}");
}
