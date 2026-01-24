//! Two real gossip nodes, on one machine, using **identical ports**.
//!
//! That last part is the whole point. A postvec-server fleet runs the same
//! command on every node, so every node uses the same gossip port and a
//! seed list is a list of bare hosts. The bug this suite exists to catch is
//! the one inherited from ninference: deciding a seed is "me" because the
//! *ports* match while the local bind is a wildcard, which — since every node
//! in a fleet shares the gossip port — throws away the entire seed list and
//! leaves every node seeing only itself.
//!
//! Distinct loopback addresses (`127.0.0.2`, `127.0.0.3`, …) give two nodes
//! the same port on one host, which is exactly the shape a fleet has and
//! exactly the shape a same-port test needs. All of `127.0.0.0/8` is local on
//! Linux; on a platform where it is not, these tests will fail to bind and
//! say so.

use postvec_server::cli::ServeArgs;
use postvec_server::cluster::ClusterManager;
use postvec_server::config::{self, FileConfig, Settings};
use postvec_server::net;
use postvec_server::state::NodeIdentity;
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU16, Ordering};
use std::time::Duration;

/// Ports for these tests. Every node in one test shares a port — that is the
/// property under test — so the counter advances per test, not per node.
fn next_port() -> u16 {
    static NEXT: AtomicU16 = AtomicU16::new(0);
    let offset = NEXT.fetch_add(1, Ordering::Relaxed);
    let base = 19_300 + offset * 4;
    for candidate in base..base + 4 {
        if port_is_free(candidate) {
            return candidate;
        }
    }
    panic!("no free gossip port near {base}");
}

/// memberlist's TCP transport also opens a UDP socket on the same port.
fn port_is_free(port: u16) -> bool {
    let addr: SocketAddr = format!("127.0.0.1:{port}").parse().unwrap();
    std::net::TcpListener::bind(addr).is_ok() && std::net::UdpSocket::bind(addr).is_ok()
}

fn settings(bind: &str, port: u16, group: &str, peers: &[String]) -> Settings {
    config::resolve(
        &ServeArgs {
            bind: Some(bind.to_string()),
            advertise: Some(bind.to_string()),
            gossip: Some(port),
            group: Some(group.to_string()),
            peers: (!peers.is_empty()).then(|| peers.to_vec()),
            insecure: true,
            ..Default::default()
        },
        &FileConfig::default(),
        &BTreeMap::new(),
        std::path::PathBuf::from("/nonexistent-root"),
        None,
    )
    .expect("settings")
}

async fn node(bind: &str, port: u16, group: &str, peers: &[String]) -> ClusterManager {
    // Gossip failures surface as log warnings, which are invisible without
    // this. Run with `RUST_LOG=info` when one of these tests puzzles you.
    let _ = env_logger::builder().is_test(true).try_init();
    let settings = settings(bind, port, group, peers);
    let advertise = net::resolve_advertise(&settings).expect("advertise");
    let identity = NodeIdentity::new(&settings, advertise);
    let (seeds, problems) = net::resolve_peers(&settings.peers, settings.gossip_port).await;
    assert!(problems.is_empty(), "{problems:?}");
    ClusterManager::start(&settings, &identity, seeds)
        .await
        .unwrap_or_else(|e| panic!("gossip on {bind}:{port}: {e}"))
}

async fn await_members(node: &ClusterManager, want: usize, what: &str) -> Vec<String> {
    for _ in 0..300 {
        let members = node.members().await;
        if members.len() == want {
            return members.into_iter().map(|m| m.address).collect();
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    let members = node.members().await;
    panic!(
        "{what}: expected {want} member(s), saw {}: {:?}",
        members.len(),
        members
            .iter()
            .map(|m| (&m.address, &m.status))
            .collect::<Vec<_>>()
    );
}

/// The regression test. Two nodes, same port, each seeded with the other's
/// bare address — the exact configuration a fleet has.
#[tokio::test]
async fn two_nodes_on_identical_ports_find_each_other() {
    let port = next_port();
    let a_addr = format!("127.0.0.2:{port}");
    let b_addr = format!("127.0.0.3:{port}");

    // Each node's peer list contains *both* nodes, including itself — which
    // is what an operator writes when every node runs the same command, and
    // which the self-filter has to handle without discarding the other one.
    let peers = vec![a_addr.clone(), b_addr.clone()];
    let a = node("127.0.0.2", port, "postvec", &peers).await;
    let b = node("127.0.0.3", port, "postvec", &peers).await;

    a.join().await.expect("a joins");
    b.join().await.expect("b joins");

    let seen_by_a = await_members(&a, 2, "node A").await;
    let seen_by_b = await_members(&b, 2, "node B").await;
    for seen in [&seen_by_a, &seen_by_b] {
        assert!(
            seen.iter().any(|addr| addr.contains("127.0.0.2")),
            "{seen:?}"
        );
        assert!(
            seen.iter().any(|addr| addr.contains("127.0.0.3")),
            "{seen:?}"
        );
    }

    // Exactly one member is flagged as the local node, on each side.
    for (label, node) in [("A", &a), ("B", &b)] {
        let current: Vec<_> = node
            .members()
            .await
            .into_iter()
            .filter(|m| m.current)
            .collect();
        assert_eq!(current.len(), 1, "node {label}: {current:?}");
    }

    a.shutdown().await;
    b.shutdown().await;
}

/// A node's own addresses may appear in its seed list — every node running
/// the same command guarantees they will — and that must not be mistaken for
/// having no peers to dial.
#[tokio::test]
async fn a_seed_list_of_only_this_node_is_a_single_node_cluster() {
    let port = next_port();
    let solo = node("127.0.0.4", port, "postvec", &[format!("127.0.0.4:{port}")]).await;
    assert!(solo.seeds().is_empty(), "{:?}", solo.seeds());
    assert!(
        !solo.own_addrs().is_empty(),
        "the node must know its own gossip address"
    );
    // Joining is a no-op, not an error, and the maintenance heuristic must
    // not call this degraded — otherwise a single-node install logs a
    // "cluster appears degraded" warning every thirty seconds, forever.
    solo.join().await.expect("a solo join is not a failure");
    assert!(!postvec_server::cluster::should_rejoin(
        1,
        solo.seeds().len()
    ));
    solo.shutdown().await;
}

/// The group tag keeps unrelated fleets apart. A node in another group is not
/// merged and is not reported, even when it is reachable and seeded.
#[tokio::test]
async fn nodes_in_different_groups_do_not_merge() {
    let port = next_port();
    let peers = vec![format!("127.0.0.5:{port}"), format!("127.0.0.6:{port}")];
    let ours = node("127.0.0.5", port, "postvec", &peers).await;
    let theirs = node("127.0.0.6", port, "someone-elses-fleet", &peers).await;

    ours.join().await.expect("ours joins");
    theirs.join().await.expect("theirs joins");

    // Gossip does reach across — memberlist has no notion of our tag — so the
    // filtering has to happen where membership is reported.
    tokio::time::sleep(Duration::from_millis(500)).await;
    for member in ours.members().await {
        assert_eq!(
            member.group, "postvec",
            "a foreign group leaked: {member:?}"
        );
        assert!(
            !member.address.contains("127.0.0.6"),
            "the other fleet's node is visible: {member:?}"
        );
    }
    for member in theirs.members().await {
        assert_eq!(member.group, "someone-elses-fleet", "{member:?}");
    }

    ours.shutdown().await;
    theirs.shutdown().await;
}

/// A departing node announces itself, and keeps gossiping while it drains.
///
/// What is asserted here is the local half, because it is the half that is
/// deterministic. The remote half — how quickly a peer *observes* the
/// departure — is a property of the gossip transport: the announcement rides
/// the broadcast queue over UDP, and where UDP gossip is degraded (as it is
/// between loopback aliases in one process on some kernels) peers fall back
/// to anti-entropy, which is thirty seconds by default. Asserting a wall
/// clock here would be testing the host's networking, not this code.
///
/// The code's contribution is ordering, and `serve()` gets it right: announce
/// first, keep gossiping through the drain window, tear down last. Announcing
/// and shutting down in one breath is what makes peers wait out anti-entropy.
#[tokio::test]
async fn a_departing_node_announces_without_tearing_down_gossip() {
    let port = next_port();
    let peers = vec![format!("127.0.0.9:{port}"), format!("127.0.0.10:{port}")];
    let survivor = node("127.0.0.9", port, "postvec", &peers).await;
    let leaver = node("127.0.0.10", port, "postvec", &peers).await;

    survivor.join().await.expect("survivor joins");
    leaver.join().await.expect("leaver joins");
    await_members(&survivor, 2, "before the departure").await;

    assert!(
        leaver.announce_departure().await,
        "the departure was not broadcast"
    );

    // Announcing does not stop the node gossiping — that is the ordering
    // property `serve()` depends on. If the transport went down here, the
    // announcement would have nothing to travel over and peers would be left
    // waiting for anti-entropy.
    assert!(
        !leaver.members().await.is_empty(),
        "announcing a departure must not tear the transport down"
    );
    assert!(
        !survivor.members().await.is_empty(),
        "the survivor lost its own membership"
    );

    // A second announcement is a no-op rather than an error; shutdown paths
    // get retried.
    assert!(!leaver.announce_departure().await);

    leaver.shutdown().await;
    survivor.shutdown().await;
}
