//! Probing remote ninference nodes: DNS, TCP, `/health`, `/config`.

use super::{parse_config_body, HttpProbes, ProbeOutcome};
use crate::error::Result;
use crate::facts::{GrpcProbe, HealthOutcome, HttpProbe, RemoteProbe};
use crate::validate::{GrpcEndpoint, HttpEndpoint};
use std::time::{Duration, Instant};

/// How many endpoint probes run at once. Bounded so a large topology cannot
/// open an unbounded number of sockets.
const CONCURRENCY: usize = 4;

/// Probe every configured endpoint. Never fails as a whole: an unreachable
/// node is a finding, not an error.
pub async fn probe(
    grpc: &[GrpcEndpoint],
    http: &[HttpEndpoint],
    policy: crate::cli::TlsPolicy,
    timeout: Duration,
) -> Result<RemoteProbe> {
    let clients = HttpProbes::new(policy, timeout)?;
    let grpc_results = run_bounded(grpc.iter().map(|endpoint| {
        let endpoint = endpoint.clone();
        async move { probe_grpc(&endpoint, timeout).await }
    }))
    .await;
    let http_results = run_bounded(http.iter().map(|endpoint| {
        let endpoint = endpoint.clone();
        let clients = &clients;
        async move { probe_http(clients, &endpoint).await }
    }))
    .await;
    Ok(RemoteProbe {
        grpc: grpc_results,
        http: http_results,
        malformed: Vec::new(),
    })
}

/// Run futures with a fixed window of concurrency, preserving input order.
///
/// `buffered` (rather than `buffer_unordered`) so probe results line up with the
/// configured endpoint order, which is what the operator sees and what the
/// extension's round-robin follows.
async fn run_bounded<F, T>(futures: impl IntoIterator<Item = F>) -> Vec<T>
where
    F: std::future::Future<Output = T>,
{
    use futures_util::stream::StreamExt;
    futures_util::stream::iter(futures)
        .buffered(CONCURRENCY)
        .collect()
        .await
}

async fn probe_grpc(endpoint: &GrpcEndpoint, timeout: Duration) -> GrpcProbe {
    let started = Instant::now();
    let mut probe = GrpcProbe {
        endpoint: endpoint.clone(),
        resolved: Vec::new(),
        resolve_error: None,
        connected: false,
        connect_error: None,
        duration_ms: 0,
    };

    let lookup = tokio::time::timeout(
        timeout,
        tokio::net::lookup_host((endpoint.host.as_str(), endpoint.port)),
    )
    .await;
    let addresses = match lookup {
        Ok(Ok(iter)) => iter.collect::<Vec<_>>(),
        Ok(Err(e)) => {
            probe.resolve_error = Some(format!("{e}"));
            probe.duration_ms = started.elapsed().as_millis() as u64;
            return probe;
        }
        Err(_) => {
            probe.resolve_error = Some(format!(
                "name resolution did not finish within {}",
                humantime::format_duration(timeout)
            ));
            probe.duration_ms = started.elapsed().as_millis() as u64;
            return probe;
        }
    };
    probe.resolved = addresses.iter().map(|a| a.to_string()).collect();
    if addresses.is_empty() {
        probe.resolve_error = Some("resolved to no addresses".to_string());
        probe.duration_ms = started.elapsed().as_millis() as u64;
        return probe;
    }

    // TCP only. This proves transport reachability, which is all a
    // non-invasive probe can prove without speaking gRPC to a real service.
    let mut last_error = None;
    for address in &addresses {
        match tokio::time::timeout(timeout, tokio::net::TcpStream::connect(address)).await {
            Ok(Ok(stream)) => {
                drop(stream);
                probe.connected = true;
                break;
            }
            Ok(Err(e)) => last_error = Some(format!("{address}: {e}")),
            Err(_) => {
                last_error = Some(format!(
                    "{address}: no answer within {}",
                    humantime::format_duration(timeout)
                ))
            }
        }
    }
    if !probe.connected {
        probe.connect_error = last_error;
    }
    probe.duration_ms = started.elapsed().as_millis() as u64;
    probe
}

async fn probe_http(clients: &HttpProbes, endpoint: &HttpEndpoint) -> HttpProbe {
    let started = Instant::now();
    let mut probe = HttpProbe {
        endpoint: endpoint.clone(),
        health: HealthOutcome::Failed {
            detail: "not probed".to_string(),
        },
        config_status: None,
        config: None,
        error: None,
        tls_verified: None,
        duration_ms: 0,
    };

    probe.health = match clients.get(&endpoint.health_url(), endpoint.is_https).await {
        ProbeOutcome::Answered { status, .. } => match status {
            200..=299 => HealthOutcome::Ok { status },
            // Older nodes do not implement /health at all; that is not a
            // failure of the deployment.
            404 | 405 | 501 => HealthOutcome::NotImplemented { status },
            other => HealthOutcome::Failed {
                detail: format!("HTTP {other}"),
            },
        },
        ProbeOutcome::Failed { detail } => HealthOutcome::Failed { detail },
    };

    match clients.get(&endpoint.config_url(), endpoint.is_https).await {
        ProbeOutcome::Answered {
            status,
            body,
            tls_verified,
        } => {
            probe.config_status = Some(status);
            probe.tls_verified = tls_verified;
            if (200..=299).contains(&status) {
                match parse_config_body(&body) {
                    Ok(inventory) => probe.config = Some(inventory),
                    Err(e) => probe.error = Some(e.to_string()),
                }
            } else {
                probe.error = Some(format!("GET /config returned HTTP {status}"));
            }
        }
        ProbeOutcome::Failed { detail } => probe.error = Some(detail),
    }

    probe.duration_ms = started.elapsed().as_millis() as u64;
    probe
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::validate;

    #[tokio::test]
    async fn tcp_probe_succeeds_against_a_real_listener() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let endpoint = validate::grpc_endpoint(&format!("127.0.0.1:{port}")).unwrap();
        let probe = probe_grpc(&endpoint, Duration::from_secs(2)).await;
        assert!(probe.connected, "{:?}", probe.connect_error);
        assert!(probe.resolve_error.is_none());
        assert!(!probe.resolved.is_empty());
    }

    #[tokio::test]
    async fn tcp_probe_reports_a_closed_port_without_failing() {
        // Bind then drop, so the port is almost certainly free.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        drop(listener);
        let endpoint = validate::grpc_endpoint(&format!("127.0.0.1:{port}")).unwrap();
        let probe = probe_grpc(&endpoint, Duration::from_secs(2)).await;
        assert!(!probe.connected);
        assert!(probe.connect_error.is_some());
    }

    #[tokio::test]
    async fn dns_failure_is_reported_as_a_resolve_error() {
        let endpoint = validate::grpc_endpoint("postvec-no-such-host.invalid:33333").unwrap();
        let probe = probe_grpc(&endpoint, Duration::from_secs(3)).await;
        assert!(!probe.connected);
        assert!(probe.resolve_error.is_some());
    }

    #[tokio::test]
    async fn http_probe_parses_config_and_classifies_missing_health() {
        let server = crate::testing::HttpFixture::start(|path| match path {
            "/config" => (
                200,
                r#"{"success":true,"data":{"models":[
                    {"name":"m","configuration":{"enabled":true,
                     "params":{"model_type":"embed"}}}]}}"#
                    .to_string(),
            ),
            _ => (404, "not found".to_string()),
        })
        .await;
        let endpoint = validate::http_endpoint(&server.base_url()).unwrap();
        let clients = HttpProbes::new(
            crate::cli::TlsPolicy::ExtensionCompatible,
            Duration::from_secs(2),
        )
        .unwrap();
        let probe = probe_http(&clients, &endpoint).await;
        assert!(matches!(
            probe.health,
            HealthOutcome::NotImplemented { status: 404 }
        ));
        assert_eq!(probe.config_status, Some(200));
        assert_eq!(
            probe.config.as_ref().unwrap().enabled_names(),
            ["m".to_string()].into()
        );
        assert!(probe.error.is_none());
        assert_eq!(probe.tls_verified, None, "plain HTTP has nothing to verify");
    }

    #[tokio::test]
    async fn a_200_that_is_not_the_envelope_is_an_error() {
        let server =
            crate::testing::HttpFixture::start(|_| (200, "<html>login</html>".to_string())).await;
        let endpoint = validate::http_endpoint(&server.base_url()).unwrap();
        let clients = HttpProbes::new(
            crate::cli::TlsPolicy::ExtensionCompatible,
            Duration::from_secs(2),
        )
        .unwrap();
        let probe = probe_http(&clients, &endpoint).await;
        assert!(probe.config.is_none());
        assert!(probe.error.is_some());
    }

    #[tokio::test]
    async fn probe_reports_every_endpoint_even_when_some_fail() {
        let server = crate::testing::HttpFixture::start(|path| match path {
            "/config" => (200, r#"{"success":true,"data":{"models":[]}}"#.to_string()),
            _ => (200, "{}".to_string()),
        })
        .await;
        let good = validate::http_endpoint(&server.base_url()).unwrap();
        let bad = validate::http_endpoint("http://127.0.0.1:1").unwrap();
        let result = probe(
            &[],
            &[good, bad],
            crate::cli::TlsPolicy::ExtensionCompatible,
            Duration::from_millis(500),
        )
        .await
        .unwrap();
        assert_eq!(result.http.len(), 2);
        assert!(result.http[0].config.is_some());
        assert!(result.http[1].config.is_none());
    }
}
