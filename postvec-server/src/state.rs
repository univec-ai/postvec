//! Everything the listeners share: the engine, the resolved settings, this
//! node's advertised identity, the metrics registry and the drain flag.

use crate::cluster::ClusterManager;
use crate::config::Settings;
use crate::metrics::Metrics;
use crate::net::Advertise;
use engine::InferenceEngine;
use std::net::IpAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::SystemTime;

/// How this node describes itself to peers, to `/config` readers and in its
/// own logs.
#[derive(Debug, Clone)]
pub struct NodeIdentity {
    pub advertise: IpAddr,
    /// `{scheme}://{advertise}:{http_port}` — what a peer would dial.
    pub api_address: String,
    /// `{advertise}:{grpc_port}` — what postvec would put in
    /// `postvec.ninference_grpc_endpoints`.
    pub grpc_address: String,
    /// The reachable HTTP URL, which is `api_address` unless the operator
    /// pinned something else.
    pub frontend: String,
}

impl NodeIdentity {
    pub fn new(settings: &Settings, advertise: Advertise) -> Self {
        let scheme = if settings.tls.is_some() {
            "https"
        } else {
            "http"
        };
        let api_address = format!(
            "{scheme}://{}",
            crate::net::host_port(advertise.ip, settings.http_port)
        );
        Self {
            frontend: crate::net::frontend_url(settings, advertise.ip),
            api_address,
            grpc_address: crate::net::host_port(advertise.ip, settings.grpc_port),
            advertise: advertise.ip,
        }
    }
}

pub struct ServerState {
    pub engine: Arc<InferenceEngine>,
    pub settings: Arc<Settings>,
    pub identity: NodeIdentity,
    pub metrics: Arc<Metrics>,
    /// `None` in the degenerate case where gossip could not start. The node
    /// still serves; it just cannot describe its peers.
    pub cluster: Option<Arc<ClusterManager>>,
    pub started_at: SystemTime,
    /// Serializes model load/unload. Held across the whole
    /// admission-check-and-mutate sequence so the resident-count check is an
    /// admission decision rather than an observation two concurrent admin
    /// requests can both act on.
    pub lifecycle: Arc<tokio::sync::Mutex<()>>,
    draining: AtomicBool,
}

impl ServerState {
    pub fn new(
        engine: Arc<InferenceEngine>,
        settings: Arc<Settings>,
        identity: NodeIdentity,
        metrics: Arc<Metrics>,
        cluster: Option<Arc<ClusterManager>>,
    ) -> Arc<Self> {
        Arc::new(Self {
            engine,
            settings,
            identity,
            metrics,
            cluster,
            started_at: SystemTime::now(),
            lifecycle: Arc::new(tokio::sync::Mutex::new(())),
            draining: AtomicBool::new(false),
        })
    }

    /// Enter the drain: `/ready` goes 503 so healthchecks and load balancers
    /// stop sending new work, while `/health` stays 200 and in-flight
    /// requests finish.
    pub fn begin_drain(&self) {
        self.draining.store(true, Ordering::SeqCst);
    }

    pub fn draining(&self) -> bool {
        self.draining.load(Ordering::SeqCst)
    }

    /// Models resident *and* able to answer — pool and executor both built.
    pub fn ready_models(&self) -> Vec<String> {
        self.engine
            .get_active_models()
            .into_iter()
            .filter(|name| self.engine.is_model_ready(name))
            .collect()
    }

    /// Readiness in the load-balancer sense: this node can serve an
    /// inference request right now.
    pub fn ready(&self) -> bool {
        !self.draining() && !self.ready_models().is_empty()
    }

    pub fn uptime_seconds(&self) -> u64 {
        self.started_at
            .elapsed()
            .map(|d| d.as_secs())
            .unwrap_or_default()
    }

    pub fn start_unix_seconds(&self) -> u64 {
        self.started_at
            .duration_since(SystemTime::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or_default()
    }
}
