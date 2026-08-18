// SPDX-License-Identifier: BUSL-1.1

//! pgwire proxy: the client authenticates against the upstream database
//! itself; the proxy relays bytes and rewrites `postvec.search()` and
//! `postvec.embed()` calls, embedding their text on the way through.
//! Authentication is relayed. SCRAM channel binding cannot survive a proxy,
//! so SCRAM-SHA-256-PLUS is never offered; libpq falls back to SCRAM-SHA-256.

use super::{
    inference::Client,
    install,
    rewrite::{self, Arg, Call},
    ManagedDb,
};
use crate::state::ServerState;
use anyhow::{bail, ensure, Context, Result};
use openssl::ssl::{Ssl, SslAcceptor, SslConnector, SslMethod, SslVerifyMode};
use postvec_core::client::EmbedPurpose;
use serde_json::{json, Value};
use sqlx::{
    postgres::{PgConnectOptions, PgPool, PgPoolOptions, PgSslMode},
    ConnectOptions, Executor,
};
use std::{
    collections::{HashMap, VecDeque},
    pin::Pin,
    sync::{
        atomic::{AtomicBool, AtomicI64, AtomicU64, Ordering::Relaxed},
        Arc, Mutex,
    },
    time::Duration,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

trait Io: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Io for T {}
type Stream = Box<dyn Io>;
const MAX_MESSAGE: usize = 256 * 1024 * 1024;
const SSL_REQUEST: i32 = 80877103;
const GSSENC_REQUEST: i32 = 80877104;
const CANCEL_REQUEST: i32 = 80877102;
const PROTOCOL_3: i32 = 196608;

#[derive(Default)]
pub struct Stats {
    pub port: u16,
    pub connections: AtomicI64,
    pub searches: AtomicU64,
    pub embeds: AtomicU64,
}
impl Stats {
    pub fn json(&self) -> Value {
        json!({"port": self.port, "connections": self.connections.load(Relaxed),
            "rewrites": {"search": self.searches.load(Relaxed), "embed": self.embeds.load(Relaxed)}})
    }
}

struct Proxy {
    state: Arc<ServerState>,
    upstream: PgConnectOptions,
    tls_files: HashMap<String, String>,
    acceptor: Option<SslAcceptor>,
    pool: PgPool,
    client: tokio::sync::RwLock<Client>,
    stats: Arc<Stats>,
    name: String,
}

pub(super) async fn serve(
    state: Arc<ServerState>,
    db: ManagedDb,
    listener: std::net::TcpListener,
    stats: Arc<Stats>,
) -> Result<()> {
    let acceptor = match &state.settings.tls {
        Some(paths) => {
            let mut b = SslAcceptor::mozilla_intermediate_v5(SslMethod::tls_server())?;
            b.set_private_key_file(&paths.key, openssl::ssl::SslFiletype::PEM)?;
            b.set_certificate_chain_file(&paths.cert)?;
            b.check_private_key()?;
            Some(b.build())
        }
        None => None,
    };
    let options = install::options(&db.args())?;
    let proxy = Arc::new(Proxy {
        tls_files: tls_files(&options),
        pool: PgPoolOptions::new()
            .max_connections(4)
            .acquire_timeout(Duration::from_secs(10))
            .after_connect(|conn, _| {
                Box::pin(async move {
                    conn.execute("SET statement_timeout='30s'; SET lock_timeout='5s'")
                        .await?;
                    Ok(())
                })
            })
            .connect_lazy_with(options.clone()),
        upstream: options,
        acceptor,
        client: tokio::sync::RwLock::new(Client::new(&state)),
        stats,
        name: db.name.clone(),
        state,
    });
    async fn discover(proxy: &Proxy) {
        let mut client = Client::new(&proxy.state);
        if let Err(e) = client.refresh(&proxy.state).await {
            log::warn!("proxy {}: model discovery: {e}", proxy.name);
        }
        *proxy.client.write().await = client;
    }
    discover(&proxy).await;
    let _refresh = super::AbortOnDrop({
        let proxy = proxy.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                discover(&proxy).await;
            }
        })
    });
    listener.set_nonblocking(true)?;
    let listener = tokio::net::TcpListener::from_std(listener)?;
    log::info!(
        "proxy {} listening on {} ({})",
        proxy.name,
        listener.local_addr()?,
        if proxy.acceptor.is_some() {
            "tls"
        } else {
            "plain"
        }
    );
    let slots = Arc::new(tokio::sync::Semaphore::new(db.proxy_max_connections));
    while !proxy.state.draining() {
        let (tcp, peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(e) => {
                log::warn!("proxy {}: accept: {e}", proxy.name);
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let Ok(slot) = slots.clone().try_acquire_owned() else {
            log::warn!(
                "proxy {}: connection cap reached, dropping {peer}",
                proxy.name
            );
            continue;
        };
        let proxy = proxy.clone();
        tokio::spawn(async move {
            let _slot = slot;
            let _ = tcp.set_nodelay(true);
            proxy.stats.connections.fetch_add(1, Relaxed);
            let _count = ConnCount(proxy.stats.clone());
            if let Err(e) = connection(&proxy, Box::new(tcp)).await {
                log::info!("proxy {} {peer}: {e:#}", proxy.name);
            }
        });
    }
    Ok(())
}

async fn connection(proxy: &Proxy, client: Stream) -> Result<()> {
    let Some((client, upstream)) =
        tokio::time::timeout(Duration::from_secs(10), handshake(proxy, client))
            .await
            .context("proxy handshake timed out")??
    else {
        return Ok(());
    };
    let (cr, cw) = tokio::io::split(client);
    let (ur, uw) = tokio::io::split(upstream);
    let shared = Arc::new(Shared::default());
    tokio::select! {
        r = backend_to_client(ur, cw, shared.clone()) => r,
        r = client_to_backend(proxy, cr, uw, shared) => r,
    }
}

async fn handshake(proxy: &Proxy, mut client: Stream) -> Result<Option<(Stream, Stream)>> {
    let mut encrypted = false;
    let startup = loop {
        let len = client.read_i32().await? as usize;
        ensure!((8..=10_000).contains(&len), "invalid startup message");
        let mut body = vec![0; len - 4];
        client.read_exact(&mut body).await?;
        match i32::from_be_bytes(body[..4].try_into()?) {
            SSL_REQUEST => match &proxy.acceptor {
                _ if encrypted || len != 8 => bail!("invalid SSL request"),
                Some(acceptor) => {
                    client.write_all(b"S").await?;
                    let ssl = Ssl::new(acceptor.context())?;
                    let mut tls = tokio_openssl::SslStream::new(ssl, client)?;
                    Pin::new(&mut tls).accept().await.context("client TLS")?;
                    client = Box::new(tls);
                    encrypted = true;
                }
                None => client.write_all(b"N").await?,
            },
            GSSENC_REQUEST if !encrypted && len == 8 => client.write_all(b"N").await?,
            CANCEL_REQUEST if len == 16 => {
                let mut upstream = proxy.connect().await?;
                upstream.write_all(&(len as i32).to_be_bytes()).await?;
                upstream.write_all(&body).await?;
                return Ok(None);
            }
            PROTOCOL_3 => {
                ensure!(
                    encrypted || proxy.acceptor.is_none(),
                    "client TLS is required"
                );
                let mut rest = &body[4..];
                let (mut user, mut database) = (None, None);
                while rest != b"\0" {
                    let (key, tail) = cstr(rest)?;
                    let (value, tail) = cstr(tail)?;
                    match key {
                        "user" => user = Some(value),
                        "database" => database = Some(value),
                        _ => {}
                    }
                    rest = tail;
                }
                ensure!(
                    database.or(user)
                        == Some(
                            proxy
                                .upstream
                                .get_database()
                                .unwrap_or(proxy.upstream.get_username())
                        ),
                    "proxy database does not match configured database"
                );
                break body;
            }
            _ => bail!("unsupported protocol"),
        }
    };
    let mut upstream = proxy.connect().await?;
    upstream
        .write_all(&(startup.len() as i32 + 4).to_be_bytes())
        .await?;
    upstream.write_all(&startup).await?;
    Ok(Some((client, upstream)))
}

impl Proxy {
    async fn connect(&self) -> Result<Stream> {
        let (host, port) = (self.upstream.get_host(), self.upstream.get_port());
        let socket = self
            .upstream
            .get_socket()
            .map(|p| p.display().to_string())
            .or_else(|| host.starts_with('/').then(|| host.to_string()));
        let mut stream: Stream = match &socket {
            Some(dir) => Box::new(
                tokio::net::UnixStream::connect(format!("{dir}/.s.PGSQL.{port}"))
                    .await
                    .context("upstream socket")?,
            ),
            None => Box::new(
                tokio::time::timeout(
                    Duration::from_secs(10),
                    tokio::net::TcpStream::connect((host, port)),
                )
                .await
                .context("upstream connect timed out")?
                .context("upstream connect")?,
            ),
        };
        let mode = self.upstream.get_ssl_mode();
        if socket.is_some() || matches!(mode, PgSslMode::Disable) {
            return Ok(stream);
        }
        stream.write_all(&8i32.to_be_bytes()).await?;
        stream.write_all(&SSL_REQUEST.to_be_bytes()).await?;
        let reply = stream.read_u8().await?;
        ensure!(
            matches!(reply, b'S' | b'N'),
            "invalid upstream SSL response"
        );
        if reply == b'N' {
            ensure!(
                matches!(mode, PgSslMode::Prefer | PgSslMode::Allow),
                "upstream refuses TLS"
            );
            return Ok(stream);
        }
        let mut builder = SslConnector::builder(SslMethod::tls_client())?;
        for (key, path) in &self.tls_files {
            match key.as_str() {
                "sslrootcert" => builder.set_ca_file(path)?,
                "sslcert" => builder.set_certificate_chain_file(path)?,
                "sslkey" => builder.set_private_key_file(path, openssl::ssl::SslFiletype::PEM)?,
                _ => {}
            }
        }
        let verify = matches!(mode, PgSslMode::VerifyCa | PgSslMode::VerifyFull);
        builder.set_verify(if verify {
            SslVerifyMode::PEER
        } else {
            SslVerifyMode::NONE
        });
        let mut config = builder.build().configure()?;
        config.set_verify_hostname(matches!(mode, PgSslMode::VerifyFull));
        let mut tls = tokio_openssl::SslStream::new(config.into_ssl(host)?, stream)?;
        Pin::new(&mut tls).connect().await.context("upstream TLS")?;
        Ok(Box::new(tls))
    }

    /// One vector per call, taking `$n` texts from the bind parameters.
    async fn embed_all(
        &self,
        calls: &[Call],
        params: &[Option<&[u8]>],
        shared: &Shared,
    ) -> Result<Vec<Option<Vec<f32>>>> {
        ensure!(
            !shared.non_utf8.load(Relaxed),
            "postvec proxy inference requires client_encoding=UTF8"
        );
        let mut vectors = Vec::with_capacity(calls.len());
        for call in calls {
            let text = match &call.text {
                Arg::Literal(text) => text.as_str(),
                Arg::Param(n) => {
                    let value = (*n as usize)
                        .checked_sub(1)
                        .and_then(|i| params.get(i))
                        .context(
                        "postvec: the query text is a bind parameter; use the extended protocol",
                    )?;
                    std::str::from_utf8(value.context("postvec: the query text must not be NULL")?)
                        .context("postvec: the query text must be UTF-8 text")?
                }
            };
            vectors.push(Some(self.embed(call, text).await?));
        }
        Ok(vectors)
    }

    /// Embed one text for a call: `search()` resolves the entry's model,
    /// `embed()` names it directly.
    async fn embed(&self, call: &Call, text: &str) -> Result<Vec<f32>> {
        let (model, dim, purpose) = if call.embed {
            (call.relation.clone(), None, EmbedPurpose::Document)
        } else {
            let rows: Vec<(String, i32)> = sqlx::query_as(
                "SELECT model, dim FROM postvec.registry WHERE source_column=$2 AND state<>'disabled'
                 AND table_name=(parse_ident($1))[cardinality(parse_ident($1))]
                 AND (cardinality(parse_ident($1))=1 OR table_schema=(parse_ident($1))[1])",
            )
            .bind(&call.relation)
            .bind(&call.column)
            .fetch_all(&self.pool)
            .await
            .context("registry lookup")?;
            match rows.as_slice() {
                [(model, dim)] => (model.clone(), Some(*dim), EmbedPurpose::Query),
                [] => bail!("postvec: {}.{} is not enabled", call.relation, call.column),
                _ => bail!(
                    "postvec: relation {} is ambiguous; qualify it with a schema",
                    call.relation
                ),
            }
        };
        let client = self.client.read().await;
        let (name, route) = client.route(&model, purpose)?;
        let mut rows = client
            .predict(&[text.to_string()], None, &name, &route)
            .await?;
        let vector = rows.pop().context("inference returned no vector")?;
        ensure!(rows.is_empty(), "inference returned extra vectors");
        ensure!(
            dim.is_none_or(|d| d as usize == vector.len()),
            "inference returned {} dimensions, expected {}",
            vector.len(),
            dim.unwrap_or_default()
        );
        ensure!(
            vector.iter().all(|v| v.is_finite()),
            "inference returned a non-finite vector"
        );
        if call.embed {
            self.stats.embeds.fetch_add(1, Relaxed);
        } else {
            self.stats.searches.fetch_add(1, Relaxed);
        }
        Ok(vector)
    }
}

#[derive(Default)]
struct Shared {
    authenticated: AtomicBool,
    legacy_strings: AtomicBool,
    non_utf8: AtomicBool,
    replies: Mutex<VecDeque<Option<(u8, bool)>>>,
}

impl Shared {
    fn sent(&self, kind: u8, internal: bool) {
        let entry = match kind {
            b'P' => Some((b'1', internal)),
            b'C' => Some((b'3', internal)),
            b'S' | b'Q' => None,
            _ => return,
        };
        self.replies.lock().unwrap().push_back(entry);
    }
    fn swallow(&self, kind: u8) -> bool {
        let mut replies = self.replies.lock().unwrap();
        match kind {
            b'1' | b'3' => replies.pop_front() == Some(Some((kind, true))),
            b'Z' => {
                while matches!(replies.pop_front(), Some(Some(_))) {}
                false
            }
            _ => false,
        }
    }
}

async fn read_message<R: AsyncRead + Unpin>(r: &mut R) -> Result<Option<(u8, Vec<u8>)>> {
    let kind = match r.read_u8().await {
        Ok(k) => k,
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let len = r.read_i32().await? as usize;
    ensure!((4..=MAX_MESSAGE).contains(&len), "invalid message length");
    let mut body = Vec::with_capacity((len - 4).min(8192));
    r.take((len - 4) as u64).read_to_end(&mut body).await?;
    ensure!(body.len() == len - 4, "truncated message");
    Ok(Some((kind, body)))
}

fn frame(kind: u8, body: &[u8]) -> Vec<u8> {
    let mut m = Vec::with_capacity(body.len() + 5);
    m.push(kind);
    m.extend((body.len() as i32 + 4).to_be_bytes());
    m.extend_from_slice(body);
    m
}

fn cstr(body: &[u8]) -> Result<(&str, &[u8])> {
    let end = body
        .iter()
        .position(|b| *b == 0)
        .context("unterminated string")?;
    Ok((std::str::from_utf8(&body[..end])?, &body[end + 1..]))
}

fn strip_sasl_plus(body: &[u8]) -> Vec<u8> {
    let mut list = vec![0, 0, 0, 10];
    for mech in body[4..].split(|b| *b == 0).filter(|m| !m.is_empty()) {
        if mech != b"SCRAM-SHA-256-PLUS" {
            list.extend_from_slice(mech);
            list.push(0);
        }
    }
    list.push(0);
    list
}

fn tls_files(options: &PgConnectOptions) -> HashMap<String, String> {
    options
        .to_url_lossy()
        .query_pairs()
        .filter_map(|(key, value)| {
            matches!(key.as_ref(), "sslrootcert" | "sslcert" | "sslkey").then(|| {
                (
                    key.into_owned(),
                    value.strip_prefix("file: ").unwrap_or(&value).to_string(),
                )
            })
        })
        .collect()
}

struct ConnCount(Arc<Stats>);
impl Drop for ConnCount {
    fn drop(&mut self) {
        self.0.connections.fetch_sub(1, Relaxed);
    }
}

async fn backend_to_client<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    mut r: R,
    mut w: W,
    shared: Arc<Shared>,
) -> Result<()> {
    while let Some((kind, mut body)) = read_message(&mut r).await? {
        match kind {
            b'S' => {
                let (name, rest) = cstr(&body)?;
                let (value, _) = cstr(rest)?;
                match name {
                    "standard_conforming_strings" => {
                        shared.legacy_strings.store(value == "off", Relaxed)
                    }
                    "client_encoding" => shared
                        .non_utf8
                        .store(!matches!(value, "UTF8" | "SQL_ASCII"), Relaxed),
                    _ => {}
                }
            }
            b'R' if body == 0i32.to_be_bytes() => shared.authenticated.store(true, Relaxed),
            b'R' if body.len() > 4 && body[..4] == 10i32.to_be_bytes() => {
                body = strip_sasl_plus(&body);
            }
            b'1' | b'3' | b'Z' if shared.swallow(kind) => continue,
            _ => {}
        }
        w.write_all(&frame(kind, &body)).await?;
    }
    Ok(())
}

struct Prepared {
    sql: String,
    calls: Vec<Call>,
    types: Vec<u8>,
}

/// Statements the proxy cannot rewrite are forwarded untouched: the managed
/// schema's own `search()` and `embed()` raise the explanatory error. What
/// the proxy itself cannot do (embed the text) is reported by running
/// `postvec._proxy_error()` upstream, so errors stay in order with the
/// backend's replies and leave the transaction state to the database.
async fn client_to_backend<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    proxy: &Proxy,
    mut r: R,
    mut w: W,
    shared: Arc<Shared>,
) -> Result<()> {
    let mut statements: HashMap<String, Prepared> = HashMap::new();
    let internal = format!("postvec_{}", uuid::Uuid::new_v4().simple());
    let mut close = vec![b'S'];
    close.extend(cstring(&internal));
    while let Some((kind, body)) = read_message(&mut r).await? {
        if !shared.authenticated.load(Relaxed) {
            ensure!(matches!(kind, b'p' | b'X'), "authentication required");
            w.write_all(&frame(kind, &body)).await?;
            continue;
        }
        let body = match kind {
            b'Q' => {
                statements.remove("");
                let (sql, _) = cstr(&body)?;
                match rewrite::scan_with_strings(sql, !shared.legacy_strings.load(Relaxed)) {
                    Ok(calls) if calls.is_empty() => body,
                    Ok(calls) => cstring(&match proxy.embed_all(&calls, &[], &shared).await {
                        Ok(vectors) => rewrite::render(sql, &calls, &vectors),
                        Err(e) => error_sql(&e),
                    }),
                    Err(e) => cstring(&error_sql(&anyhow::Error::msg(e))),
                }
            }
            b'P' => {
                let (name, rest) = cstr(&body)?;
                let (sql, types) = cstr(rest)?;
                match rewrite::scan_with_strings(sql, !shared.legacy_strings.load(Relaxed)) {
                    Ok(calls) if !calls.is_empty() => {
                        let placeholders = vec![None; calls.len()];
                        let rewritten =
                            parse_body(name, &rewrite::render(sql, &calls, &placeholders), types);
                        statements.insert(
                            name.into(),
                            Prepared {
                                sql: sql.into(),
                                calls,
                                types: types.to_vec(),
                            },
                        );
                        rewritten
                    }
                    _ => {
                        statements.remove(name);
                        body
                    }
                }
            }
            b'B' => {
                let (portal, rest) = cstr(&body)?;
                let (name, rest) = cstr(rest)?;
                let Some(prepared) = statements.get(name) else {
                    w.write_all(&frame(kind, &body)).await?;
                    continue;
                };
                let (params, results) = bind_params(rest)?;
                let (parse, bind) = match proxy.embed_all(&prepared.calls, &params, &shared).await {
                    Ok(vectors) => (
                        parse_body(
                            &internal,
                            &rewrite::render(&prepared.sql, &prepared.calls, &vectors),
                            &prepared.types,
                        ),
                        rest.to_vec(),
                    ),
                    Err(e) => {
                        let message = format!("{e:#}");
                        let mut bind = vec![0, 1, 0, 0, 0, 1];
                        bind.extend((message.len() as i32).to_be_bytes());
                        bind.extend_from_slice(message.as_bytes());
                        bind.extend_from_slice(results);
                        (
                            parse_body(
                                &internal,
                                "SELECT postvec._proxy_error($1)",
                                &[0, 1, 0, 0, 0, 25],
                            ),
                            bind,
                        )
                    }
                };
                shared.sent(b'C', true);
                shared.sent(b'P', true);
                w.write_all(&frame(b'C', &close)).await?;
                w.write_all(&frame(b'P', &parse)).await?;
                let mut body = cstring(portal);
                body.extend(cstring(&internal));
                body.extend(bind);
                w.write_all(&frame(b'B', &body)).await?;
                shared.sent(b'C', true);
                w.write_all(&frame(b'C', &close)).await?;
                continue;
            }
            b'C' => {
                if body.first() == Some(&b'S') {
                    statements.remove(cstr(&body[1..])?.0);
                }
                body
            }
            _ => body,
        };
        shared.sent(kind, false);
        w.write_all(&frame(kind, &body)).await?;
    }
    Ok(())
}

fn error_sql(e: &anyhow::Error) -> String {
    format!(
        "SELECT postvec._proxy_error({})",
        postvec_core::registry::quote_literal_estring(&format!("{e:#}"))
    )
}

fn cstring(s: &str) -> Vec<u8> {
    let mut v = s.as_bytes().to_vec();
    v.push(0);
    v
}

fn parse_body(name: &str, sql: &str, types: &[u8]) -> Vec<u8> {
    let mut body = name.as_bytes().to_vec();
    body.push(0);
    body.extend_from_slice(sql.as_bytes());
    body.push(0);
    body.extend_from_slice(types);
    body
}

/// Bind parameter values (text and binary formats carry the same bytes for
/// text types), plus the result-format tail that follows them.
type Params<'a> = Vec<Option<&'a [u8]>>;
fn bind_params(rest: &[u8]) -> Result<(Params<'_>, &[u8])> {
    let u16_at = |at: usize| -> Result<usize> {
        Ok(u16::from_be_bytes(rest.get(at..at + 2).context("short Bind")?.try_into()?) as usize)
    };
    let mut at = 2 + u16_at(0)? * 2;
    let count = u16_at(at)?;
    at += 2;
    let mut params = Vec::with_capacity(count);
    for _ in 0..count {
        let len = i32::from_be_bytes(rest.get(at..at + 4).context("short Bind")?.try_into()?);
        at += 4;
        params.push(if len < 0 {
            None
        } else {
            let v = rest.get(at..at + len as usize).context("short Bind")?;
            at += len as usize;
            Some(v)
        });
    }
    Ok((params, &rest[at..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_replies_remain_ordered_across_pipelines_and_errors() {
        let shared = Shared::default();
        for (kind, internal) in [
            (b'P', false),
            (b'P', true),
            (b'S', false),
            (b'P', false),
            (b'S', false),
        ] {
            shared.sent(kind, internal);
        }
        assert!(!shared.swallow(b'1'));
        assert!(shared.swallow(b'1'));
        assert!(!shared.swallow(b'Z'));
        assert!(!shared.swallow(b'1'));
        assert!(!shared.swallow(b'Z'));
        for kind in [b'P', b'S', b'P', b'S'] {
            shared.sent(kind, true);
        }
        assert!(!shared.swallow(b'E'));
        assert!(!shared.swallow(b'Z'));
        assert!(shared.swallow(b'1'));
    }

    #[test]
    fn strips_channel_binding_from_sasl_list() {
        let mut body = 10i32.to_be_bytes().to_vec();
        body.extend_from_slice(b"SCRAM-SHA-256-PLUS\0SCRAM-SHA-256\0");
        let out = strip_sasl_plus(&body);
        assert_eq!(&out[..4], &10i32.to_be_bytes());
        assert_eq!(&out[4..], b"SCRAM-SHA-256\0\0");
    }

    #[test]
    fn tls_options_use_sqlx_normalization() {
        let options: PgConnectOptions = "postgresql://db.example/app?ssl-ca=/my%20ca.pem&ssl-cert=/client.pem&ssl-key=/client.key".parse().unwrap();
        let pairs = tls_files(&options);
        assert_eq!(pairs["sslrootcert"], "/my ca.pem");
        assert_eq!(pairs["sslcert"], "/client.pem");
        assert_eq!(pairs["sslkey"], "/client.key");
    }
}
