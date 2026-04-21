//! Addressing: what this node advertises, and how peer strings become
//! socket addresses.
//!
//! Two things here are load-bearing on a real deployment:
//!
//! - **The advertise address.** On a multi-homed host — WireGuard, Docker,
//!   anything with more than one default route — autodetection picks a NIC
//!   the other nodes cannot reach, and the symptom is a node that serves
//!   fine and sees only itself. So the derivation is explicit, logged, and
//!   warns whenever it had to guess.
//! - **Name resolution.** The upstream engine's cluster configuration is literal
//!   `ip:port` because its seeds come from Ansible. A published server is
//!   pointed at Compose service names and DNS records, so peers are resolved
//!   here before they reach memberlist's socket-address transport.

use crate::config::Settings;
use std::net::{IpAddr, SocketAddr};

/// Where the advertised IP came from, so the boot log can be honest about
/// whether it was chosen or guessed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AdvertiseSource {
    /// `--advertise` / `POSTVEC_SERVER_ADVERTISE` / the file.
    Explicit,
    /// `--bind` named one specific address, so that is the answer.
    Bind,
    /// Autodetected from the routing table. The one case worth a warning.
    Autodetected,
}

#[derive(Debug, Clone, Copy)]
pub struct Advertise {
    pub ip: IpAddr,
    pub source: AdvertiseSource,
}

/// `--advertise` > a specific `--bind` > the host's primary address.
pub fn resolve_advertise(settings: &Settings) -> Result<Advertise, String> {
    if let Some(ip) = settings.advertise {
        return Ok(Advertise {
            ip,
            source: AdvertiseSource::Explicit,
        });
    }
    if !settings.bind.is_unspecified() {
        return Ok(Advertise {
            ip: settings.bind,
            source: AdvertiseSource::Bind,
        });
    }
    let ip = local_ip_address::local_ip().map_err(|e| {
        format!(
            "cannot determine this host's address for cluster advertisement: {e}\n\
             (pass --advertise <IP> — the address other nodes should dial)"
        )
    })?;
    Ok(Advertise {
        ip,
        source: AdvertiseSource::Autodetected,
    })
}

/// The URL a human (or `postvec doctor`) can curl to reach this node's HTTP
/// API.
///
/// This is the upstream engine's `frontend_address`, which exists because the
/// reachable URL is not always `{scheme}://{advertise}:{http_port}` — NAT, a
/// load balancer, a DNS name the operator would rather see printed. It is
/// gossiped and echoed in `/config`; nothing dials it, and postvec never
/// reads it.
///
/// It is never discovered from the network. A database-adjacent process that
/// asks the internet "what is my IP" is the telemetry temptation wearing a
/// different hat.
pub fn frontend_url(settings: &Settings, advertise: IpAddr) -> String {
    if let Some(explicit) = &settings.frontend {
        return explicit.clone();
    }
    let scheme = if settings.tls.is_some() {
        "https"
    } else {
        "http"
    };
    format!("{scheme}://{}", host_port(advertise, settings.http_port))
}

/// `ip:port`, bracketing IPv6 so the result is a usable authority.
pub fn host_port(ip: IpAddr, port: u16) -> String {
    match ip {
        IpAddr::V4(v4) => format!("{v4}:{port}"),
        IpAddr::V6(v6) => format!("[{v6}]:{port}"),
    }
}

/// One `--peers` entry, before any DNS work.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PeerTarget {
    /// A literal address; nothing to resolve.
    Literal(SocketAddr),
    /// A name that has to go through the resolver.
    Named { host: String, port: u16 },
}

/// Parse one peer entry.
///
/// Accepted, in this order: `ip:port`, `[v6]:port`, a bare IP, `[v6]`, a bare
/// name, and `name:port`. A bare entry takes the fleet-wide gossip port,
/// which is the happy path — every node in a fleet uses identical ports, and
/// `host:port` is the escape hatch for the one deployment with a collision.
pub fn parse_peer(raw: &str, default_port: u16) -> Result<PeerTarget, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("empty peer entry".to_string());
    }
    if let Ok(addr) = raw.parse::<SocketAddr>() {
        return Ok(PeerTarget::Literal(addr));
    }
    if let Ok(ip) = raw.parse::<IpAddr>() {
        return Ok(PeerTarget::Literal(SocketAddr::new(ip, default_port)));
    }
    // `[::1]` with no port: an IPv6 literal still wearing its brackets.
    if let Some(inner) = raw.strip_prefix('[').and_then(|r| r.strip_suffix(']')) {
        return match inner.parse::<IpAddr>() {
            Ok(ip) => Ok(PeerTarget::Literal(SocketAddr::new(ip, default_port))),
            Err(e) => Err(format!("peer {raw:?} is not a valid IPv6 literal: {e}")),
        };
    }
    // A name, optionally with a port. Exactly one colon distinguishes
    // `name:port` from a malformed IPv6 literal missing its brackets.
    match raw.rsplit_once(':') {
        Some((host, port)) if !host.contains(':') => {
            let port: u16 = port
                .parse()
                .map_err(|e| format!("peer {raw:?}: {port:?} is not a port number: {e}"))?;
            if host.is_empty() {
                return Err(format!("peer {raw:?} has no host part"));
            }
            if port == 0 {
                return Err(format!("peer {raw:?} must name a real port, not 0"));
            }
            Ok(PeerTarget::Named {
                host: host.to_string(),
                port,
            })
        }
        Some(_) => Err(format!(
            "peer {raw:?} looks like an IPv6 address without brackets; write it as \
             [{raw}]:{default_port}"
        )),
        None => Ok(PeerTarget::Named {
            host: raw.to_string(),
            port: default_port,
        }),
    }
}

/// Resolve every peer entry to socket addresses.
///
/// A name that resolves to several addresses contributes all of them:
/// memberlist tries seeds in order until one answers, so more candidates is
/// strictly better. A name that does not resolve is a **warning**, not a
/// failure — the same posture the rest of joining takes, because a peer that
/// is not in DNS yet is the normal state during a rolling deploy, and the
/// maintenance worker will retry.
pub async fn resolve_peers(peers: &[String], default_port: u16) -> (Vec<SocketAddr>, Vec<String>) {
    let mut resolved: Vec<SocketAddr> = Vec::new();
    let mut problems: Vec<String> = Vec::new();

    for raw in peers {
        match parse_peer(raw, default_port) {
            Ok(PeerTarget::Literal(addr)) => push_unique(&mut resolved, addr),
            Ok(PeerTarget::Named { host, port }) => {
                match tokio::net::lookup_host((host.as_str(), port)).await {
                    Ok(addrs) => {
                        let before = resolved.len();
                        for addr in addrs {
                            push_unique(&mut resolved, addr);
                        }
                        if resolved.len() == before {
                            problems.push(format!("peer {raw:?} resolved to no addresses"));
                        }
                    }
                    Err(e) => problems.push(format!("peer {raw:?} did not resolve: {e}")),
                }
            }
            Err(e) => problems.push(e),
        }
    }
    (resolved, problems)
}

fn push_unique(into: &mut Vec<SocketAddr>, addr: SocketAddr) {
    if !into.contains(&addr) {
        into.push(addr);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cli::ServeArgs;
    use crate::config::{resolve, FileConfig};
    use std::collections::BTreeMap;
    use std::path::PathBuf;

    fn settings(flags: ServeArgs) -> Settings {
        resolve(
            &flags,
            &FileConfig::default(),
            &BTreeMap::new(),
            PathBuf::from("/srv/root"),
            None,
        )
        .unwrap()
    }

    #[test]
    fn explicit_advertise_wins() {
        let s = settings(ServeArgs {
            advertise: Some("10.9.9.9".into()),
            bind: Some("127.0.0.1".into()),
            ..Default::default()
        });
        let a = resolve_advertise(&s).unwrap();
        assert_eq!(a.ip, "10.9.9.9".parse::<IpAddr>().unwrap());
        assert_eq!(a.source, AdvertiseSource::Explicit);
    }

    #[test]
    fn a_specific_bind_is_the_next_best_answer() {
        let s = settings(ServeArgs {
            bind: Some("192.168.5.4".into()),
            ..Default::default()
        });
        let a = resolve_advertise(&s).unwrap();
        assert_eq!(a.ip, "192.168.5.4".parse::<IpAddr>().unwrap());
        assert_eq!(a.source, AdvertiseSource::Bind);
    }

    #[test]
    fn a_wildcard_bind_falls_through_to_autodetection() {
        // 0.0.0.0 and :: are both "unspecified" and neither is dialable.
        for bind in ["0.0.0.0", "::"] {
            let s = settings(ServeArgs {
                bind: Some(bind.into()),
                ..Default::default()
            });
            let a = resolve_advertise(&s).unwrap();
            assert_eq!(a.source, AdvertiseSource::Autodetected, "bind {bind}");
        }
    }

    #[test]
    fn frontend_defaults_to_the_advertised_https_url() {
        let s = settings(ServeArgs::default());
        assert_eq!(
            frontend_url(&s, "10.0.0.10".parse().unwrap()),
            "https://10.0.0.10:22222"
        );
    }

    #[test]
    fn insecure_makes_the_default_frontend_http() {
        let s = settings(ServeArgs {
            insecure: true,
            ..Default::default()
        });
        assert_eq!(
            frontend_url(&s, "10.0.0.10".parse().unwrap()),
            "http://10.0.0.10:22222"
        );
    }

    #[test]
    fn an_explicit_frontend_overrides_only_the_url() {
        let s = settings(ServeArgs {
            frontend: Some("https://nodes.example.test/inference".into()),
            advertise: Some("10.0.0.10".into()),
            ..Default::default()
        });
        let a = resolve_advertise(&s).unwrap();
        assert_eq!(a.ip, "10.0.0.10".parse::<IpAddr>().unwrap());
        assert_eq!(
            frontend_url(&s, a.ip),
            "https://nodes.example.test/inference"
        );
    }

    #[test]
    fn ipv6_frontends_are_bracketed() {
        let s = settings(ServeArgs::default());
        assert_eq!(
            frontend_url(&s, "::1".parse().unwrap()),
            "https://[::1]:22222"
        );
    }

    #[test]
    fn peers_parse_in_every_accepted_shape() {
        let p = |raw| parse_peer(raw, 11111).unwrap();
        assert_eq!(
            p("10.0.0.1"),
            PeerTarget::Literal("10.0.0.1:11111".parse().unwrap()),
            "a bare host takes the fleet gossip port"
        );
        assert_eq!(
            p("10.0.0.1:9999"),
            PeerTarget::Literal("10.0.0.1:9999".parse().unwrap()),
            "the escape hatch for a colliding port"
        );
        assert_eq!(
            p("[fd00::1]:9999"),
            PeerTarget::Literal("[fd00::1]:9999".parse().unwrap())
        );
        assert_eq!(
            p("[fd00::1]"),
            PeerTarget::Literal("[fd00::1]:11111".parse().unwrap())
        );
        assert_eq!(
            p("node-2"),
            PeerTarget::Named {
                host: "node-2".into(),
                port: 11111
            },
            "Compose service names and DNS records are the common case"
        );
        assert_eq!(
            p("node-2.svc.cluster.local:12000"),
            PeerTarget::Named {
                host: "node-2.svc.cluster.local".into(),
                port: 12000
            }
        );
    }

    #[test]
    fn malformed_peers_are_named_precisely() {
        assert!(parse_peer("", 1).unwrap_err().contains("empty"));
        assert!(parse_peer("node:notaport", 1)
            .unwrap_err()
            .contains("not a port"));
        assert!(parse_peer("node:0", 1).unwrap_err().contains("not 0"));
        assert!(parse_peer(":9999", 1).unwrap_err().contains("no host part"));
        // A bare IPv6 literal is unambiguous and simply works.
        assert_eq!(
            parse_peer("fd00::1", 11111).unwrap(),
            PeerTarget::Literal("[fd00::1]:11111".parse().unwrap())
        );
        // Multi-colon text that is neither an address nor a name gets the
        // bracket hint rather than a confusing "not a port".
        let err = parse_peer("fd00::zz:9999", 11111).unwrap_err();
        assert!(err.contains("without brackets"), "{err}");
    }

    #[tokio::test]
    async fn literal_peers_resolve_without_dns_and_deduplicate() {
        let (addrs, problems) = resolve_peers(
            &[
                "10.0.0.1".to_string(),
                "10.0.0.1:11111".to_string(),
                "10.0.0.2:12000".to_string(),
            ],
            11111,
        )
        .await;
        assert_eq!(
            addrs,
            vec![
                "10.0.0.1:11111".parse::<SocketAddr>().unwrap(),
                "10.0.0.2:12000".parse().unwrap(),
            ]
        );
        assert!(problems.is_empty(), "{problems:?}");
    }

    #[tokio::test]
    async fn localhost_resolves_through_the_resolver() {
        let (addrs, problems) = resolve_peers(&["localhost".to_string()], 11111).await;
        assert!(problems.is_empty(), "{problems:?}");
        assert!(
            addrs.iter().all(|a| a.port() == 11111),
            "the fleet gossip port is applied to a bare name: {addrs:?}"
        );
        assert!(
            addrs.iter().any(|a| a.ip().is_loopback()),
            "expected a loopback address, got {addrs:?}"
        );
    }

    /// A peer that is not in DNS yet is normal during a rolling deploy: it
    /// must be reported, not fatal.
    #[tokio::test]
    async fn an_unresolvable_peer_is_a_problem_not_a_panic() {
        let (addrs, problems) = resolve_peers(
            &["10.0.0.1".to_string(), "no-such-host.invalid".to_string()],
            11111,
        )
        .await;
        assert_eq!(addrs, vec!["10.0.0.1:11111".parse::<SocketAddr>().unwrap()]);
        assert_eq!(problems.len(), 1, "{problems:?}");
        assert!(problems[0].contains("no-such-host.invalid"));
    }
}
