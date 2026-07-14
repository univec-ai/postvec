//! The model registry over HTTP: what `postvec model ls`, `ls --available`,
//! `pull`, `activate` and `deactivate` do, on the node, built on the
//! primitives postvec-cli's `registry` module already provides (index
//! fetch, verified download, strict extraction, receipts, the root lock).
//!
//! Reads are on every listener. Mutations are on the loopback admin listener
//! and, with `--manage`, on the public one too, so the dashboard can
//! drive them: nothing here authenticates, like the rest of the node.
//!
//! `pull` installs deactivated, like the CLI: activating is a second step,
//! which also loads the model. A pull runs detached and is followed through
//! `GET /api/registry/pulls`.

use crate::admin::{load_models, model_result, unload_models};
use crate::models::validate_model_name;
use crate::state::ServerState;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use postvec_registry::auth::{self, Credential};
use postvec_registry::client::{DownloadError, Progress, RegistryClient};
use postvec_registry::error::redact;
use postvec_registry::receipt::{timestamp_now, LicenseEvidence};
use postvec_registry::root::{
    disabled_closure_to_enable, enabled_dependants, InstalledModel, ModelRoot,
};
use postvec_registry::{index, stage_from_archive, urls};
use registry_schema::{Index, IndexModel};
use serde::Deserialize;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

const MAX_MODELS_PER_REQUEST: usize = 32;
const MAX_PULL_JOBS: usize = 32;
const REGISTRY_TIMEOUT: Duration = Duration::from_secs(30);

pub struct PullJob {
    pub id: u64,
    models: Vec<String>,
    status: &'static str,
    progress: Arc<Progress>,
    results: Vec<Value>,
    error: Option<String>,
}

impl PullJob {
    fn json(&self) -> Value {
        let (done, total) = self.progress.snapshot();
        json!({
            "id": self.id,
            "models": self.models,
            "status": self.status,
            "downloaded_bytes": done,
            "total_bytes": total,
            "results": self.results,
            "error": self.error,
        })
    }
}

#[derive(Deserialize)]
struct Request {
    models: Vec<String>,
    /// `<license>@<version>` tokens acknowledging notice-policy terms, the
    /// same words as `postvec model pull --accept-license`.
    #[serde(default)]
    accept_license: Vec<String>,
}

type Reply = Result<Value, (StatusCode, String)>;

fn respond(reply: Reply) -> Response {
    match reply {
        Ok(data) => Json(json!({ "success": true, "data": data })).into_response(),
        Err((status, error)) => (
            status,
            Json(json!({ "success": false, "error": redact(&error) })),
        )
            .into_response(),
    }
}

fn bad(e: impl ToString) -> (StatusCode, String) {
    (StatusCode::BAD_REQUEST, e.to_string())
}

fn failed(e: impl ToString) -> (StatusCode, String) {
    (StatusCode::INTERNAL_SERVER_ERROR, e.to_string())
}

fn busy(e: impl ToString) -> (StatusCode, String) {
    (StatusCode::TOO_MANY_REQUESTS, e.to_string())
}

fn validate(request: &Request) -> Result<(), (StatusCode, String)> {
    if request.models.is_empty() || request.models.len() > MAX_MODELS_PER_REQUEST {
        return Err(bad(format!(
            "`models` must name 1 to {MAX_MODELS_PER_REQUEST} models"
        )));
    }
    for name in &request.models {
        validate_model_name(name).map_err(bad)?;
    }
    Ok(())
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> postvec_registry::error::Result<T> + Send + 'static,
) -> Result<T, String> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| format!("task failed: {e}"))?
        .map_err(|e| e.to_string())
}

fn root(state: &ServerState) -> ModelRoot {
    ModelRoot::new(state.settings.root.clone())
}

async fn installed(state: &ServerState) -> Result<Vec<InstalledModel>, String> {
    let root = root(state);
    blocking(move || root.installed()).await
}

/// The credential is the node's own: `POSTVEC_API_KEY` or the service
/// account's `postvec login` store. None is the public channel.
async fn fetch_index() -> Result<(Index, Option<Credential>), String> {
    let credential = blocking(|| auth::resolve(None)).await?;
    let url = if credential.is_some() {
        urls::authenticated_index_url()
    } else {
        urls::public_index_url()
    };
    let client =
        RegistryClient::new(REGISTRY_TIMEOUT, url.overridden).map_err(|e| e.to_string())?;
    let index = client
        .fetch_index(&url.url, credential.as_ref().map(|c| c.key.as_str()))
        .await
        .map_err(|e| e.to_string())?;
    Ok((index, credential))
}

// ---- reads --------------------------------------------------------------

async fn models(State(state): State<Arc<ServerState>>) -> Response {
    respond(
        async {
            let mut rows = Vec::new();
            for m in installed(&state).await.map_err(failed)? {
                rows.push(json!({
                    "name": m.dir_name,
                    "backend": m.backend,
                    "model_type": m.model_type,
                    "target_dim": m.target_dim,
                    "disk_bytes": m.disk_bytes,
                    "enabled": m.enabled,
                    "owner": m.ownership(Duration::from_secs(5)).await.describe(),
                    "revision": m.receipt.as_ref().map(|r| r.revision()),
                    "loaded": state.engine.is_model_ready(&m.dir_name),
                    "receipt_error": m.receipt_error,
                }));
            }
            Ok(json!({ "models": rows }))
        }
        .await,
    )
}

async fn available(State(state): State<Arc<ServerState>>) -> Response {
    respond(
        async {
            let (index, credential) = fetch_index().await.map_err(failed)?;
            let installed = installed(&state).await.map_err(failed)?;
            let rows: Vec<Value> = index
                .models
                .iter()
                .map(|m| {
                    let local = installed.iter().find(|i| i.dir_name == m.name);
                    let revision = local.map(|i| i.receipt.as_ref().map(|r| r.revision()));
                    json!({
                        "name": m.name,
                        "dependencies": m.dependencies,
                        "access": m.access,
                        "model_type": m.model_type,
                        "target_dim": m.target_dim,
                        "download_bytes": m.archive.size,
                        "license": m.license,
                        "license_version": m.license_version,
                        "license_url": m.license_url,
                        "license_acceptance": m.license_acceptance(),
                        "revision": m.revision(),
                        "installed": local.is_some(),
                        "installed_revision": revision.flatten(),
                        "update": match revision {
                            None => "not-installed",
                            Some(None) => "unknown",
                            Some(Some(r)) if r >= m.revision() => "current",
                            Some(Some(_)) => "upgradable",
                        },
                        "withdrawn": m.withdrawn,
                        "summary": m.summary,
                    })
                })
                .collect();
            Ok(json!({
                "channel": index.channel,
                "authenticated": index.authenticated,
                "signed_in_as": credential.map(|c| c.masked()),
                "models": rows,
            }))
        }
        .await,
    )
}

async fn pulls(State(state): State<Arc<ServerState>>) -> Response {
    let jobs: Vec<Value> = state
        .pulls
        .lock()
        .unwrap()
        .iter()
        .map(PullJob::json)
        .collect();
    respond(Ok(json!({ "pulls": jobs })))
}

// ---- pull ---------------------------------------------------------------

fn register_pull(
    jobs: &mut Vec<PullJob>,
    models: Vec<String>,
) -> Result<u64, (StatusCode, String)> {
    while jobs.len() >= MAX_PULL_JOBS {
        match jobs.iter().position(|job| job.status != "running") {
            Some(i) => {
                jobs.remove(i);
            }
            None => {
                return Err(busy(format!(
                    "{MAX_PULL_JOBS} registry pulls are already running; retry after one finishes"
                )));
            }
        }
    }
    let id = jobs.last().map(|job| job.id + 1).unwrap_or(1);
    jobs.push(PullJob {
        id,
        models,
        status: "running",
        progress: Arc::new(Progress::default()),
        results: Vec::new(),
        error: None,
    });
    Ok(id)
}

async fn pull(State(state): State<Arc<ServerState>>, Json(request): Json<Request>) -> Response {
    if let Err(e) = validate(&request) {
        return respond(Err(e));
    }
    let id = {
        let mut jobs = state.pulls.lock().unwrap();
        match register_pull(&mut jobs, request.models.clone()) {
            Ok(id) => id,
            Err(e) => return respond(Err(e)),
        }
    };
    let task_state = state.clone();
    tokio::spawn(async move {
        let outcome = run_pull(&task_state, id, &request).await;
        let mut jobs = task_state.pulls.lock().unwrap();
        let job = jobs
            .iter_mut()
            .find(|j| j.id == id)
            .expect("job registered");
        job.status = if outcome.is_ok() { "done" } else { "failed" };
        job.error = outcome.err().map(|e| redact(&e));
    });
    respond(Ok(json!({ "job": id })))
}

async fn run_pull(state: &Arc<ServerState>, id: u64, request: &Request) -> Result<(), String> {
    let (index, credential) = fetch_index().await?;
    let closure = index::expand_closure(&index, &request.models).map_err(|e| e.to_string())?;
    let root = root(state);
    // The root lock is what serializes this against a `postvec model pull`
    // on the same host; it is held until the job ends.
    let (_lock, installed) = {
        let root = root.clone();
        blocking(move || Ok((root.lock_exclusive()?, root.installed()?))).await?
    };
    let insecure =
        urls::public_index_url().overridden || urls::authenticated_index_url().overridden;
    let client = RegistryClient::new(REGISTRY_TIMEOUT, insecure).map_err(|e| e.to_string())?;
    let progress = state
        .pulls
        .lock()
        .unwrap()
        .iter()
        .find(|j| j.id == id)
        .map(|j| j.progress.clone())
        .expect("job registered");
    for entry in closure {
        let model = entry.model;
        let result = install_one(
            &root,
            &client,
            &progress,
            &installed,
            model,
            request,
            credential.as_ref().map(|value| value.key.as_str()),
        )
        .await;
        let value = match &result {
            Ok(status) => model_result(&model.name, status, None),
            Err(e) => model_result(&model.name, "error", Some(redact(e))),
        };
        if let Some(job) = state.pulls.lock().unwrap().iter_mut().find(|j| j.id == id) {
            job.results.push(value);
        }
        result.map(|_| ())?;
    }
    Ok(())
}

async fn install_one(
    root: &ModelRoot,
    client: &RegistryClient,
    progress: &Progress,
    installed: &[InstalledModel],
    model: &IndexModel,
    request: &Request,
    bearer: Option<&str>,
) -> Result<&'static str, String> {
    let digest = model
        .archive
        .digest_hex()
        .map_err(|e| e.to_string())?
        .to_string();
    if let Some(local) = installed.iter().find(|i| i.dir_name == model.name) {
        if local.backend != model.backend {
            return Err(format!(
                "installed under backend {:?}; the registry serves it for {:?}",
                local.backend, model.backend
            ));
        }
        let Some(receipt) = &local.receipt else {
            return Err(
                "already on disk, not from the registry; remove the directory to reinstall".into(),
            );
        };
        if receipt.archive_digest.trim_start_matches("sha256:") == digest {
            return Ok("already-installed");
        }
        return Err(format!(
            "installed at revision {}, the registry head is {}; upgrade with `postvec model upgrade {}`",
            receipt.revision(),
            model.revision(),
            model.name
        ));
    }
    let evidence = if model.license_acceptance() == "notice" {
        let token = format!(
            "{}@{}",
            model.license.as_deref().unwrap_or(""),
            model.license_version.as_deref().unwrap_or("")
        );
        if !request.accept_license.contains(&token) {
            return Err(format!(
                "its terms ({token}{}) must be acknowledged: resend with accept_license [{token:?}]",
                model.license_url.as_deref().map(|u| format!(", {u}")).unwrap_or_default()
            ));
        }
        Some(LicenseEvidence {
            accepted_at: timestamp_now(),
            method: "flag".into(),
        })
    } else {
        None
    };

    root.ensure_staging().map_err(|e| e.to_string())?;
    let part = root.part_path(&digest);
    progress.add_total(model.archive.size);
    if let Ok(meta) = tokio::fs::metadata(&part).await {
        progress.add_done(meta.len().min(model.archive.size));
    }
    let source_host = match client.download(model, &part, progress).await {
        Ok(host) => host,
        Err(DownloadError::AuthExpired) => {
            let key = bearer.ok_or_else(|| {
                "the registry source requires authentication; sign in and retry".to_string()
            })?;
            let target = urls::authenticated_index_url();
            let fresh_index = client
                .fetch_index(&target.url, Some(key))
                .await
                .map_err(|e| e.to_string())?;
            let fresh = fresh_index.model(&model.name).ok_or_else(|| {
                format!(
                    "{} disappeared from the authenticated catalogue",
                    model.name
                )
            })?;
            if fresh.archive.digest != model.archive.digest {
                return Err(format!(
                    "{} changed revision during the download; retry the pull",
                    model.name
                ));
            }
            client
                .download(fresh, &part, progress)
                .await
                .map_err(|e| match e {
                    DownloadError::AuthExpired => {
                        "authentication kept failing after a fresh registry index".to_string()
                    }
                    DownloadError::Failed(e) => e.to_string(),
                })?
        }
        Err(DownloadError::Failed(e)) => return Err(e.to_string()),
    };

    let (root, model) = (root.clone(), model.clone());
    blocking(move || {
        let staged = stage_from_archive(
            &root,
            &model,
            &part,
            Some(source_host),
            evidence.as_ref(),
            false,
        )?;
        let installed = root.install_staged(&staged.path, &model.backend, &model.name);
        let _ = std::fs::remove_file(&part);
        installed.map(|_| "installed")
    })
    .await
}

// ---- activate / deactivate ---------------------------------------------

fn installed_model<'a>(
    installed: &'a [InstalledModel],
    name: &str,
) -> Result<&'a InstalledModel, String> {
    installed
        .iter()
        .find(|m| m.dir_name == name)
        .ok_or_else(|| format!("{name:?} is not installed; pull it first"))
}

/// Enable on disk first, then load: a model that failed to load is still
/// activated, which is what a restart would honour. Any installed model
/// qualifies, receipt or not.
async fn activate(State(state): State<Arc<ServerState>>, Json(request): Json<Request>) -> Response {
    respond(
        async {
            validate(&request)?;
            let root = root(&state);
            let (_lock, installed) =
                blocking(move || Ok((root.lock_exclusive()?, root.installed()?)))
                    .await
                    .map_err(failed)?;
            let mut names = disabled_closure_to_enable(&installed, &request.models);
            for name in &request.models {
                if !names.contains(name) {
                    names.push(name.clone());
                }
            }
            let (mut results, mut to_load) = (Vec::new(), Vec::new());
            for name in names {
                match installed_model(&installed, &name).and_then(|m| set_enabled(&state, m, true))
                {
                    Ok(()) => to_load.push(name),
                    Err(e) => results.push(model_result(&name, "error", Some(e))),
                }
            }
            if !to_load.is_empty() {
                results.extend(load_models(state.clone(), to_load).await.map_err(failed)?);
            }
            Ok(json!({ "results": results }))
        }
        .await,
    )
}

/// Unload first, then disable: an error means "still on".
async fn deactivate(
    State(state): State<Arc<ServerState>>,
    Json(request): Json<Request>,
) -> Response {
    respond(
        async {
            validate(&request)?;
            let root = root(&state);
            let (_lock, installed) =
                blocking(move || Ok((root.lock_exclusive()?, root.installed()?)))
                    .await
                    .map_err(failed)?;
            let (mut results, mut to_unload) = (Vec::new(), Vec::new());
            for name in &request.models {
                let checked = installed_model(&installed, name).and_then(|_| {
                    match enabled_dependants(&installed, name, &request.models) {
                        d if d.is_empty() => Ok(()),
                        d => Err(format!("still needed by enabled model(s) {}", d.join(", "))),
                    }
                });
                match checked {
                    Ok(()) => to_unload.push(name.clone()),
                    Err(e) => results.push(model_result(name, "error", Some(e))),
                }
            }
            if !to_unload.is_empty() {
                for outcome in unload_models(state.clone(), to_unload)
                    .await
                    .map_err(failed)?
                {
                    let name = outcome["model"].as_str().unwrap_or_default().to_string();
                    if outcome["status"] == "error" {
                        results.push(outcome);
                        continue;
                    }
                    let model = installed
                        .iter()
                        .find(|m| m.dir_name == name)
                        .expect("checked above");
                    results.push(match set_enabled(&state, model, false) {
                        Ok(()) => model_result(&name, "deactivated", None),
                        Err(e) => model_result(&name, "error", Some(e)),
                    });
                }
            }
            Ok(json!({ "results": results }))
        }
        .await,
    )
}

fn set_enabled(state: &ServerState, model: &InstalledModel, enabled: bool) -> Result<(), String> {
    root(state)
        .set_enabled(model, enabled)
        .map(|_| ())
        .map_err(|e| e.to_string())
}

pub fn router(manage: bool) -> Router<Arc<ServerState>> {
    let reads = Router::new()
        .route("/registry/models", get(models))
        .route("/registry/available", get(available))
        .route("/registry/pulls", get(pulls));
    if !manage {
        return reads;
    }
    reads
        .route("/registry/pull", post(pull))
        .route("/registry/activate", post(activate))
        .route("/registry/deactivate", post(deactivate))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pull_history_is_bounded_without_evicting_running_jobs() {
        let mut jobs = Vec::new();
        for i in 0..MAX_PULL_JOBS {
            let id = register_pull(&mut jobs, vec![format!("model-{i}")]).unwrap();
            assert_eq!(id, (i + 1) as u64);
        }
        let error = register_pull(&mut jobs, vec!["overflow".into()]).unwrap_err();
        assert_eq!(error.0, StatusCode::TOO_MANY_REQUESTS);
        assert_eq!(jobs.len(), MAX_PULL_JOBS);

        jobs[0].status = "done";
        assert!(register_pull(&mut jobs, vec!["replacement".into()]).is_ok());
        assert_eq!(jobs.len(), MAX_PULL_JOBS);
    }
}
