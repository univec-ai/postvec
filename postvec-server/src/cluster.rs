//! Cluster membership over memberlist gossip (memberlist 0.8, TCP only).
//!
//! Default group is `postvec`. Peers are resolved in [`crate::net`] before
//! they get here. The gossip advertise address is set explicitly.
//! Self-detection compares this node's real gossip addresses: treating
//! "same port + wildcard bind" as "this is me" would discard every peer,
//! because a fleet shares one gossip port.
//!
//! Reported membership is filtered to this node's group. Leave is graceful.
//!
//! postvec never joins this mesh. Membership exists so `/config` and
//! `postvec-server status` can describe the fleet. It replicates nothing,
//! elects nothing and balances nothing.

use crate::config::Settings;
use crate::state::NodeIdentity;
use memberlist::{
    delegate::{CompositeDelegate, NodeDelegate, VoidDelegate},
    net::{resolver::socket_addr::SocketAddrResolver, NetTransportOptions},
    proto::{MaybeResolvedAddress, Meta, NodeId},
    tokio::{TokioRuntime, TokioTcpMemberlist},
    Options,
};
use serde::{Deserialize, Serialize};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;
use uuid::Uuid;

/// How often the maintenance worker re-checks membership.
pub const MAINTENANCE_INTERVAL: Duration = Duration::from_secs(30);
/// How long a graceful leave waits for its broadcast.
const LEAVE_TIMEOUT: Duration = Duration::from_secs(3);

/// Gossiped node metadata. Kept small on purpose: `memberlist` caps it, and
/// the cap is enforced below rather than discovered in production.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NodeMetadata {
    pub api_address: String,
    pub grpc_address: String,
    pub group_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub frontend_address: Option<String>,
    /// The build this peer runs. Version skew across a fleet is the second
    /// most common cause of inventory drift, after somebody forgetting to
    /// pull, and it is invisible without this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
}

/// A member, flattened for `/config` and `status`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClusterMember {
    pub address: String,
    pub grpc: String,
    pub group: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub frontend_address: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub version: Option<String>,
    /// True for the node that answered the request.
    pub current: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum ClusterError {
    #[error("memberlist configuration: {0}")]
    Config(String),
    #[error("no peer could be contacted: {0}")]
    Join(String),
}

#[derive(Clone)]
struct LocalDelegate {
    meta: Arc<NodeMetadata>,
}

/// Only `node_meta` is implemented; every other delegate hook keeps its
/// default. This node broadcasts its addresses and consumes nobody else's
/// user data — there is no state to merge and no message to handle.
impl NodeDelegate for LocalDelegate {
    async fn node_meta(&self, limit: usize) -> Meta {
        let bytes = match serde_json::to_vec(&*self.meta) {
            Ok(b) => b,
            Err(e) => {
                log::error!("cannot serialize node metadata: {e}");
                return Meta::empty();
            }
        };
        if bytes.len() > limit {
            // Empty metadata still gossips liveness; peers fall back to the
            // raw gossip address. Losing the addresses is bad, so say so
            // loudly rather than shipping a truncated struct.
            log::warn!(
                "node metadata is {} bytes, over the {limit}-byte gossip limit; peers will see \
                 this node without its addresses (shorten --frontend or --group)",
                bytes.len()
            );
            return Meta::empty();
        }
        match Meta::try_from(bytes) {
            Ok(meta) => meta,
            Err(e) => {
                log::warn!("cannot build gossip metadata: {e}");
                Meta::empty()
            }
        }
    }
}

type LocalCompositeDelegate = CompositeDelegate<
    NodeId,
    SocketAddr,
    VoidDelegate<NodeId, SocketAddr>,
    VoidDelegate<NodeId, SocketAddr>,
    VoidDelegate<NodeId, SocketAddr>,
    VoidDelegate<NodeId, SocketAddr>,
    LocalDelegate,
    VoidDelegate<NodeId, SocketAddr>,
>;

type LocalResolver = SocketAddrResolver<TokioRuntime>;
type LocalMemberlist = TokioTcpMemberlist<NodeId, LocalResolver, LocalCompositeDelegate>;

pub struct ClusterManager {
    memberlist: LocalMemberlist,
    metadata: Arc<NodeMetadata>,
    /// This node's own gossip addresses — the ones a seed list may legally
    /// contain and that must not be dialled.
    own_addrs: Vec<SocketAddr>,
    /// Configured peers that are *not* this node. Empty means "nothing to
    /// join", which is a correct single-node cluster rather than a degraded
    /// one.
    seeds: Vec<SocketAddr>,
}

/// The gossip addresses that mean "this node".
///
/// A wildcard bind means "I listen on every interface", **not** "every
/// address with my port is me" — conflating those two is what makes a seed
/// list evaporate. With a wildcard bind the node is reachable at its
/// advertised address and at loopback; with a specific bind, at that address.
fn own_gossip_addrs(bind: IpAddr, advertise: IpAddr, port: u16) -> Vec<SocketAddr> {
    let mut addrs = vec![SocketAddr::new(advertise, port)];
    if bind.is_unspecified() {
        addrs.push(SocketAddr::new(std::net::Ipv4Addr::LOCALHOST.into(), port));
        addrs.push(SocketAddr::new(std::net::Ipv6Addr::LOCALHOST.into(), port));
    } else {
        addrs.push(SocketAddr::new(bind, port));
    }
    addrs.sort();
    addrs.dedup();
    addrs
}

impl ClusterManager {
    pub async fn start(
        settings: &Settings,
        identity: &NodeIdentity,
        seeds: Vec<SocketAddr>,
    ) -> Result<Self, ClusterError> {
        let metadata = Arc::new(NodeMetadata {
            api_address: identity.api_address.clone(),
            grpc_address: identity.grpc_address.clone(),
            group_name: settings.group.clone(),
            // Only carried when it says something `api_address` does not.
            frontend_address: (identity.frontend != identity.api_address)
                .then(|| identity.frontend.clone()),
            version: Some(env!("CARGO_PKG_VERSION").to_string()),
        });

        let delegate: LocalCompositeDelegate =
            CompositeDelegate::default().with_node_delegate(LocalDelegate {
                meta: Arc::clone(&metadata),
            });
        let node_id = NodeId::new(Uuid::new_v4().to_string())
            .map_err(|e| ClusterError::Config(format!("node id: {e:?}")))?;

        // Gossip always binds the wildcard address: binding the advertised IP
        // directly breaks the common container case, where the routable
        // address is not present on any interface inside the namespace. The
        // advertise address is set separately, below, and that is what peers
        // actually dial.
        let bind_addr = SocketAddr::new(settings.bind, settings.gossip_port);
        let mut transport: NetTransportOptions<NodeId, LocalResolver, memberlist::tokio::TokioTcp> =
            NetTransportOptions::new(node_id);
        transport.add_bind_address(bind_addr);
        let transport = transport
            .with_advertise_address(SocketAddr::new(identity.advertise, settings.gossip_port));

        let memberlist = LocalMemberlist::with_delegate(delegate, transport, Options::lan())
            .await
            .map_err(|e| {
                ClusterError::Config(format!(
                    "cannot start gossip on {bind_addr}: {e:?}\n\
                     (is another process already using --gossip {}?)",
                    settings.gossip_port
                ))
            })?;

        let own_addrs = own_gossip_addrs(settings.bind, identity.advertise, settings.gossip_port);
        let (mine, external): (Vec<_>, Vec<_>) =
            seeds.into_iter().partition(|addr| own_addrs.contains(addr));
        if !mine.is_empty() {
            log::debug!(
                "{} configured peer(s) are this node and will not be dialled",
                mine.len()
            );
        }

        Ok(Self {
            memberlist,
            metadata,
            own_addrs,
            seeds: external,
        })
    }

    /// This node's own gossip addresses, for diagnostics.
    pub fn own_addrs(&self) -> &[SocketAddr] {
        &self.own_addrs
    }

    pub fn seeds(&self) -> &[SocketAddr] {
        &self.seeds
    }

    /// Contact seeds until one answers. Best-effort by design: a node that
    /// cannot see its peers still serves the clients that can see *it*, and
    /// the maintenance worker keeps retrying.
    pub async fn join(&self) -> Result<(), ClusterError> {
        if self.seeds.is_empty() {
            log::info!("no peers to dial; running as a single-node cluster");
            return Ok(());
        }
        let mut failures = Vec::new();
        for addr in &self.seeds {
            match self
                .memberlist
                .join(MaybeResolvedAddress::Resolved(*addr))
                .await
            {
                Ok(contacted) => {
                    log::info!("joined the cluster via {contacted}");
                    return Ok(());
                }
                Err(e) => {
                    log::warn!("peer {addr} did not answer: {e:?}");
                    failures.push(format!("{addr}: {e:?}"));
                }
            }
        }
        Err(ClusterError::Join(failures.join("; ")))
    }

    /// Members of *this node's group*, this node included.
    ///
    /// Foreign groups are never surfaced: the tag exists to keep unrelated
    /// fleets apart, and reporting a node you must not route to would defeat
    /// it.
    pub async fn members(&self) -> Vec<ClusterMember> {
        let own_api = &self.metadata.api_address;
        self.memberlist
            .members()
            .await
            .iter()
            .filter_map(|node| {
                let node = node.as_ref();
                let metadata: Option<NodeMetadata> =
                    serde_json::from_slice(node.meta().as_bytes()).ok();
                let group = metadata
                    .as_ref()
                    .map(|m| m.group_name.clone())
                    .unwrap_or_else(|| "unknown".to_string());
                if group != self.metadata.group_name {
                    return None;
                }
                let address = metadata
                    .as_ref()
                    .map(|m| m.api_address.clone())
                    .unwrap_or_else(|| node.address().to_string());
                Some(ClusterMember {
                    current: address == *own_api,
                    grpc: metadata
                        .as_ref()
                        .map(|m| m.grpc_address.clone())
                        .unwrap_or_else(|| node.address().to_string()),
                    frontend_address: metadata.as_ref().and_then(|m| m.frontend_address.clone()),
                    version: metadata.as_ref().and_then(|m| m.version.clone()),
                    address,
                    group,
                    status: node.state().to_string(),
                })
            })
            .collect()
    }

    /// Broadcast a departure, then stop. Peers mark this node dead at once
    /// instead of waiting out the suspicion timeout, which is the difference
    /// between a rolling restart that looks clean and one that leaves a
    /// `suspect` node in every peer's `/config` for tens of seconds.
    /// Announce the departure, leaving gossip running.
    ///
    /// The announcement is queued on the broadcast queue and goes out on the
    /// gossip loop's own tick, so the transport has to stay up for a moment
    /// afterwards or peers fall back to learning about it through anti-entropy
    /// — thirty seconds by default, which is exactly the delay this exists to
    /// avoid. The caller therefore announces *first* and tears down after the
    /// drain window, rather than doing both in one breath.
    /// Returns whether the announcement was made — `false` when there was
    /// nothing to announce to, or when the broadcast could not be sent.
    pub async fn announce_departure(&self) -> bool {
        match self.memberlist.leave(LEAVE_TIMEOUT).await {
            Ok(true) => {
                log::info!("announced departure to the cluster");
                true
            }
            Ok(false) => false,
            Err(e) => {
                log::warn!("announcing the departure failed: {e:?}");
                false
            }
        }
    }

    /// Stop gossiping. Call after [`Self::announce_departure`] and after the
    /// drain window.
    pub async fn shutdown(&self) {
        if let Err(e) = self.memberlist.shutdown().await {
            log::warn!("gossip shutdown failed: {e:?}");
        }
    }
}

/// Count members that are alive or merely suspect. A suspect peer is not yet
/// evidence of isolation, so it does not trigger a re-join storm.
fn active_count(members: &[ClusterMember]) -> usize {
    members
        .iter()
        .filter(|m| {
            m.status.eq_ignore_ascii_case("alive") || m.status.eq_ignore_ascii_case("suspect")
        })
        .count()
}

/// Should a node with this view try to re-join?
///
/// Two triggers: total isolation, and seeing fewer peers than there are
/// seeds. The second one matters because a partition can heal on one side
/// only, leaving a node permanently half-connected with nothing to notice it.
///
/// With no configured peers there is nothing to re-join to, so a
/// single-node deployment must not qualify.
pub fn should_rejoin(active: usize, seed_count: usize) -> bool {
    seed_count > 0 && (active <= 1 || active < seed_count)
}

/// Re-join whenever the view looks degraded. Runs until the process exits.
pub async fn maintenance_worker(cluster: Arc<ClusterManager>) {
    let mut interval = tokio::time::interval(MAINTENANCE_INTERVAL);
    // A missed tick must not produce a burst of catch-up joins.
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        interval.tick().await;
        let members = cluster.members().await;
        let active = active_count(&members);
        let seeds = cluster.seeds().len();
        if should_rejoin(active, seeds) {
            log::warn!(
                "cluster looks degraded (visible: {active}, configured peers: {seeds}); \
                 re-joining"
            );
            match cluster.join().await {
                Ok(()) => log::info!("re-join succeeded"),
                Err(e) => log::warn!("re-join failed: {e}"),
            }
        } else {
            log::debug!("cluster healthy ({active} visible members)");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn member(status: &str) -> ClusterMember {
        ClusterMember {
            address: "https://10.0.0.1:22222".into(),
            grpc: "10.0.0.1:33333".into(),
            group: "postvec".into(),
            status: status.into(),
            frontend_address: None,
            version: None,
            current: false,
        }
    }

    #[test]
    fn suspect_peers_still_count_as_visible() {
        let members = [member("alive"), member("suspect"), member("dead")];
        assert_eq!(active_count(&members), 2);
    }

    /// Same gossip port on every node must not classify every seed as self.
    #[test]
    fn only_this_nodes_real_addresses_count_as_self() {
        let own = own_gossip_addrs(
            "0.0.0.0".parse().unwrap(),
            "10.0.0.10".parse().unwrap(),
            11111,
        );
        assert!(own.contains(&"10.0.0.10:11111".parse().unwrap()), "{own:?}");
        assert!(own.contains(&"127.0.0.1:11111".parse().unwrap()), "{own:?}");
        // The peers: same port, different host. These must be dialled.
        for peer in ["10.0.0.11:11111", "10.0.0.12:11111"] {
            assert!(
                !own.contains(&peer.parse().unwrap()),
                "{peer} was misclassified as this node: {own:?}"
            );
        }
        // A different port on this node's own address is a different node.
        assert!(
            !own.contains(&"10.0.0.10:11112".parse().unwrap()),
            "{own:?}"
        );
    }

    #[test]
    fn a_specific_bind_is_its_own_address() {
        let own = own_gossip_addrs(
            "127.0.0.2".parse().unwrap(),
            "127.0.0.2".parse().unwrap(),
            11111,
        );
        assert_eq!(own, vec!["127.0.0.2:11111".parse::<SocketAddr>().unwrap()]);
        assert!(!own.contains(&"127.0.0.3:11111".parse().unwrap()));
    }

    #[test]
    fn rejoin_triggers_on_isolation_and_on_a_short_view() {
        // A single-node cluster has no seeds and must not thrash.
        assert!(!should_rejoin(1, 0), "1 member, 0 seeds is a single node");
        // Isolated inside a real fleet.
        assert!(should_rejoin(1, 3));
        assert!(should_rejoin(0, 3));
        // Half-healed partition: fewer peers visible than configured.
        assert!(should_rejoin(2, 3));
        // Fully connected.
        assert!(!should_rejoin(3, 3));
        assert!(!should_rejoin(4, 3), "an extra peer joined; that is fine");
    }

    #[test]
    fn metadata_round_trips_and_omits_absent_fields() {
        let meta = NodeMetadata {
            api_address: "https://10.0.0.1:22222".into(),
            grpc_address: "10.0.0.1:33333".into(),
            group_name: "postvec".into(),
            frontend_address: None,
            version: Some("0.1.0".into()),
        };
        let raw = serde_json::to_vec(&meta).unwrap();
        let value: serde_json::Value = serde_json::from_slice(&raw).unwrap();
        assert!(
            value.get("frontend_address").is_none(),
            "an absent frontend must not spend gossip bytes"
        );
        let back: NodeMetadata = serde_json::from_slice(&raw).unwrap();
        assert_eq!(back.api_address, meta.api_address);
        assert_eq!(back.version, meta.version);
    }

    /// A peer running an older build predates `version`; its metadata must
    /// still deserialize.
    #[test]
    fn metadata_from_an_older_peer_still_parses() {
        let raw = br#"{"api_address":"http://a:1","grpc_address":"a:2","group_name":"postvec"}"#;
        let meta: NodeMetadata = serde_json::from_slice(raw).unwrap();
        assert!(meta.version.is_none());
        assert!(meta.frontend_address.is_none());
    }

    /// Realistic metadata has to fit comfortably inside the gossip cap.
    #[test]
    fn metadata_stays_small() {
        let meta = NodeMetadata {
            api_address: "https://255.255.255.255:65535".into(),
            grpc_address: "255.255.255.255:65535".into(),
            group_name: "a-fairly-long-customer-fleet-name".into(),
            frontend_address: Some("https://inference.internal.example.test:65535".into()),
            version: Some("10.20.30-rc.1".into()),
        };
        let len = serde_json::to_vec(&meta).unwrap().len();
        assert!(len < 512, "gossip metadata is {len} bytes");
    }
}
