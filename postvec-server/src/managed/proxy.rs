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
use sqlx::postgres::{PgConnectOptions, PgPool, PgPoolOptions, PgSslMode};
use std::{
    collections::HashMap,
    pin::Pin,
    sync::{
        atomic::{AtomicI64, AtomicU64, AtomicUsize, Ordering::Relaxed},
        Arc,
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
    root_cert: Option<String>,
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
        root_cert: ssl_root_cert(&db.dsn),
        pool: PgPoolOptions::new()
            .max_connections(4)
            .acquire_timeout(Duration::from_secs(10))
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
    let refresh = {
        let proxy = proxy.clone();
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(30)).await;
                discover(&proxy).await;
            }
        })
    };
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
    while !proxy.state.draining() {
        let (tcp, peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(e) => {
                log::warn!("proxy {}: accept: {e}", proxy.name);
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let proxy = proxy.clone();
        tokio::spawn(async move {
            let _ = tcp.set_nodelay(true);
            proxy.stats.connections.fetch_add(1, Relaxed);
            let _count = ConnCount(proxy.stats.clone());
            if let Err(e) = connection(&proxy, Box::new(tcp)).await {
                log::info!("proxy {} {peer}: {e:#}", proxy.name);
            }
        });
    }
    refresh.abort();
    Ok(())
}

async fn connection(proxy: &Proxy, mut client: Stream) -> Result<()> {
    let startup = loop {
        let len = client.read_i32().await? as usize;
        ensure!((8..=10_000).contains(&len), "invalid startup message");
        let mut body = vec![0; len - 4];
        client.read_exact(&mut body).await?;
        match i32::from_be_bytes(body[..4].try_into()?) {
            SSL_REQUEST => match &proxy.acceptor {
                Some(acceptor) => {
                    client.write_all(b"S").await?;
                    let ssl = Ssl::new(acceptor.context())?;
                    let mut tls = tokio_openssl::SslStream::new(ssl, client)?;
                    Pin::new(&mut tls).accept().await.context("client TLS")?;
                    client = Box::new(tls);
                }
                None => client.write_all(b"N").await?,
            },
            GSSENC_REQUEST => client.write_all(b"N").await?,
            CANCEL_REQUEST => {
                let mut upstream = proxy.connect().await?;
                upstream.write_all(&(len as i32).to_be_bytes()).await?;
                upstream.write_all(&body).await?;
                return Ok(());
            }
            PROTOCOL_3 => break body,
            _ => bail!("unsupported protocol"),
        }
    };
    let mut upstream = proxy.connect().await?;
    upstream
        .write_all(&(startup.len() as i32 + 4).to_be_bytes())
        .await?;
    upstream.write_all(&startup).await?;
    let (cr, cw) = tokio::io::split(client);
    let (ur, uw) = tokio::io::split(upstream);
    let shared = Arc::new(Shared::default());
    tokio::select! {
        r = backend_to_client(ur, cw, shared.clone()) => r,
        r = client_to_backend(proxy, cr, uw, shared) => r,
    }
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
        if stream.read_u8().await? != b'S' {
            ensure!(
                matches!(mode, PgSslMode::Prefer | PgSslMode::Allow),
                "upstream refuses TLS"
            );
            return Ok(stream);
        }
        let mut builder = SslConnector::builder(SslMethod::tls_client())?;
        if let Some(path) = &self.root_cert {
            builder.set_ca_file(path)?;
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
    ) -> Result<Vec<Option<Vec<f32>>>> {
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
    /// ParseComplete messages the client did not ask for.
    swallow: AtomicUsize,
}

async fn read_message<R: AsyncRead + Unpin>(r: &mut R) -> Result<Option<(u8, Vec<u8>)>> {
    let kind = match r.read_u8().await {
        Ok(k) => k,
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let len = r.read_i32().await? as usize;
    ensure!((4..=MAX_MESSAGE).contains(&len), "invalid message length");
    let mut body = vec![0; len - 4];
    r.read_exact(&mut body).await?;
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

fn ssl_root_cert(dsn: &str) -> Option<String> {
    if let Ok(url) = reqwest::Url::parse(dsn) {
        if let Some((_, v)) = url
            .query_pairs()
            .find(|(k, _)| k.eq_ignore_ascii_case("sslrootcert"))
        {
            return Some(v.into_owned());
        }
    }
    dsn.split(|c: char| c.is_whitespace() || c == '&')
        .filter_map(|part| part.split_once('='))
        .find(|(k, _)| k.eq_ignore_ascii_case("sslrootcert"))
        .map(|(_, v)| v.trim_matches('\'').to_string())
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
            b'R' if body.len() > 4 && body[..4] == 10i32.to_be_bytes() => {
                body = strip_sasl_plus(&body);
            }
            b'1' if shared.swallow.load(Relaxed) > 0 => {
                shared.swallow.fetch_sub(1, Relaxed);
                continue;
            }
            b'Z' => shared.swallow.store(0, Relaxed),
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
    while let Some((kind, body)) = read_message(&mut r).await? {
        let body = match kind {
            b'Q' => {
                let (sql, _) = cstr(&body)?;
                match rewrite::scan(sql) {
                    Ok(calls) if !calls.is_empty() => {
                        let sql = match proxy.embed_all(&calls, &[]).await {
                            Ok(vectors) => rewrite::render(sql, &calls, &vectors),
                            Err(e) => format!("SELECT postvec._proxy_error({})", quote(&e)),
                        };
                        cstring(&sql)
                    }
                    _ => body,
                }
            }
            b'P' => {
                let (name, rest) = cstr(&body)?;
                let (sql, types) = cstr(rest)?;
                match rewrite::scan(sql) {
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
                let (parse, bind) = match proxy.embed_all(&prepared.calls, &params).await {
                    Ok(vectors) => (
                        parse_body(
                            "",
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
                            parse_body("", "SELECT postvec._proxy_error($1)", &[0, 1, 0, 0, 0, 25]),
                            bind,
                        )
                    }
                };
                w.write_all(&frame(b'P', &parse)).await?;
                shared.swallow.fetch_add(1, Relaxed);
                let mut body = cstring(portal);
                body.push(0);
                body.extend(bind);
                body
            }
            b'C' => {
                if body.first() == Some(&b'S') {
                    statements.remove(cstr(&body[1..])?.0);
                }
                body
            }
            _ => body,
        };
        w.write_all(&frame(kind, &body)).await?;
    }
    Ok(())
}

fn quote(e: &anyhow::Error) -> String {
    postvec_core::registry::quote_literal_estring(&format!("{e:#}"))
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
    let i16_at = |at: usize| -> Result<usize> {
        Ok(i16::from_be_bytes(rest.get(at..at + 2).context("short Bind")?.try_into()?) as usize)
    };
    let mut at = 2 + i16_at(0)? * 2;
    let count = i16_at(at)?;
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
    fn strips_channel_binding_from_sasl_list() {
        let mut body = 10i32.to_be_bytes().to_vec();
        body.extend_from_slice(b"SCRAM-SHA-256-PLUS\0SCRAM-SHA-256\0");
        let out = strip_sasl_plus(&body);
        assert_eq!(&out[..4], &10i32.to_be_bytes());
        assert_eq!(&out[4..], b"SCRAM-SHA-256\0\0");
    }

    #[test]
    fn sslrootcert_from_url_and_keyword_dsn() {
        assert_eq!(
            ssl_root_cert("postgresql://db.example/app?sslmode=verify-full&sslrootcert=/ca.pem")
                .as_deref(),
            Some("/ca.pem")
        );
        assert_eq!(
            ssl_root_cert("host=db.example dbname=app sslrootcert=/ca.pem").as_deref(),
            Some("/ca.pem")
        );
    }
}
