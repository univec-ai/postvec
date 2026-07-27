// SPDX-License-Identifier: BUSL-1.1
// Copyright (c) 2026 Univec Ltd. See postvec-server/LICENSE.

//! Registry routes end to end: pull from a loopback registry fixture, then
//! activate (which loads) and deactivate (which unloads).
//!
//! Needs postvec-cli's test-only index-URL override, so it is gated:
//! `cargo test -p postvec-server --features registry-test --test registry`.
#![cfg(feature = "registry-test")]

use postvec_registry::archive::{write_deterministic, ArchiveInput};
use postvec_server::cli::ServeArgs;
use postvec_server::config::{self, FileConfig};
use postvec_server::metrics::Metrics;
use postvec_server::net;
use postvec_server::state::{NodeIdentity, ServerState};
use serde_json::{json, Value};
use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

const MODEL: &str = "fixture-model";

fn descriptor(enabled: bool) -> Value {
    json!({
        "name": MODEL, "backend": "generic", "enabled": enabled,
        "file_path": "weights.bin",
        "executor": { "key": "dummy", "inputs": [{"json_key": "texts"}], "outputs": [{"json_key": "embeddings"}] },
        "params": { "model_type": "embed", "target_model": MODEL, "target_dim": 8 }
    })
}

/// One model, one archive, one index, served by a thread that speaks just
/// enough HTTP for the registry client (HEAD and GET, exact Content-Length).
fn serve_registry(work: &Path) -> String {
    let src = work.join("src");
    std::fs::create_dir_all(&src).unwrap();
    std::fs::write(src.join("d.json"), descriptor(true).to_string()).unwrap();
    std::fs::write(src.join("weights.bin"), vec![7u8; 100_000]).unwrap();
    let inputs = [
        ("ninference.hub.json", "d.json"),
        ("weights.bin", "weights.bin"),
    ]
    .map(|(relative, file)| ArchiveInput {
        relative_path: relative.into(),
        source: src.join(file),
    });
    let mut archive = Vec::new();
    let written = write_deterministic(MODEL, &inputs, &mut archive).unwrap();
    let digest = {
        use sha2::{Digest, Sha256};
        hex(&Sha256::digest(&archive))
    };
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap().to_string();
    let index = json!({
        "schema_version": 1, "channel": "public",
        "models": [{
            "name": MODEL, "access": "public", "model_type": "embed", "backend": "generic",
            "target_dim": 8, "target_model": MODEL, "license": "mit", "revision": 1, "dependencies": [], "postvec_requires": [],
            "archive": {
                "digest": format!("sha256:{digest}"), "size": archive.len(),
                "installed_size": written.installed_size,
                "sources": [format!("http://{address}/archives/{digest}")]
            }
        }]
    })
    .to_string()
    .into_bytes();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let mut stream = stream.unwrap();
            let mut buf = vec![0u8; 4096];
            let n = stream.read(&mut buf).unwrap_or(0);
            let request = String::from_utf8_lossy(&buf[..n]).to_string();
            let mut words = request.split_whitespace();
            let (method, path) = (words.next().unwrap_or(""), words.next().unwrap_or(""));
            let (status, body): (&str, &[u8]) = match path {
                "/v1/index.json" => ("200 OK", &index),
                p if p == format!("/archives/{digest}") => ("200 OK", &archive),
                _ => ("404 Not Found", b""),
            };
            let _ = write!(
                stream,
                "HTTP/1.1 {status}\r\nContent-Length: {}\r\nContent-Type: application/octet-stream\r\nConnection: close\r\n\r\n",
                body.len()
            );
            if method != "HEAD" {
                let _ = stream.write_all(body);
            }
        }
    });
    format!("http://{address}/v1/index.json")
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

struct Node {
    admin: SocketAddr,
    public: SocketAddr,
    state: Arc<ServerState>,
    client: reqwest::Client,
}

impl Node {
    async fn start(root: &Path, manage: bool) -> Node {
        let flags = ServeArgs {
            insecure: true,
            bind: Some("127.0.0.1".into()),
            manage,
            ..Default::default()
        };
        let settings = Arc::new(
            config::resolve(
                &flags,
                &FileConfig::default(),
                &std::collections::BTreeMap::new(),
                root.into(),
                None,
            )
            .unwrap(),
        );
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
            Arc::new(providers::gateway::Gateway::empty()),
        );
        let admin =
            postvec_server::admin::spawn(state.clone(), TcpListener::bind("127.0.0.1:0").unwrap())
                .unwrap();
        let public =
            postvec_server::http::spawn(state.clone(), TcpListener::bind("127.0.0.1:0").unwrap())
                .unwrap();
        let node = Node {
            admin: admin.bound,
            public: public.bound,
            state,
            client: reqwest::Client::new(),
        };
        while node
            .client
            .get(format!("http://{}/health", node.admin))
            .send()
            .await
            .is_err()
        {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        node
    }

    async fn get(&self, addr: SocketAddr, path: &str) -> Value {
        self.client
            .get(format!("http://{addr}{path}"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap()
    }

    async fn post(&self, addr: SocketAddr, path: &str, body: Value) -> (u16, Value) {
        let r = self
            .client
            .post(format!("http://{addr}{path}"))
            .json(&body)
            .send()
            .await
            .unwrap();
        (r.status().as_u16(), r.json().await.unwrap())
    }
}

#[tokio::test]
async fn pull_activate_deactivate_through_the_admin_port() {
    let work = tempfile::tempdir().unwrap();
    std::env::set_var(
        "POSTVEC_REGISTRY_PUBLIC_INDEX_URL",
        serve_registry(work.path()),
    );
    // The root lock refuses group-writable trees; a tempdir is 0700.
    let root = work.path().join("root");
    std::fs::create_dir_all(root.join("models")).unwrap();
    for dir in [work.path(), root.as_path(), &root.join("models")] {
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
    let root = root.canonicalize().unwrap();
    let node = Node::start(&root, true).await;

    // The safe default keeps the public port to the reads.
    let locked = Node::start(&root, false).await;
    let (status, _) = locked
        .post(
            locked.public,
            "/api/registry/pull",
            json!({"models": [MODEL]}),
        )
        .await;
    assert_eq!(status, 404);
    drop(locked);

    // --manage lets the dashboard use the public port deliberately.
    let (status, body) = node
        .post(
            node.public,
            "/api/registry/pull",
            json!({"models": [MODEL]}),
        )
        .await;
    assert_eq!(status, 200, "{body}");
    assert_eq!(body["data"]["job"], json!(1));
    let deadline = Instant::now() + Duration::from_secs(30);
    let job = loop {
        let jobs = node.get(node.admin, "/api/registry/pulls").await;
        let job = jobs["data"]["pulls"][0].clone();
        if job["status"] != "running" && job["status"] != "queued" {
            break job;
        }
        assert!(Instant::now() < deadline, "pull never finished: {jobs}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    };
    assert_eq!(job["status"], "done", "{job}");
    assert_eq!(job["results"][0]["status"], "installed", "{job}");
    assert_eq!(job["downloaded_bytes"], job["total_bytes"]);

    // Installed deactivated, seen by both views; a second pull is a no-op.
    let installed = node.get(node.public, "/api/registry/models").await;
    let row = &installed["data"]["models"][0];
    assert_eq!(row["name"], json!(MODEL));
    assert_eq!(row["enabled"], json!(false));
    assert_eq!(row["revision"], json!(1));
    assert_eq!(row["loaded"], json!(false));
    let available = node.get(node.public, "/api/registry/available").await;
    assert_eq!(
        available["data"]["models"][0]["update"],
        json!("current"),
        "{available}"
    );
    assert!(available["data"]["models"][0]
        .get("license_version")
        .is_some());
    assert!(available["data"]["models"][0].get("license_url").is_some());
    assert_eq!(available["data"]["models"][0]["dependencies"], json!([]));
    let (_, body) = node
        .post(node.admin, "/api/registry/pull", json!({"models": [MODEL]}))
        .await;
    assert_eq!(body["data"]["job"], json!(2));
    tokio::time::sleep(Duration::from_millis(300)).await;
    let jobs = node.get(node.admin, "/api/registry/pulls").await;
    assert_eq!(
        jobs["data"]["pulls"][1]["results"][0]["status"],
        json!("already-installed"),
        "{jobs}"
    );

    // Activate enables on disk and loads; deactivate unloads and disables.
    let (_, body) = node
        .post(
            node.admin,
            "/api/registry/activate",
            json!({"models": [MODEL]}),
        )
        .await;
    assert_eq!(
        body["data"]["results"][0]["status"],
        json!("loaded"),
        "{body}"
    );
    assert!(node.state.engine.is_model_ready(MODEL));
    let installed = node.get(node.admin, "/api/registry/models").await;
    assert_eq!(installed["data"]["models"][0]["enabled"], json!(true));
    let (_, body) = node
        .post(
            node.admin,
            "/api/registry/deactivate",
            json!({"models": [MODEL]}),
        )
        .await;
    assert_eq!(
        body["data"]["results"][0]["status"],
        json!("deactivated"),
        "{body}"
    );
    assert!(!node.state.engine.is_model_ready(MODEL));
    let installed = node.get(node.admin, "/api/registry/models").await;
    assert_eq!(installed["data"]["models"][0]["enabled"], json!(false));
    assert_eq!(installed["data"]["models"][0]["removable"], json!(true));

    // Remove unloads (it was reactivated above) and deletes the directory;
    // the catalogue then offers it again.
    let (_, body) = node
        .post(
            node.admin,
            "/api/registry/activate",
            json!({"models": [MODEL]}),
        )
        .await;
    assert_eq!(
        body["data"]["results"][0]["status"],
        json!("loaded"),
        "{body}"
    );
    let (_, body) = node
        .post(
            node.admin,
            "/api/registry/remove",
            json!({"models": [MODEL]}),
        )
        .await;
    assert_eq!(
        body["data"]["results"][0]["status"],
        json!("removed"),
        "{body}"
    );
    assert!(!node.state.engine.is_model_ready(MODEL));
    assert!(!root.join("models/generic").join(MODEL).exists());
    let available = node.get(node.public, "/api/registry/available").await;
    assert_eq!(
        available["data"]["models"][0]["update"],
        json!("not-installed")
    );

    // A directory without a registry receipt remains operator-owned.
    let manual = root.join("models/generic").join(MODEL);
    std::fs::create_dir_all(&manual).unwrap();
    std::fs::write(
        manual.join("ninference.hub.json"),
        descriptor(false).to_string(),
    )
    .unwrap();
    std::fs::write(manual.join("weights.bin"), b"manual").unwrap();
    let installed = node.get(node.admin, "/api/registry/models").await;
    assert_eq!(installed["data"]["models"][0]["removable"], json!(false));
    let (_, body) = node
        .post(
            node.admin,
            "/api/registry/remove",
            json!({"models": [MODEL]}),
        )
        .await;
    assert_eq!(body["data"]["results"][0]["status"], json!("error"));
    assert!(manual.exists());

    // Names the registry does not know, and bad requests, are refusals.
    let (status, body) = node
        .post(
            node.admin,
            "/api/registry/activate",
            json!({"models": ["ghost"]}),
        )
        .await;
    assert_eq!(status, 200);
    assert_eq!(
        body["data"]["results"][0]["status"],
        json!("error"),
        "{body}"
    );
    let (status, _) = node
        .post(node.admin, "/api/registry/pull", json!({"models": []}))
        .await;
    assert_eq!(status, 400);
}
