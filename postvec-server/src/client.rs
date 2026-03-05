//! The node-local subcommands: `status`, `load`, `unload`.
//!
//! All three talk to `127.0.0.1` on the **admin** port, which mirrors the
//! read-only routes precisely so this client never has to negotiate TLS with
//! a self-signed certificate against a machine it is already running on.
//! They are node-local tools by design: nothing here gossip-scans, and
//! nothing here mutates a peer.
//!
//! `status --fleet` is the one exception, and it is read-only. It reads each
//! alive peer's `/config` and compares model inventories, because a fleet
//! whose nodes carry different models is the failure remote mode actually
//! produces: postvec round-robins its configured gRPC endpoints, so a
//! converter present on two nodes out of three fails one request in three,
//! intermittently, with a `MODEL_NOT_LOADED` that looks like a fluke.
//!
//! Exit codes: `0` healthy, `1` reachable but degraded (not ready, inventory
//! drift, a failed model operation), `2` the node could not be reached.

use crate::cli::DEFAULT_ADMIN_PORT;
use crate::cli::{LocalArgs, ModelArgs, StatusArgs};
use crate::models;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

pub const EXIT_OK: i32 = 0;
pub const EXIT_DEGRADED: i32 = 1;
pub const EXIT_UNREACHABLE: i32 = 2;

const DEFAULT_TIMEOUT_SECONDS: u64 = 10;

fn admin_base(args: &LocalArgs) -> String {
    format!(
        "http://127.0.0.1:{}",
        args.admin.unwrap_or(DEFAULT_ADMIN_PORT)
    )
}

fn timeout(args: &LocalArgs) -> Duration {
    Duration::from_secs(args.timeout.unwrap_or(DEFAULT_TIMEOUT_SECONDS).max(1))
}

/// One HTTP client for the whole invocation.
///
/// Invalid certificates are accepted for the same reason postvec's own
/// discovery accepts them: peers serve `/config` with a self-signed
/// certificate on a private network, and the trust boundary is the network,
/// not the PKI. It only ever matters for `--fleet`; the local calls are
/// plain loopback HTTP.
fn http_client(args: &LocalArgs) -> anyhow::Result<reqwest::Client> {
    Ok(reqwest::Client::builder()
        .danger_accept_invalid_certs(true)
        .timeout(timeout(args))
        .build()?)
}

fn unreachable_hint(args: &LocalArgs, error: &dyn std::fmt::Display) -> String {
    format!(
        "cannot reach the local node at {}: {error}\n\
         Is postvec-server running on this host? If it uses a different admin port, pass \
         --admin <PORT>.",
        admin_base(args)
    )
}

// ---- status ------------------------------------------------------------

/// The subset of `/config` this client reads.
struct NodeReport {
    models: Vec<String>,
    /// Provider-backed entries: public name → connector type, read from the
    /// entry's top-level `provider` extra. These have no on-disk descriptor
    /// (the serving truth is a providers.d file), so the descriptor-drift
    /// checks skip them and the fleet report labels them.
    providers: BTreeMap<String, String>,
    /// Public name → the secret-free routing identity behind it. Name parity
    /// alone cannot see two nodes serving one name from different files,
    /// model ids or dimensions — which round-robin turns into intermittent
    /// dimension failures, or worse, same-dimension vectors from a different
    /// model that nothing downstream can detect.
    fingerprints: BTreeMap<String, String>,
    server: Value,
    cluster: Value,
}

fn parse_config(body: &Value) -> NodeReport {
    let data = body.get("data").cloned().unwrap_or(Value::Null);
    let entries = data.get("models").and_then(Value::as_array);
    let models = entries
        .map(|models| {
            models
                .iter()
                .filter_map(|m| m.get("name").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default();
    let providers = entries
        .map(|models| {
            models
                .iter()
                .filter_map(|m| {
                    Some((
                        m.get("name")?.as_str()?.to_string(),
                        m.get("provider")?.as_str()?.to_string(),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    let fingerprints = entries
        .map(|models| {
            models
                .iter()
                .filter(|m| m.get("provider").and_then(Value::as_str).is_some())
                .filter_map(|m| Some((m.get("name")?.as_str()?.to_string(), fingerprint(m))))
                .collect()
        })
        .unwrap_or_default();
    NodeReport {
        models,
        providers,
        fingerprints,
        server: data.get("server").cloned().unwrap_or(Value::Null),
        cluster: data.get("cluster").cloned().unwrap_or(Value::Null),
    }
}

/// The secret-free routing identity of one provider-backed `/config` entry.
///
/// Everything here decides what a caller actually gets back, and nothing here
/// is a credential: the connector type, the providers.d file stem, the id the
/// provider's API is asked for, and the declared dimension. `base_url` is
/// deliberately absent — it is operator-supplied and can carry an internal
/// hostname, and the three fields above already separate every case a fleet
/// can get wrong.
fn fingerprint(entry: &Value) -> String {
    let field = |name: &str| {
        entry
            .get(name)
            .and_then(Value::as_str)
            .unwrap_or("?")
            .to_string()
    };
    let dim = entry
        .get("configuration")
        .and_then(|c| c.get("params"))
        .and_then(|p| p.get("target_dim"))
        .and_then(Value::as_i64)
        .map(|d| d.to_string())
        .unwrap_or_else(|| "?".to_string());
    format!(
        "{}/{}/{} dim {dim} endpoint {}",
        field("provider"),
        field("provider_file"),
        field("provider_model_id"),
        field("provider_endpoint")
    )
}

/// Names every node advertises but does not agree on.
///
/// Set parity answers "is this name everywhere"; it cannot answer "is it the
/// same thing everywhere". Two nodes serving `openai-text-embedding-3-small`
/// from different files, model ids or dimensions are *identical* to a
/// name-set comparison, while round-robin endpoint selection hands one
/// caller each. A dimension mismatch shows up as intermittent dead letters;
/// a same-dimension model mismatch shows up as nothing at all, and poisons
/// the column silently.
fn fingerprint_drift(nodes: &[(String, BTreeMap<String, String>)]) -> Vec<String> {
    if nodes.len() < 2 {
        return Vec::new();
    }
    let names: BTreeSet<&str> = nodes
        .iter()
        .flat_map(|(_, prints)| prints.keys().map(String::as_str))
        .collect();
    let mut drift = Vec::new();
    for name in names {
        let mut seen: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
        for (address, prints) in nodes {
            if let Some(print) = prints.get(name) {
                seen.entry(print.as_str()).or_default().push(address);
            }
        }
        if seen.len() > 1 {
            let detail: Vec<String> = seen
                .iter()
                .map(|(print, addresses)| format!("{print} on {}", addresses.join(", ")))
                .collect();
            drift.push(format!(
                "{name}: served with DIFFERENT provider settings across the fleet — {}. \
                 Round-robin sends callers to either; fix providers.d so every node agrees",
                detail.join(" | ")
            ));
        }
    }
    drift
}

/// Peers worth querying: alive, in this group, and not this node.
fn peer_addresses(cluster: &Value) -> Vec<String> {
    cluster
        .get("nodes")
        .and_then(Value::as_array)
        .map(|nodes| {
            nodes
                .iter()
                .filter(|n| {
                    n.get("status")
                        .and_then(Value::as_str)
                        .is_some_and(|s| s.eq_ignore_ascii_case("alive"))
                        && !n.get("current").and_then(Value::as_bool).unwrap_or(false)
                })
                .filter_map(|n| n.get("address").and_then(Value::as_str))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

/// Names one node advertises but another does not, in both directions.
///
/// This is the check `postvec doctor` cannot do today: its own consistency
/// check unions every node's models before comparing, which hides exactly
/// this — a name missing from one node looks identical to a name present
/// everywhere.
/// `provider_of` labels provider-backed names in the drift lines: a
/// provider model missing from one node is not a missing model *file* but a
/// missing (or broken) providers.d entry/key on that node, and the fix is
/// different enough to be worth naming.
fn inventory_drift(
    nodes: &[(String, Vec<String>)],
    provider_of: &BTreeMap<String, String>,
) -> Vec<String> {
    if nodes.len() < 2 {
        return Vec::new();
    }
    let union: BTreeSet<&str> = nodes
        .iter()
        .flat_map(|(_, models)| models.iter().map(String::as_str))
        .collect();
    let mut drift = Vec::new();
    for name in union {
        let missing: Vec<&str> = nodes
            .iter()
            .filter(|(_, models)| !models.iter().any(|m| m == name))
            .map(|(address, _)| address.as_str())
            .collect();
        if !missing.is_empty() {
            let label = match provider_of.get(name) {
                Some(provider) => format!(
                    " [provider-backed via {provider:?} — check providers.d and its key on \
                     the missing node(s)]"
                ),
                None => String::new(),
            };
            drift.push(format!(
                "{name}: absent from {} of {} node(s) — {}{label}",
                missing.len(),
                nodes.len(),
                missing.join(", ")
            ));
        }
    }
    drift
}

pub async fn status(args: &StatusArgs) -> anyhow::Result<i32> {
    let local = &args.local;
    let client = http_client(local)?;
    let base = admin_base(local);

    let body: Value = match client.get(format!("{base}/config")).send().await {
        Ok(response) => match response.error_for_status() {
            Ok(response) => response.json().await?,
            Err(e) => {
                eprintln!("{}", unreachable_hint(local, &e));
                return Ok(EXIT_UNREACHABLE);
            }
        },
        Err(e) => {
            eprintln!("{}", unreachable_hint(local, &e));
            return Ok(EXIT_UNREACHABLE);
        }
    };
    let report = parse_config(&body);

    // `/ready` answers 503 when it is not ready and the body carries the
    // reason, so the status code must not be allowed to discard it.
    let ready_body: Value = match client.get(format!("{base}/ready")).send().await {
        Ok(response) => response.json().await.unwrap_or(Value::Null),
        Err(_) => Value::Null,
    };
    let ready = ready_body
        .get("data")
        .and_then(|d| d.get("ready"))
        .and_then(Value::as_bool)
        .unwrap_or(false);

    // The on-disk half. The server reports its own root, so an operator does
    // not have to remember which one this node was started with.
    let root = local.root.clone().or_else(|| {
        report
            .server
            .get("root")
            .and_then(Value::as_str)
            .map(std::path::PathBuf::from)
    });
    let inventory = root.as_deref().map(models::inventory);

    let loaded: BTreeSet<&str> = report.models.iter().map(String::as_str).collect();
    let mut warnings: Vec<String> = Vec::new();
    // Policy, not a problem: reported, but it does not make the node degraded.
    let mut policy_note: Option<String> = None;
    let mut on_disk: Vec<(String, bool, bool)> = Vec::new(); // name, enabled, loaded
    match &inventory {
        Some(Ok(inv)) => {
            for warning in &inv.warnings {
                warnings.push(warning.clone());
            }
            for model in &inv.models {
                on_disk.push((
                    model.name.clone(),
                    model.enabled,
                    loaded.contains(model.name.as_str()),
                ));
            }
            // A node started with `--models` excludes everything outside
            // that list on purpose. Reporting those as "not loaded" would
            // turn a deliberate policy into a page of warnings.
            let allow_list: Vec<String> = report
                .server
                .get("models_allowed")
                .and_then(Value::as_array)
                .map(|names| {
                    names
                        .iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default();
            let eligible =
                |name: &str| allow_list.is_empty() || allow_list.iter().any(|a| a == name);

            let pending: Vec<String> = inv
                .loadable()
                .into_iter()
                .filter(|n| !loaded.contains(n.as_str()) && eligible(n))
                .collect();
            if !pending.is_empty() {
                warnings.push(format!(
                    "{} enabled model(s) are on disk but not loaded: {} — run \
                     `postvec-server load {}`",
                    pending.len(),
                    pending.join(", "),
                    pending.join(" ")
                ));
            }
            let excluded = inv.loadable().into_iter().filter(|n| !eligible(n)).count();
            if excluded > 0 {
                policy_note = Some(format!(
                    "{excluded} enabled model(s) on disk are outside this node's --models \
                     allow-list and will not load"
                ));
            }
            for name in &report.models {
                // Provider-backed models have no on-disk descriptor by
                // design: their serving truth is a providers.d file, which
                // survives a restart. The descriptor checks are for engine
                // models only.
                if report.providers.contains_key(name) {
                    continue;
                }
                match inv.get(name) {
                    None => warnings.push(format!(
                        "model {name:?} is loaded but has no descriptor on disk; it will not \
                         come back after a restart"
                    )),
                    Some(model) if !model.enabled => warnings.push(format!(
                        "model {name:?} is loaded but its descriptor is deactivated; it will \
                         not come back after a restart"
                    )),
                    Some(_) => {}
                }
            }
        }
        Some(Err(e)) => warnings.push(format!("cannot read the model root: {e}")),
        None => {}
    }

    // The fleet half.
    let mut fleet: Vec<(String, Vec<String>)> = Vec::new();
    let mut drift: Vec<String> = Vec::new();
    // Set separately from `drift`: a name absent from one node is a report,
    // a name that means two different things is a readiness failure.
    let mut fleet_incompatible = false;
    if args.fleet {
        let own = report
            .server
            .get("frontend")
            .and_then(Value::as_str)
            .unwrap_or("this node")
            .to_string();
        fleet.push((own.clone(), report.models.clone()));
        // Any node's provider marker labels the union entry: a name that is
        // provider-backed anywhere gets the providers.d diagnosis.
        let mut fleet_providers = report.providers.clone();
        let mut fleet_prints = vec![(own, report.fingerprints.clone())];
        for address in peer_addresses(&report.cluster) {
            match client.get(format!("{address}/config")).send().await {
                Ok(response) => match response.json::<Value>().await {
                    Ok(body) => {
                        let peer = parse_config(&body);
                        fleet_providers.extend(peer.providers);
                        fleet_prints.push((address.clone(), peer.fingerprints));
                        fleet.push((address, peer.models));
                    }
                    Err(e) => warnings.push(format!("peer {address} served invalid /config: {e}")),
                },
                Err(e) => warnings.push(format!("peer {address} did not answer /config: {e}")),
            }
        }
        drift = inventory_drift(&fleet, &fleet_providers);
        // Same name, different thing. Reported alongside the absent-from
        // lines because the fix is the same kind of work — make providers.d
        // agree — but treated more harshly in the exit code: a name missing
        // from one node is what a rolling restart looks like, while a name
        // that *means* two things is a fleet that hands callers vectors from
        // two different models under one identity. The first is a report;
        // the second is a readiness failure.
        let incompatible = fingerprint_drift(&fleet_prints);
        fleet_incompatible = !incompatible.is_empty();
        drift.extend(incompatible);
        for line in &drift {
            warnings.push(format!("inventory drift — {line}"));
        }
    }

    if local.json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ready": ready,
                "server": report.server,
                "cluster": report.cluster,
                "loaded": report.models,
                "providers": report.providers,
                "on_disk": on_disk
                    .iter()
                    .map(|(name, enabled, loaded)| json!({
                        "name": name, "enabled": enabled, "loaded": loaded
                    }))
                    .collect::<Vec<_>>(),
                "fleet": fleet
                    .iter()
                    .map(|(address, models)| json!({"address": address, "models": models}))
                    .collect::<Vec<_>>(),
                "drift": drift,
                "warnings": warnings,
                "note": policy_note,
            }))?
        );
    } else {
        print_status(&report, ready, &on_disk, &fleet, &warnings);
        if let Some(note) = &policy_note {
            println!("\nnote\n  {note}");
        }
    }

    Ok(if ready && warnings.is_empty() && !fleet_incompatible {
        EXIT_OK
    } else {
        EXIT_DEGRADED
    })
}

fn print_status(
    report: &NodeReport,
    ready: bool,
    on_disk: &[(String, bool, bool)],
    fleet: &[(String, Vec<String>)],
    warnings: &[String],
) {
    let field = |key: &str| -> String {
        report
            .server
            .get(key)
            .map(|v| match v {
                Value::String(s) => s.clone(),
                other => other.to_string(),
            })
            .unwrap_or_else(|| "?".to_string())
    };
    println!(
        "postvec-server {}  ({})",
        field("version"),
        field("features")
    );
    println!("  ready       {}", if ready { "yes" } else { "NO" });
    println!("  uptime      {}s", field("uptime_seconds"));
    if report.server.get("draining").and_then(Value::as_bool) == Some(true) {
        println!("  draining    yes — finishing in-flight work");
    }
    println!("  frontend    {}", field("frontend"));

    println!("\nmodels");
    let engine_models: Vec<&String> = report
        .models
        .iter()
        .filter(|name| !report.providers.contains_key(*name))
        .collect();
    if on_disk.is_empty() {
        for name in &engine_models {
            println!("  {name}  loaded");
        }
        if engine_models.is_empty() {
            println!("  (none loaded)");
        }
    } else {
        for (name, enabled, loaded) in on_disk {
            println!(
                "  {name:<40} {:<12} {}",
                if *enabled { "enabled" } else { "deactivated" },
                if *loaded { "loaded" } else { "not loaded" }
            );
        }
    }
    if !report.providers.is_empty() {
        println!("\nprovider-backed models (served from providers.d, no on-disk descriptor)");
        for (name, provider) in &report.providers {
            println!("  {name:<40} via {provider}");
        }
    }

    if let Some(nodes) = report.cluster.get("nodes").and_then(Value::as_array) {
        println!("\ncluster ({})", field_of(&report.cluster, "group"));
        if nodes.is_empty() {
            println!("  (gossip reports no members)");
        }
        for node in nodes {
            println!(
                "  {:<34} {:<8} {}{}",
                node.get("address").and_then(Value::as_str).unwrap_or("?"),
                node.get("status").and_then(Value::as_str).unwrap_or("?"),
                node.get("version").and_then(Value::as_str).unwrap_or("?"),
                if node.get("current").and_then(Value::as_bool) == Some(true) {
                    "  (this node)"
                } else {
                    ""
                }
            );
        }
    }

    if fleet.len() > 1 {
        println!("\nfleet inventory");
        for (address, models) in fleet {
            println!("  {:<34} {} model(s)", address, models.len());
        }
    }

    if !warnings.is_empty() {
        println!("\nwarnings");
        for warning in warnings {
            println!("  ! {warning}");
        }
    }
}

fn field_of(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or("?")
        .to_string()
}

// ---- load / unload -----------------------------------------------------

pub async fn load(args: &ModelArgs) -> anyhow::Result<i32> {
    admin_call(args, "load").await
}

pub async fn unload(args: &ModelArgs) -> anyhow::Result<i32> {
    admin_call(args, "unload").await
}

async fn admin_call(args: &ModelArgs, action: &str) -> anyhow::Result<i32> {
    for name in &args.models {
        if let Err(e) = models::validate_model_name(name) {
            eprintln!("{e}");
            return Ok(EXIT_DEGRADED);
        }
    }
    let client = http_client(&args.local)?;
    let url = format!("{}/admin/{action}", admin_base(&args.local));
    let response = match client
        .post(&url)
        .json(&json!({ "models": args.models }))
        .send()
        .await
    {
        Ok(response) => response,
        Err(e) => {
            eprintln!("{}", unreachable_hint(&args.local, &e));
            return Ok(EXIT_UNREACHABLE);
        }
    };

    let status = response.status();
    let body: Value = response.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        eprintln!(
            "{action} refused ({status}): {}",
            body.get("error")
                .and_then(Value::as_str)
                .unwrap_or("no detail")
        );
        return Ok(EXIT_DEGRADED);
    }

    let results = body
        .get("data")
        .and_then(|d| d.get("results"))
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    if args.local.json {
        println!("{}", serde_json::to_string_pretty(&body)?);
    } else {
        for result in &results {
            let name = result.get("model").and_then(Value::as_str).unwrap_or("?");
            let outcome = result.get("status").and_then(Value::as_str).unwrap_or("?");
            match result.get("error").and_then(Value::as_str) {
                Some(error) => println!("{name}: {outcome} — {error}"),
                None => println!("{name}: {outcome}"),
            }
        }
    }

    let failed = results
        .iter()
        .any(|r| r.get("status").and_then(Value::as_str) == Some("error"));
    Ok(if failed { EXIT_DEGRADED } else { EXIT_OK })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn config(models: &[&str]) -> Value {
        json!({
            "success": true,
            "data": {
                "models": models.iter().map(|n| json!({
                    "name": n, "status": "local", "configuration": {"enabled": true}
                })).collect::<Vec<_>>(),
                "server": {"version": "0.1.0", "uptime_seconds": 12, "draining": false},
                "cluster": {"group": "postvec", "nodes": []},
            }
        })
    }

    #[test]
    fn config_parsing_extracts_model_names() {
        let report = parse_config(&config(&["a", "b"]));
        assert_eq!(report.models, ["a", "b"]);
        assert_eq!(report.server["version"], json!("0.1.0"));
    }

    #[test]
    fn a_malformed_config_degrades_instead_of_panicking() {
        let report = parse_config(&json!({}));
        assert!(report.models.is_empty());
        assert_eq!(report.server, Value::Null);
    }

    #[test]
    fn only_alive_peers_that_are_not_this_node_are_queried() {
        let cluster = json!({"nodes": [
            {"address": "https://a:22222", "status": "alive", "current": true},
            {"address": "https://b:22222", "status": "alive", "current": false},
            {"address": "https://c:22222", "status": "dead",  "current": false},
            {"address": "https://d:22222", "status": "suspect", "current": false},
        ]});
        assert_eq!(peer_addresses(&cluster), ["https://b:22222"]);
    }

    /// Set parity answers "is this name everywhere". It cannot answer "is it
    /// the same thing everywhere" — and with round-robin endpoint selection
    /// the second question is the one that decides whether a caller gets a
    /// usable vector. A dimension mismatch surfaces as intermittent dead
    /// letters; a same-dimension model mismatch surfaces as nothing at all.
    #[test]
    fn same_name_different_provider_settings_is_drift() {
        let node = |file: &str, id: &str, dim: i64| {
            let mut prints = BTreeMap::new();
            prints.insert(
                "openai-text-embedding-3-small".to_string(),
                fingerprint(&json!({
                    "name": "openai-text-embedding-3-small",
                    "provider": "openai",
                    "provider_file": file,
                    "provider_model_id": id,
                    "configuration": { "params": { "target_dim": dim } },
                })),
            );
            prints
        };

        // Agreement is silent.
        let agreed = vec![
            (
                "a".to_string(),
                node("openai", "text-embedding-3-small", 1536),
            ),
            (
                "b".to_string(),
                node("openai", "text-embedding-3-small", 1536),
            ),
        ];
        assert!(fingerprint_drift(&agreed).is_empty());

        // A different declared dimension: intermittent dead letters.
        let dims = vec![
            (
                "a".to_string(),
                node("openai", "text-embedding-3-small", 1536),
            ),
            (
                "b".to_string(),
                node("openai", "text-embedding-3-small", 3072),
            ),
        ];
        let drift = fingerprint_drift(&dims);
        assert_eq!(drift.len(), 1, "{drift:?}");
        assert!(
            drift[0].contains("DIFFERENT provider settings"),
            "{drift:?}"
        );
        assert!(
            drift[0].contains("dim 1536") && drift[0].contains("dim 3072"),
            "{drift:?}"
        );

        // The quiet one: same dimension, different model behind the name.
        let ids = vec![
            (
                "a".to_string(),
                node("openai", "text-embedding-3-small", 1536),
            ),
            (
                "b".to_string(),
                node("azure", "text-embedding-ada-002", 1536),
            ),
        ];
        let drift = fingerprint_drift(&ids);
        assert_eq!(drift.len(), 1, "{drift:?}");
        assert!(drift[0].contains("text-embedding-ada-002"), "{drift:?}");

        // One node is not a fleet.
        assert!(fingerprint_drift(&dims[..1]).is_empty());
    }

    /// The check `postvec doctor` cannot make today, because its own
    /// consistency check unions every node's models before comparing.
    #[test]
    fn drift_names_the_nodes_a_model_is_missing_from() {
        let nodes = vec![
            (
                "a".to_string(),
                vec!["embed".to_string(), "conv".to_string()],
            ),
            ("b".to_string(), vec!["embed".to_string()]),
            (
                "c".to_string(),
                vec!["embed".to_string(), "conv".to_string()],
            ),
        ];
        let drift = inventory_drift(&nodes, &BTreeMap::new());
        assert_eq!(drift.len(), 1, "{drift:?}");
        assert!(drift[0].starts_with("conv:"), "{drift:?}");
        assert!(drift[0].contains("absent from 1 of 3"), "{drift:?}");
        assert!(drift[0].contains('b'), "{drift:?}");
        assert!(
            !drift[0].contains("provider-backed"),
            "engine models are not provider-labeled: {drift:?}"
        );
    }

    #[test]
    fn an_agreeing_fleet_has_no_drift() {
        let nodes = vec![
            ("a".to_string(), vec!["embed".to_string()]),
            ("b".to_string(), vec!["embed".to_string()]),
        ];
        assert!(inventory_drift(&nodes, &BTreeMap::new()).is_empty());
    }

    #[test]
    fn a_single_node_is_never_drifted() {
        let nodes = vec![("a".to_string(), vec!["embed".to_string()])];
        assert!(inventory_drift(&nodes, &BTreeMap::new()).is_empty());
    }

    /// A `/config` with a provider entry: the name lands in `models` (so the
    /// existing comparison covers it with no new machinery) AND in the
    /// provider map (so reports can label it).
    #[test]
    fn parse_config_extracts_provider_markers() {
        let body = json!({
            "success": true,
            "data": {
                "models": [
                    { "name": "embed", "status": "local",
                      "configuration": {"enabled": true} },
                    { "name": "openai-text-embedding-3-small", "status": "provider",
                      "provider": "openai",
                      "configuration": {"enabled": true, "params": {"model_type": "embed"}} },
                ],
            }
        });
        let report = parse_config(&body);
        assert_eq!(report.models, ["embed", "openai-text-embedding-3-small"]);
        assert_eq!(
            report.providers.get("openai-text-embedding-3-small"),
            Some(&"openai".to_string())
        );
        assert!(!report.providers.contains_key("embed"));
    }

    /// The two-node parity case the fleet report exists for: one node
    /// missing the provider file shows a drift line labeled with the
    /// provider and the providers.d diagnosis — a missing key is an ops
    /// problem on that node, not a missing model file.
    #[test]
    fn a_node_missing_the_provider_file_shows_a_labeled_diff() {
        let nodes = vec![
            (
                "https://a:22222".to_string(),
                vec![
                    "embed".to_string(),
                    "openai-text-embedding-3-small".to_string(),
                ],
            ),
            ("https://b:22222".to_string(), vec!["embed".to_string()]),
        ];
        let providers: BTreeMap<String, String> = [(
            "openai-text-embedding-3-small".to_string(),
            "openai".to_string(),
        )]
        .into();
        let drift = inventory_drift(&nodes, &providers);
        assert_eq!(drift.len(), 1, "{drift:?}");
        assert!(
            drift[0].contains("openai-text-embedding-3-small"),
            "{drift:?}"
        );
        assert!(drift[0].contains("https://b:22222"), "{drift:?}");
        assert!(
            drift[0].contains("provider-backed via \"openai\""),
            "{drift:?}"
        );
        assert!(drift[0].contains("providers.d"), "{drift:?}");
    }

    #[test]
    fn the_local_client_always_uses_the_loopback_admin_port() {
        let args = LocalArgs {
            admin: Some(9999),
            ..Default::default()
        };
        assert_eq!(admin_base(&args), "http://127.0.0.1:9999");
        assert_eq!(
            admin_base(&LocalArgs::default()),
            format!("http://127.0.0.1:{DEFAULT_ADMIN_PORT}")
        );
    }
}
