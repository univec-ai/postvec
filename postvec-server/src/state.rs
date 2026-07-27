// SPDX-License-Identifier: BUSL-1.1
// Copyright (c) 2026 Univec Ltd. See postvec-server/LICENSE.

//! Shared process state for the HTTP, gRPC and admin listeners.

use crate::cluster::ClusterManager;
use crate::config::Settings;
use crate::metrics::Metrics;
use crate::net::Advertise;
use crate::registry::PullJob;
use engine::InferenceEngine;
use providers::gateway::Gateway;
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
    /// `postvec.grpc_endpoints`.
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
    /// External-provider gateway. Empty in the zero-config case; shared by
    /// `/config`, the gRPC dispatch and the admin reload route.
    pub gateway: Arc<Gateway>,
    /// Provider inflight budget the gRPC ingress limit was sized with at
    /// boot. The reload route warns when a reload outgrows it.
    pub startup_provider_budget: usize,
    pub started_at: SystemTime,
    /// Serializes model load/unload. Held across the whole
    /// admission-check-and-mutate sequence so the resident-count check is an
    /// admission decision rather than an observation two concurrent admin
    /// requests can both act on.
    pub lifecycle: Arc<tokio::sync::Mutex<()>>,
    /// Registry pulls started through the API, oldest first. `pull_serial`
    /// runs them one at a time: a second request queues rather than failing
    /// on the root lock the first one holds.
    pub pulls: std::sync::Mutex<Vec<PullJob>>,
    pub pull_serial: tokio::sync::Mutex<()>,
    draining: AtomicBool,
}

impl ServerState {
    pub fn new(
        engine: Arc<InferenceEngine>,
        settings: Arc<Settings>,
        identity: NodeIdentity,
        metrics: Arc<Metrics>,
        cluster: Option<Arc<ClusterManager>>,
        gateway: Arc<Gateway>,
    ) -> Arc<Self> {
        let startup_provider_budget = gateway.inflight_budget();
        Arc::new(Self {
            engine,
            settings,
            identity,
            metrics,
            cluster,
            gateway,
            startup_provider_budget,
            started_at: SystemTime::now(),
            lifecycle: Arc::new(tokio::sync::Mutex::new(())),
            pulls: std::sync::Mutex::new(Vec::new()),
            pull_serial: tokio::sync::Mutex::new(()),
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

    /// Names this process can answer right now, engine and gateway.
    /// A provider-only node has no engine models; readiness still has to
    /// be true.
    pub fn ready_models(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .engine
            .get_active_models()
            .into_iter()
            .filter(|name| self.engine.is_model_ready(name))
            .collect();
        names.extend(
            self.gateway
                .models()
                .iter()
                .filter_map(|m| m["name"].as_str())
                .map(str::to_string),
        );
        names.sort();
        names.dedup();
        names
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
