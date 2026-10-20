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

/// How this node describes itself to peers, `/config` readers and its own logs.
#[derive(Debug, Clone)]
pub struct NodeIdentity {
    pub advertise: IpAddr,
    /// `{scheme}://{advertise}:{http_port}`, what a peer would dial.
    pub api_address: String,
    /// `{advertise}:{grpc_port}`, what postvec puts in `postvec.grpc_endpoints`.
    pub grpc_address: String,
    /// Reachable HTTP URL. `api_address` unless the operator pinned something else.
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
    pub managed: crate::managed::ManagedRuntime,
    pub engine: Arc<InferenceEngine>,
    pub settings: Arc<Settings>,
    pub identity: NodeIdentity,
    pub metrics: Arc<Metrics>,
    /// `None` when gossip could not start. The node still serves. It cannot describe its peers.
    pub cluster: Option<Arc<ClusterManager>>,
    /// External-provider gateway. Empty when unconfigured. Shared by `/config`, gRPC and admin reload.
    pub gateway: Arc<Gateway>,
    pub started_at: SystemTime,
    /// Serializes load and unload across admission and mutate, so the resident count is the decision.
    pub lifecycle: Arc<tokio::sync::Mutex<()>>,
    /// Registry pulls, oldest first. `pull_serial` runs one at a time.
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
        let managed = crate::managed::ManagedRuntime::new(&settings.managed);
        Arc::new(Self {
            managed,
            engine,
            settings,
            identity,
            metrics,
            cluster,
            gateway,
            started_at: SystemTime::now(),
            lifecycle: Arc::new(tokio::sync::Mutex::new(())),
            pulls: std::sync::Mutex::new(Vec::new()),
            pull_serial: tokio::sync::Mutex::new(()),
            draining: AtomicBool::new(false),
        })
    }

    /// `/ready` goes 503. `/health` stays 200. In-flight requests finish.
    pub fn begin_drain(&self) {
        self.draining.store(true, Ordering::SeqCst);
    }

    pub fn draining(&self) -> bool {
        self.draining.load(Ordering::SeqCst)
    }

    /// Names this process can answer now, engine and gateway.
    /// A provider-only node has no engine models and must still be ready.
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
