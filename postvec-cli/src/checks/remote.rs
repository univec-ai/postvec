//! Checks over remote ninference nodes.
//!
//! Aggregation follows how the extension actually behaves: it round-robins over
//! the configured endpoints and can operate on a partial topology, so one dead
//! node out of three is a warning while losing every node is a failure.

use super::CheckResult;
#[cfg(test)]
use super::CheckStatus;
use crate::facts::{DatabaseCache, HealthOutcome, RemoteProbe};
use serde_json::json;

const AGGREGATE_SCOPE: &str = "inference";

pub struct RemoteInput<'a> {
    pub probe: &'a RemoteProbe,
    /// The model cache of each inspected database, kept separate: a union
    /// would let one database's contents cover for another's gaps.
    pub cached_models: &'a [DatabaseCache],
    /// True when the caller asked for verified TLS only.
    pub tls_strict: bool,
}

pub fn checks(input: &RemoteInput<'_>) -> Vec<CheckResult> {
    let mut out = Vec::new();
    for malformed in &input.probe.malformed {
        out.push(
            CheckResult::fail(
                "remote.grpc.address",
                format!("endpoint:{}", malformed.value),
                format!(
                    "configured endpoint {:?} is not usable: {}",
                    malformed.value, malformed.error
                ),
            )
            .with_fix(
                "the extension skips an unparseable endpoint silently; correct \
                 postvec.ninference_grpc_endpoints / postvec.ninference_http_endpoints and \
                 reload the configuration",
            ),
        );
    }
    if input.probe.grpc.is_empty()
        && input.probe.http.is_empty()
        && input.probe.malformed.is_empty()
    {
        out.push(
            CheckResult::fail(
                "remote.grpc.address",
                AGGREGATE_SCOPE,
                "remote mode is selected but no inference endpoints are configured",
            )
            .required()
            .with_fix(
                "set postvec.ninference_grpc_endpoints and postvec.ninference_http_endpoints \
                 (an empty gRPC list makes the worker skip draining entirely)",
            ),
        );
        return out;
    }

    for probe in &input.probe.grpc {
        let scope = format!("endpoint:{}", probe.endpoint.authority);
        out.push(match &probe.resolve_error {
            Some(error) => CheckResult::fail(
                "remote.grpc.address",
                &scope,
                format!("{} does not resolve", probe.endpoint.host),
            )
            .with_evidence(json!({"error": error}))
            .with_fix("check DNS, /etc/hosts, or use an address instead of a name"),
            None => CheckResult::pass(
                "remote.grpc.address",
                &scope,
                format!(
                    "{} resolves to {}",
                    probe.endpoint.host,
                    probe.resolved.join(", ")
                ),
            ),
        });
        if probe.resolve_error.is_none() {
            out.push(
                if probe.connected {
                    CheckResult::pass(
                        "remote.grpc.connect",
                        &scope,
                        "the gRPC port accepts connections",
                    )
                } else {
                    CheckResult::fail(
                        "remote.grpc.connect",
                        &scope,
                        format!(
                            "the gRPC port is not reachable: {}",
                            probe
                                .connect_error
                                .as_deref()
                                .unwrap_or("no further detail")
                        ),
                    )
                    .with_fix(
                        "gRPC carries all embedding work; check the node, the mesh route and \
                         any firewall. The plaintext port is safe only inside the mesh",
                    )
                }
                .with_duration_ms(probe.duration_ms),
            );
        }
    }

    for probe in &input.probe.http {
        let scope = format!("endpoint:{}", probe.endpoint.base);
        if let Some(check) = tls(probe, &scope, input.tls_strict) {
            out.push(check);
        }
        out.push(match &probe.health {
            HealthOutcome::Ok { status } => CheckResult::pass(
                "remote.http.health",
                &scope,
                format!("GET /health → {status}"),
            ),
            // Not every node version implements the route; absence says nothing
            // about the node's health.
            HealthOutcome::NotImplemented { status } => CheckResult::skip(
                "remote.http.health",
                &scope,
                format!("this node does not implement /health (HTTP {status})"),
            ),
            HealthOutcome::Failed { detail } => CheckResult::warn(
                "remote.http.health",
                &scope,
                format!("GET /health failed: {detail}"),
            )
            .with_fix("/config below is the check that actually matters for discovery"),
        });
        out.push(
            match (&probe.config, &probe.error) {
                (Some(inventory), _) => CheckResult::pass(
                    "remote.http.config",
                    &scope,
                    format!(
                        "GET /config returned {} model(s), {} enabled",
                        inventory.models.len(),
                        inventory.enabled_names().len()
                    ),
                )
                .with_evidence(json!({"enabled": inventory.enabled_names()})),
                (None, Some(error)) => CheckResult::fail(
                    "remote.http.config",
                    &scope,
                    format!("GET /config is not usable: {error}"),
                )
                .with_fix(
                    "the model cache is populated from this endpoint; without it enable() and \
                     migrate() cannot resolve a model",
                ),
                (None, None) => CheckResult::fail(
                    "remote.http.config",
                    &scope,
                    "GET /config did not return an inventory",
                ),
            }
            .with_duration_ms(probe.duration_ms),
        );
    }

    out.extend(aggregate(input));
    out.extend(consistency(input));
    out
}

fn tls(probe: &crate::facts::HttpProbe, scope: &str, strict: bool) -> Option<CheckResult> {
    match probe.tls_verified {
        // Plain HTTP, or the endpoint never answered: nothing to report here.
        None => None,
        Some(true) => Some(CheckResult::pass(
            "remote.http.tls",
            scope,
            "the certificate verified",
        )),
        Some(false) => Some(
            CheckResult::warn(
                "remote.http.tls",
                scope,
                "the certificate did not verify and was accepted anyway",
            )
            .with_fix(if strict {
                "this endpoint would fail under --tls strict".to_string()
            } else {
                "the extension's discovery client also accepts invalid certificates, so this \
                 matches production behaviour; it is safe only because the mesh is the trust \
                 boundary"
                    .to_string()
            }),
        ),
    }
}

/// Whole-topology verdict. Partial availability is explicitly not a failure.
fn aggregate(input: &RemoteInput<'_>) -> Vec<CheckResult> {
    let mut out = Vec::new();
    if !input.probe.grpc.is_empty() {
        let reachable = input.probe.grpc.iter().filter(|p| p.connected).count();
        let total = input.probe.grpc.len();
        if reachable == 0 {
            out.push(
                CheckResult::fail(
                    "remote.grpc.connect",
                    AGGREGATE_SCOPE,
                    format!("none of the {total} configured gRPC endpoint(s) are reachable"),
                )
                .required()
                .with_fix("no embedding or conversion work can complete until one answers"),
            );
        } else if reachable < total {
            out.push(CheckResult::warn(
                "remote.grpc.connect",
                AGGREGATE_SCOPE,
                format!("{reachable} of {total} gRPC endpoint(s) are reachable"),
            ));
        } else {
            out.push(CheckResult::pass(
                "remote.grpc.connect",
                AGGREGATE_SCOPE,
                format!("all {total} gRPC endpoint(s) are reachable"),
            ));
        }
    }
    if !input.probe.http.is_empty() {
        let usable = input
            .probe
            .http
            .iter()
            .filter(|p| p.config.is_some())
            .count();
        let total = input.probe.http.len();
        if usable == 0 {
            out.push(
                CheckResult::fail(
                    "remote.http.config",
                    AGGREGATE_SCOPE,
                    format!("no valid GET /config from any of the {total} HTTP endpoint(s)"),
                )
                .required()
                .with_fix("model discovery is impossible until one endpoint answers"),
            );
        } else if usable < total {
            out.push(CheckResult::warn(
                "remote.http.config",
                AGGREGATE_SCOPE,
                format!("{usable} of {total} HTTP endpoint(s) served a valid /config"),
            ));
        } else {
            out.push(CheckResult::pass(
                "remote.http.config",
                AGGREGATE_SCOPE,
                format!("all {total} HTTP endpoint(s) served a valid /config"),
            ));
        }
    }
    out
}

fn consistency(input: &RemoteInput<'_>) -> Vec<CheckResult> {
    let advertised = input.probe.advertised_enabled();
    if advertised.is_empty()
        && input
            .cached_models
            .iter()
            .all(|cache| cache.models.is_empty())
    {
        return Vec::new();
    }
    let mut out = Vec::new();

    // Nodes disagreeing about a model name matters because the configured
    // endpoint order silently decides the winner.
    let conflicts = input.probe.conflicting_models();
    if !conflicts.is_empty() {
        out.push(
            CheckResult::warn(
                "remote.models.consistency",
                AGGREGATE_SCOPE,
                format!(
                    "{} model name(s) are advertised with different types by different nodes",
                    conflicts.len()
                ),
            )
            .with_evidence(json!({"conflicting": conflicts}))
            .with_fix(
                "the extension takes the first node that answers, in the configured endpoint \
                 order, so the winner is arbitrary; align the nodes' model configurations",
            ),
        );
        return out;
    }

    // Compared per database. A union would report agreement whenever the
    // databases' caches *together* cover the inventory, even though each one is
    // individually incomplete.
    let mut drifted = serde_json::Map::new();
    for cache in input.cached_models {
        let advertised_not_cached: Vec<String> =
            advertised.difference(&cache.models).cloned().collect();
        let cached_not_advertised: Vec<String> =
            cache.models.difference(&advertised).cloned().collect();
        if advertised_not_cached.is_empty() && cached_not_advertised.is_empty() {
            continue;
        }
        drifted.insert(
            cache.database.clone(),
            json!({
                "advertised_not_cached": advertised_not_cached,
                "cached_not_advertised": cached_not_advertised,
            }),
        );
    }
    out.push(if drifted.is_empty() {
        CheckResult::pass(
            "remote.models.consistency",
            AGGREGATE_SCOPE,
            format!(
                "every inspected database's model cache matches what {} node(s) advertise",
                input.probe.http.len()
            ),
        )
    } else {
        CheckResult::warn(
            "remote.models.consistency",
            AGGREGATE_SCOPE,
            format!(
                "{} database(s) have a model cache that differs from the advertised \
                 inventory: {}",
                drifted.len(),
                drifted.keys().cloned().collect::<Vec<_>>().join(", ")
            ),
        )
        .with_evidence(json!({"databases": drifted}))
        .with_fix(
            "run SELECT postvec.refresh_models() in the affected database(s); the cache is \
             only pruned by a refresh in which every configured endpoint answered",
        )
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::facts::{ConfigInventory, GrpcProbe, HttpProbe, InventoryModel};
    use crate::validate;

    fn grpc(authority: &str, connected: bool, resolves: bool) -> GrpcProbe {
        GrpcProbe {
            endpoint: validate::grpc_endpoint(authority).unwrap(),
            resolved: if resolves {
                vec![authority.to_string()]
            } else {
                vec![]
            },
            resolve_error: (!resolves).then(|| "no such host".to_string()),
            connected,
            connect_error: (!connected).then(|| "connection refused".to_string()),
            duration_ms: 7,
        }
    }

    fn http(base: &str, models: &[(&str, bool)], ok: bool) -> HttpProbe {
        HttpProbe {
            endpoint: validate::http_endpoint(base).unwrap(),
            health: HealthOutcome::Ok { status: 200 },
            config_status: Some(if ok { 200 } else { 500 }),
            config: ok.then(|| ConfigInventory {
                models: models
                    .iter()
                    .map(|(name, enabled)| InventoryModel {
                        name: (*name).to_string(),
                        enabled: *enabled,
                        model_type: Some("embed".to_string()),
                        provider: None,
                        provider_file: None,
                        provider_endpoint: None,
                    })
                    .collect(),
            }),
            error: (!ok).then(|| "GET /config returned HTTP 500".to_string()),
            tls_verified: Some(true),
            duration_ms: 12,
        }
    }

    fn cached(names: &[&str]) -> Vec<DatabaseCache> {
        vec![DatabaseCache {
            database: "univec".into(),
            models: names.iter().map(|n| n.to_string()).collect(),
        }]
    }

    #[test]
    fn a_malformed_configured_endpoint_is_reported() {
        let probe = RemoteProbe {
            grpc: vec![],
            http: vec![],
            malformed: vec![crate::facts::MalformedEndpoint {
                value: "http://192.0.2.2:33333".into(),
                error: "gRPC endpoints carry no scheme".into(),
            }],
        };
        let checks = run(probe, &cached(&[]));
        let address = checks
            .iter()
            .find(|c| c.id == "remote.grpc.address")
            .unwrap();
        assert_eq!(address.status, CheckStatus::Fail);
        assert!(address.remediation.clone().unwrap().contains("silently"));
    }

    fn run(probe: RemoteProbe, models: &[DatabaseCache]) -> Vec<CheckResult> {
        checks(&RemoteInput {
            probe: &probe,
            cached_models: models,
            tls_strict: false,
        })
    }

    fn aggregate_status(checks: &[CheckResult], id: &str) -> Option<CheckStatus> {
        checks
            .iter()
            .find(|c| c.id == id && c.scope == AGGREGATE_SCOPE)
            .map(|c| c.status)
    }

    #[test]
    fn a_healthy_topology_passes() {
        let probe = RemoteProbe {
            grpc: vec![grpc("192.0.2.2:33333", true, true)],
            http: vec![http("https://192.0.2.2:22222", &[("m", true)], true)],
            malformed: vec![],
        };
        let checks = run(probe, &cached(&["m"]));
        for check in &checks {
            assert_eq!(
                check.status,
                CheckStatus::Pass,
                "{} ({}) should pass: {}",
                check.id,
                check.scope,
                check.summary
            );
        }
    }

    #[test]
    fn every_emitted_id_is_registered() {
        let probe = RemoteProbe {
            grpc: vec![grpc("a:1", false, true), grpc("b:2", false, false)],
            http: vec![http("https://a", &[("m", true)], false)],
            malformed: vec![],
        };
        for check in run(probe, &cached(&["other"])) {
            assert!(
                super::super::CHECK_ORDER.contains(&check.id),
                "{} is not in CHECK_ORDER",
                check.id
            );
        }
    }

    #[test]
    fn no_configured_endpoints_is_a_required_failure() {
        let checks = run(RemoteProbe::default(), &cached(&[]));
        assert_eq!(checks.len(), 1);
        assert!(checks[0].is_blocking());
        assert!(checks[0]
            .remediation
            .clone()
            .unwrap()
            .contains("skip draining"));
    }

    #[test]
    fn partial_availability_warns_but_total_loss_fails() {
        let partial = RemoteProbe {
            grpc: vec![grpc("a:1", true, true), grpc("b:2", false, true)],
            http: vec![
                http("https://a", &[("m", true)], true),
                http("https://b", &[], false),
            ],
            malformed: vec![],
        };
        let checks = run(partial, &cached(&["m"]));
        assert_eq!(
            aggregate_status(&checks, "remote.grpc.connect"),
            Some(CheckStatus::Warn)
        );
        assert_eq!(
            aggregate_status(&checks, "remote.http.config"),
            Some(CheckStatus::Warn)
        );
        assert!(
            !checks
                .iter()
                .any(|c| c.is_blocking() && c.scope == AGGREGATE_SCOPE),
            "postvec operates on a partial topology"
        );

        let total = RemoteProbe {
            grpc: vec![grpc("a:1", false, true)],
            http: vec![http("https://a", &[], false)],
            malformed: vec![],
        };
        let checks = run(total, &cached(&[]));
        assert_eq!(
            aggregate_status(&checks, "remote.grpc.connect"),
            Some(CheckStatus::Fail)
        );
        assert_eq!(
            aggregate_status(&checks, "remote.http.config"),
            Some(CheckStatus::Fail)
        );
    }

    #[test]
    fn a_dns_failure_suppresses_the_connect_check_for_that_endpoint() {
        let probe = RemoteProbe {
            grpc: vec![grpc("nowhere.invalid:33333", false, false)],
            http: vec![],
            malformed: vec![],
        };
        let checks = run(probe, &cached(&[]));
        let per_endpoint: Vec<&CheckResult> = checks
            .iter()
            .filter(|c| c.scope.starts_with("endpoint:"))
            .collect();
        assert_eq!(per_endpoint.len(), 1);
        assert_eq!(per_endpoint[0].id, "remote.grpc.address");
        assert_eq!(per_endpoint[0].status, CheckStatus::Fail);
    }

    #[test]
    fn a_missing_health_route_is_skipped_not_failed() {
        let mut probe = RemoteProbe {
            grpc: vec![],
            http: vec![http("https://a", &[("m", true)], true)],
            malformed: vec![],
        };
        probe.http[0].health = HealthOutcome::NotImplemented { status: 404 };
        let checks = run(probe, &cached(&["m"]));
        let health = checks
            .iter()
            .find(|c| c.id == "remote.http.health")
            .unwrap();
        assert_eq!(health.status, CheckStatus::Skip);
        assert!(!health.is_blocking());
    }

    #[test]
    fn an_unverified_certificate_warns_and_says_why_it_was_accepted() {
        let mut probe = RemoteProbe {
            grpc: vec![],
            http: vec![http("https://a", &[("m", true)], true)],
            malformed: vec![],
        };
        probe.http[0].tls_verified = Some(false);
        let checks = run(probe, &cached(&["m"]));
        let tls = checks.iter().find(|c| c.id == "remote.http.tls").unwrap();
        assert_eq!(tls.status, CheckStatus::Warn);
        assert!(tls.remediation.clone().unwrap().contains("mesh"));
    }

    #[test]
    fn plain_http_reports_no_tls_check() {
        let probe = RemoteProbe {
            grpc: vec![],
            http: vec![{
                let mut probe = http("http://a", &[("m", true)], true);
                probe.tls_verified = None;
                probe
            }],
            malformed: vec![],
        };
        let checks = run(probe, &cached(&["m"]));
        assert!(!checks.iter().any(|c| c.id == "remote.http.tls"));
    }

    #[test]
    fn cache_and_inventory_differences_are_reported_in_both_directions() {
        let probe = RemoteProbe {
            grpc: vec![],
            http: vec![http("https://a", &[("new", true), ("off", false)], true)],
            malformed: vec![],
        };
        let checks = run(probe, &cached(&["stale"]));
        let consistency = checks
            .iter()
            .find(|c| c.id == "remote.models.consistency")
            .unwrap();
        assert_eq!(consistency.status, CheckStatus::Warn);
        let evidence = format!("{:?}", consistency.evidence);
        assert!(evidence.contains("new"), "{evidence}");
        assert!(evidence.contains("stale"), "{evidence}");
        assert!(
            !evidence.contains("\"off\""),
            "a disabled model is not expected in the cache"
        );
    }

    /// The bug a union hides: database A caches only model "a", database B only
    /// "b". Together they cover the inventory; individually both are broken.
    #[test]
    fn each_databases_cache_is_reconciled_on_its_own() {
        let probe = RemoteProbe {
            grpc: vec![],
            http: vec![http("https://a", &[("a", true), ("b", true)], true)],
            malformed: vec![],
        };
        let split = vec![
            DatabaseCache {
                database: "alpha".into(),
                models: ["a".to_string()].into(),
            },
            DatabaseCache {
                database: "beta".into(),
                models: ["b".to_string()].into(),
            },
        ];
        let checks = run(probe, &split);
        let consistency = checks
            .iter()
            .find(|c| c.id == "remote.models.consistency")
            .unwrap();
        assert_eq!(
            consistency.status,
            CheckStatus::Warn,
            "a union of the two caches would have looked complete"
        );
        let evidence = format!("{:?}", consistency.evidence);
        assert!(evidence.contains("alpha"), "{evidence}");
        assert!(evidence.contains("beta"), "{evidence}");
        assert!(consistency.summary.contains("2 database(s)"));
    }

    #[test]
    fn every_database_matching_the_inventory_passes() {
        let probe = RemoteProbe {
            grpc: vec![],
            http: vec![http("https://a", &[("a", true)], true)],
            malformed: vec![],
        };
        let complete = vec![
            DatabaseCache {
                database: "alpha".into(),
                models: ["a".to_string()].into(),
            },
            DatabaseCache {
                database: "beta".into(),
                models: ["a".to_string()].into(),
            },
        ];
        assert_eq!(
            run(probe, &complete)
                .iter()
                .find(|c| c.id == "remote.models.consistency")
                .unwrap()
                .status,
            CheckStatus::Pass
        );
    }

    #[test]
    fn nodes_disagreeing_about_a_model_take_precedence_over_cache_drift() {
        let probe = RemoteProbe {
            grpc: vec![],
            http: vec![
                {
                    let mut p = http("https://a", &[("m", true)], true);
                    p.config.as_mut().unwrap().models[0].model_type = Some("embed".into());
                    p
                },
                {
                    let mut p = http("https://b", &[("m", true)], true);
                    p.config.as_mut().unwrap().models[0].model_type = Some("convert".into());
                    p
                },
            ],
            malformed: vec![],
        };
        let checks = run(probe, &cached(&[]));
        let consistency = checks
            .iter()
            .find(|c| c.id == "remote.models.consistency")
            .unwrap();
        assert_eq!(consistency.status, CheckStatus::Warn);
        assert!(consistency.summary.contains("different types"));
        assert!(consistency
            .remediation
            .clone()
            .unwrap()
            .contains("endpoint order"));
    }
}
