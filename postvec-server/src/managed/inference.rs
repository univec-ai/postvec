// SPDX-License-Identifier: BUSL-1.1

use crate::{
    proto::{self, ninference_service_server::NinferenceService},
    state::ServerState,
};
use postvec_core::client::{
    discovery::parse_config, EmbedPurpose, EmbedRoute, ErrorClass, ModelInfo, PvError, RavennaCode,
};
use sqlx::PgConnection;
use std::{
    collections::BTreeMap,
    sync::atomic::{AtomicUsize, Ordering},
    time::Duration,
};
use tonic::{transport::Channel, Request};

pub(super) struct Client {
    local: crate::grpc::InferenceService,
    nodes: Vec<(Option<Channel>, Vec<ModelInfo>)>,
    pub models: Vec<ModelInfo>,
    complete: bool,
    timeout: Duration,
    cursor: AtomicUsize,
}
impl Client {
    pub fn new(state: &ServerState) -> Self {
        Self {
            local: crate::grpc::InferenceService::local(state),
            nodes: Vec::new(),
            models: Vec::new(),
            complete: false,
            timeout: state.settings.predict_timeout,
            cursor: AtomicUsize::new(0),
        }
    }
    pub fn visibility_secs(&self) -> i64 {
        (self.timeout.as_secs() as i64 + 60).max(300)
    }
    pub async fn refresh(&mut self, state: &ServerState) -> anyhow::Result<()> {
        let envelope = crate::http::config_envelope(state).await;
        self.nodes = vec![(None, parse_config(&envelope.to_string())?)];
        self.complete = true;
        let http = reqwest::Client::builder()
            .danger_accept_invalid_certs(true)
            .timeout(Duration::from_secs(5))
            .build()?;
        if let Some(cluster) = &state.cluster {
            for node in cluster.members().await.into_iter().filter(|n| !n.current) {
                if node.status.to_lowercase() != "alive" {
                    self.complete = false;
                    continue;
                }
                let result = async {
                    let response = http
                        .get(format!("{}/config", node.address.trim_end_matches('/')))
                        .send()
                        .await?
                        .error_for_status()?;
                    if response
                        .content_length()
                        .is_some_and(|n| n > 8 * 1024 * 1024)
                    {
                        anyhow::bail!("discovery response too large");
                    }
                    let models = parse_config(&response.text().await?)?;
                    let channel = Channel::from_shared(format!("http://{}", node.grpc))?
                        .connect_timeout(Duration::from_secs(3))
                        .timeout(self.timeout)
                        .connect_lazy();
                    Ok::<_, anyhow::Error>((Some(channel), models))
                }
                .await;
                match result {
                    Ok(node) => self.nodes.push(node),
                    Err(_) => self.complete = false,
                }
            }
        }
        let mut models = BTreeMap::new();
        for (_, rows) in self.nodes.iter().rev() {
            for m in rows {
                models.insert(m.name.clone(), m.clone());
            }
        }
        self.models = models.into_values().collect();
        Ok(())
    }
    pub async fn cache(&self, conn: &mut PgConnection) -> anyhow::Result<()> {
        for m in &self.models {
            sqlx::query("INSERT INTO postvec.models(name,model_type,source_model,target_model,source_dim,target_dim,sequence_len,raw) VALUES($1,$2,$3,$4,$5,$6,$7,$8::jsonb) ON CONFLICT(name) DO UPDATE SET model_type=excluded.model_type,source_model=excluded.source_model,target_model=excluded.target_model,source_dim=excluded.source_dim,target_dim=excluded.target_dim,sequence_len=excluded.sequence_len,raw=excluded.raw,last_seen=now() WHERE (models.model_type,models.source_model,models.target_model,models.source_dim,models.target_dim,models.sequence_len,models.raw) IS DISTINCT FROM (excluded.model_type,excluded.source_model,excluded.target_model,excluded.source_dim,excluded.target_dim,excluded.sequence_len,excluded.raw)")
                .bind(&m.name).bind(&m.model_type).bind(&m.source_model).bind(&m.target_model)
                .bind(m.source_dim.map(|d| d as i32)).bind(m.target_dim.map(|d| d as i32)).bind(m.sequence_len.map(|d| d as i32))
                .bind(m.raw.to_string()).execute(&mut *conn).await?;
        }
        if self.complete {
            let names: Vec<_> = self.models.iter().map(|m| &m.name).collect();
            sqlx::query("DELETE FROM postvec.models WHERE NOT(name=ANY($1::text[]))")
                .bind(names)
                .execute(&mut *conn)
                .await?;
            sqlx::query("UPDATE postvec.worker_heartbeat SET model_refreshes=model_refreshes+1,models_refreshed_at=now() WHERE id=1").execute(conn).await?;
        }
        Ok(())
    }
    pub fn route(
        &self,
        name: &str,
        purpose: EmbedPurpose,
    ) -> Result<(String, EmbedRoute), PvError> {
        let mut direct: Vec<_> = self
            .models
            .iter()
            .filter(|m| {
                m.model_type == "embed"
                    && (m.name == name || m.target_model.as_deref() == Some(name))
            })
            .collect();
        direct.sort_by_key(|m| (m.target_model.as_deref() != Some(name), &m.name));
        if let Some(m) = direct.first() {
            return Ok((m.name.clone(), EmbedRoute::default().with_purpose(purpose)));
        }
        for (_, models) in &self.nodes {
            for c in models.iter().filter(|m| {
                m.model_type == "convert"
                    && m.target_model.as_deref() == Some(name)
                    && m.raw["extra"]["provider"].is_null()
            }) {
                if let Some(source) = &c.source_model {
                    if models.iter().any(|m| {
                        m.model_type == "embed"
                            && m.target_model.as_ref().unwrap_or(&m.name) == source
                            && m.raw["extra"]["provider"].is_null()
                    }) {
                        if let Some(bridge) = models.iter().find(|m| m.model_type == "embed-bridge")
                        {
                            return Ok((
                                bridge.name.clone(),
                                EmbedRoute {
                                    bridge_model: Some(source.clone()),
                                    target_model: Some(name.into()),
                                    purpose,
                                },
                            ));
                        }
                    }
                }
            }
        }
        Err(PvError::UnknownModel(name.into()))
    }
    pub async fn predict(
        &self,
        texts: &[String],
        vectors: Option<&[Vec<f32>]>,
        model: &str,
        route: &EmbedRoute,
    ) -> Result<Vec<Vec<f32>>, PvError> {
        let start = self.cursor.fetch_add(1, Ordering::Relaxed);
        let deadline = tokio::time::Instant::now() + self.timeout;
        let mut error = PvError::UnknownModel(model.into());
        for offset in 0..self.nodes.len() {
            let (channel, models) = &self.nodes[(start + offset) % self.nodes.len()];
            if !models.iter().any(|m| m.name == model) {
                continue;
            }
            if let (Some(via), Some(target)) = (&route.bridge_model, &route.target_model) {
                if !models.iter().any(|m| {
                    m.model_type == "convert"
                        && m.source_model.as_ref() == Some(via)
                        && m.target_model.as_ref() == Some(target)
                        && m.raw["extra"]["provider"].is_null()
                }) {
                    continue;
                }
            }
            let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
            if remaining.is_zero() {
                break;
            }
            let budget = if offset + 1 < self.nodes.len() {
                remaining / 2
            } else {
                remaining
            };
            let result = tokio::time::timeout(budget, async {
                let result = if let Some(vectors) = vectors {
                    let mut req = Request::new(proto::ConvertEmbeddingsRequest {
                        model: model.into(),
                        embeddings: vectors
                            .iter()
                            .map(|v| proto::FloatVector { vector: v.clone() })
                            .collect(),
                        ..Default::default()
                    });
                    req.set_timeout(budget);
                    if let Some(channel) = channel {
                        proto::ninference_service_client::NinferenceServiceClient::new(
                            channel.clone(),
                        )
                        .max_decoding_message_size(64 * 1024 * 1024)
                        .convert_embeddings(req)
                        .await?
                        .into_inner()
                        .embeddings
                    } else {
                        self.local
                            .convert_embeddings(req)
                            .await?
                            .into_inner()
                            .embeddings
                    }
                } else {
                    let mut req = Request::new(proto::EmbedTextsRequest {
                        model: model.into(),
                        texts: texts.to_vec(),
                        bridge_model: route.bridge_model.clone().unwrap_or_default(),
                        target_model: route.target_model.clone().unwrap_or_default(),
                        input_type: route.purpose.as_wire().into(),
                        ..Default::default()
                    });
                    req.set_timeout(budget);
                    if let Some(channel) = channel {
                        proto::ninference_service_client::NinferenceServiceClient::new(
                            channel.clone(),
                        )
                        .max_decoding_message_size(64 * 1024 * 1024)
                        .embed_texts(req)
                        .await?
                        .into_inner()
                        .embeddings
                    } else {
                        self.local.embed_texts(req).await?.into_inner().embeddings
                    }
                };
                Ok::<_, tonic::Status>(result)
            })
            .await;
            match result {
                Ok(Ok(rows)) => return decode(rows),
                Ok(Err(status)) => {
                    error = PvError::Remote {
                        code: status
                            .metadata()
                            .get("x-ravenna-error-code")
                            .and_then(|v| v.to_str().ok())
                            .map(RavennaCode::parse)
                            .unwrap_or(RavennaCode::InternalError),
                        message: status.message().into(),
                    };
                    if matches!(error.class(), ErrorClass::PoisonRow | ErrorClass::Permanent) {
                        return Err(error);
                    }
                }
                Err(_) => {
                    error = PvError::Deadline {
                        ms: self.timeout.as_millis() as u64,
                    }
                }
            }
        }
        Err(error)
    }
}
fn decode(rows: Option<prost_types::ListValue>) -> Result<Vec<Vec<f32>>, PvError> {
    use prost_types::value::Kind;
    rows.ok_or_else(|| PvError::Decode("missing embeddings".into()))?
        .values
        .into_iter()
        .map(|row| {
            let Some(Kind::ListValue(row)) = row.kind else {
                return Err(PvError::Decode("invalid vector".into()));
            };
            row.values
                .into_iter()
                .map(|v| match v.kind {
                    Some(Kind::NumberValue(n)) => Ok(n as f32),
                    _ => Err(PvError::Decode("invalid coordinate".into())),
                })
                .collect()
        })
        .collect()
}
