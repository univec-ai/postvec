// SPDX-License-Identifier: BUSL-1.1

//! pgwire proxy: the client authenticates against the upstream database
//! itself; the proxy relays bytes and rewrites `postvec.search()` and
//! `postvec.embed()` calls, embedding their text on the way through.
//! Authentication is relayed. SCRAM-SHA-256-PLUS is never offered: channel
//! binding cannot survive a proxy, so a TLS client of a TLS database must
//! connect with `channel_binding=disable`.

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
    borrow::Cow,
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
/// Rows may reach PostgreSQL's 1 GB allocation limit; authentication
/// messages stay small (PostgreSQL caps its own tokens at 64 KB).
const MAX_ROW: usize = 1 << 30;
const MAX_AUTH: usize = 65536;
const BINDING: &str = "SCRAM channel binding cannot pass through the postvec-server proxy; connect with channel_binding=disable";
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
                    super::install::tune(conn).await?;
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
        let result = async {
            client.refresh(&proxy.state).await?;
            client.constrain(&mut *proxy.pool.acquire().await?).await?;
            Ok::<_, anyhow::Error>(())
        }
        .await;
        if let Err(e) = result {
            log::warn!("proxy {}: model discovery: {e}", proxy.name);
            return;
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
    // Over the cap, as many again may negotiate: to cancel, or to be refused.
    let overflow = Arc::new(tokio::sync::Semaphore::new(db.proxy_max_connections));
    while !proxy.state.draining() {
        let (tcp, peer) = match listener.accept().await {
            Ok(accepted) => accepted,
            Err(e) => {
                log::warn!("proxy {}: accept: {e}", proxy.name);
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let (slot, capped) = match slots.clone().try_acquire_owned() {
            Ok(slot) => (slot, false),
            Err(_) => match overflow.clone().try_acquire_owned() {
                Ok(slot) => (slot, true),
                Err(_) => {
                    log::warn!(
                        "proxy {}: connection cap reached, dropping {peer}",
                        proxy.name
                    );
                    continue;
                }
            },
        };
        let proxy = proxy.clone();
        tokio::spawn(async move {
            let _slot = slot;
            let _ = tcp.set_nodelay(true);
            keepalive(&tcp);
            proxy.stats.connections.fetch_add(1, Relaxed);
            let _count = ConnCount(proxy.stats.clone());
            if let Err(e) = connection(&proxy, Box::new(tcp), capped).await {
                log::info!("proxy {} {peer}: {e:#}", proxy.name);
            }
        });
    }
    Ok(())
}

async fn connection(proxy: &Proxy, client: Stream, capped: bool) -> Result<()> {
    let Some((client, (upstream, upstream_tls))) =
        tokio::time::timeout(Duration::from_secs(10), handshake(proxy, client, capped))
            .await
            .context("proxy handshake timed out")??
    else {
        return Ok(());
    };
    let (cr, mut cw) = tokio::io::split(client);
    let (ur, uw) = tokio::io::split(upstream);
    let shared = Arc::new(Shared {
        upstream_tls,
        ..Default::default()
    });
    let result = tokio::select! {
        r = backend_to_client(ur, &mut cw, shared.clone()) => r,
        r = client_to_backend(proxy, cr, uw, shared) => r,
    };
    if result.as_ref().is_err_and(|e| e.to_string() == BINDING) {
        cw.write_all(&fatal("28000", BINDING)).await?;
    }
    result
}

/// A FATAL ErrorResponse, so clients show why the proxy ended the login.
fn fatal(code: &str, message: &str) -> Vec<u8> {
    let mut body = Vec::new();
    for (field, value) in [
        (b'S', "FATAL"),
        (b'V', "FATAL"),
        (b'C', code),
        (b'M', message),
    ] {
        body.push(field);
        body.extend(cstring(value));
    }
    body.push(0);
    frame(b'E', &body)
}

fn keepalive(tcp: &tokio::net::TcpStream) {
    let _ = socket2::SockRef::from(tcp).set_tcp_keepalive(
        &socket2::TcpKeepalive::new()
            .with_time(Duration::from_secs(30))
            .with_interval(Duration::from_secs(10))
            .with_retries(3),
    );
}

type Upstream = (Stream, bool);
async fn handshake(
    proxy: &Proxy,
    mut client: Stream,
    capped: bool,
) -> Result<Option<(Stream, Upstream)>> {
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
            CANCEL_REQUEST if (16..=268).contains(&len) => {
                let (mut upstream, _) = proxy.connect().await?;
                upstream.write_all(&(len as i32).to_be_bytes()).await?;
                upstream.write_all(&body).await?;
                return Ok(None);
            }
            v if v >> 16 == PROTOCOL_3 >> 16 => {
                ensure!(
                    encrypted || proxy.acceptor.is_none(),
                    "client TLS is required"
                );
                if capped {
                    client
                        .write_all(&fatal("53300", "postvec proxy connection limit reached"))
                        .await?;
                    bail!("connection cap reached");
                }
                let mut rest = &body[4..];
                let (mut user, mut database) = (None, None);
                while rest != b"\0" {
                    let (key, tail) = cstr(rest)?;
                    let (value, tail) = cstr(tail)?;
                    match key.as_ref() {
                        "user" => user = Some(value),
                        "database" => database = Some(value),
                        _ => {}
                    }
                    rest = tail;
                }
                ensure!(
                    database.or(user).as_deref()
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
    let (mut upstream, tls) = proxy.connect().await?;
    upstream
        .write_all(&(startup.len() as i32 + 4).to_be_bytes())
        .await?;
    upstream.write_all(&startup).await?;
    Ok(Some((client, (upstream, tls))))
}

impl Proxy {
    /// The upstream stream, and whether it negotiated TLS.
    async fn connect(&self) -> Result<Upstream> {
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
            None => {
                let tcp = tokio::time::timeout(
                    Duration::from_secs(10),
                    tokio::net::TcpStream::connect((host, port)),
                )
                .await
                .context("upstream connect timed out")?
                .context("upstream connect")?;
                keepalive(&tcp);
                Box::new(tcp)
            }
        };
        let mode = self.upstream.get_ssl_mode();
        if socket.is_some() || matches!(mode, PgSslMode::Disable) {
            return Ok((stream, false));
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
            return Ok((stream, false));
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
        Ok((Box::new(tls), true))
    }

    /// One vector per call, taking `$n` texts from the bind parameters. On
    /// failure, the vectors of the calls before the failing one.
    async fn embed_all(
        &self,
        calls: &[Call],
        params: &[Option<&[u8]>],
        shared: &Shared,
    ) -> Result<Vec<Option<Vec<f32>>>, (Vec<Option<Vec<f32>>>, anyhow::Error)> {
        let mut vectors = Vec::with_capacity(calls.len());
        for call in calls {
            match self.embed_one(call, params, shared).await {
                Ok(vector) => vectors.push(vector),
                Err(e) => return Err((vectors, e)),
            }
        }
        Ok(vectors)
    }

    async fn embed_one(
        &self,
        call: &Call,
        params: &[Option<&[u8]>],
        shared: &Shared,
    ) -> Result<Option<Vec<f32>>> {
        ensure!(
            !shared.non_utf8.load(Relaxed),
            "postvec proxy inference requires client_encoding=UTF8"
        );
        let text = match &call.text {
            Arg::Literal(text) => text.as_str(),
            Arg::Param(n) => {
                let value = (*n as usize)
                    .checked_sub(1)
                    .and_then(|i| params.get(i))
                    .context(
                        "postvec: the query text is a bind parameter; use the extended protocol",
                    )?;
                match value {
                    // embed(NULL, model) is NULL, as in SQL.
                    None if call.embed => return Ok(None),
                    None => bail!("postvec: the query text must not be NULL"),
                    Some(value) => std::str::from_utf8(value)
                        .context("postvec: the query text must be UTF-8 text")?,
                }
            }
        };
        self.permitted(call.embed, shared).await?;
        self.embed(call, text).await.map(Some)
    }

    /// Inference is paid before the database runs the call and checks
    /// EXECUTE under the current role, so a session whose login role cannot
    /// reach EXECUTE through any role it may SET ROLE to is refused first.
    async fn permitted(&self, embed: bool, shared: &Shared) -> Result<()> {
        let flag = &shared.permitted[usize::from(embed)];
        let user = shared.session_user.lock().unwrap().clone();
        let (Some(user), false) = (user, flag.load(Relaxed)) else {
            return Ok(());
        };
        let (name, signature) = if embed {
            ("embed", "postvec.embed(text,text,real[])")
        } else {
            (
                "search",
                "postvec.search(text,text,text,integer,real,integer,integer,jsonb,real[])",
            )
        };
        let allowed: bool = sqlx::query_scalar(
            "SELECT EXISTS(SELECT FROM pg_catalog.pg_roles r
            WHERE pg_has_role($1, r.oid, 'SET') AND has_function_privilege(r.oid, $2, 'EXECUTE'))",
        )
        .bind(user)
        .bind(signature)
        .fetch_one(&self.pool)
        .await
        .context("privilege check")?;
        ensure!(allowed, "permission denied for function {name}");
        flag.store(true, Relaxed);
        Ok(())
    }

    /// Embed one text for a call: `search()` resolves the entry's model,
    /// `embed()` names it directly.
    async fn embed(&self, call: &Call, text: &str) -> Result<Vec<f32>> {
        let (model, space, dim, purpose) = if call.embed {
            (call.relation.clone(), None, None, EmbedPurpose::Document)
        } else {
            let rows: Vec<(String, Option<String>, i32)> = sqlx::query_as(
                "SELECT model, space, dim FROM postvec.registry WHERE source_column=$2 AND state<>'disabled'
                 AND table_name=(parse_ident($1))[cardinality(parse_ident($1))]
                 AND (cardinality(parse_ident($1))=1 OR table_schema=(parse_ident($1))[1])",
            )
            .bind(&call.relation)
            .bind(&call.column)
            .fetch_all(&self.pool)
            .await
            .context("registry lookup")?;
            match rows.as_slice() {
                [(model, space, dim)] => (
                    model.clone(),
                    space.clone(),
                    Some(*dim),
                    EmbedPurpose::Query,
                ),
                [] => bail!("postvec: {}.{} is not enabled", call.relation, call.column),
                _ => bail!(
                    "postvec: relation {} is ambiguous; qualify it with a schema",
                    call.relation
                ),
            }
        };
        let client = self.client.read().await;
        let (name, route) = client.route(&model, space.as_deref(), purpose)?;
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
    upstream_tls: bool,
    authenticated: AtomicBool,
    legacy_strings: AtomicBool,
    non_utf8: AtomicBool,
    /// The session's login role, from ParameterStatus `session_authorization`.
    session_user: Mutex<Option<String>>,
    /// EXECUTE on `search` / `embed`, once confirmed for this session.
    permitted: [AtomicBool; 2],
    replies: Mutex<Replies>,
    /// Signalled whenever a reply settles statement state.
    resolved: tokio::sync::Notify,
}

/// A reply the backend owes, in order: ParseComplete (`1`), CloseComplete
/// (`3`), the ParameterDescription (`t`) of a statement Describe, the
/// PREPARE or DEALLOCATE command tag of SQL naming a statement (`x`), and the
/// Sync (`S`) or Query (`Q`) a ReadyForQuery answers. `statements` names the
/// client statements the reply creates (`Some` rewrite) or removes (`None`),
/// applied only when it arrives: a Parse the server rejects replaces nothing.
/// `extra` is how many proxy parameters a ParameterDescription must hide.
struct Reply {
    kind: u8,
    extra: usize,
    statements: Vec<(String, Option<Prepared>)>,
}

/// The server ignores Syncs sent during COPY FROM STDIN, so those are dropped.
#[derive(Default)]
struct Replies {
    queue: VecDeque<Reply>,
    copy_in: bool,
    statements: HashMap<String, Prepared>,
}

impl Replies {
    fn apply(&mut self, statements: Vec<(String, Option<Prepared>)>) {
        for (name, prepared) in statements {
            match prepared {
                Some(prepared) => self.statements.insert(name, prepared),
                None => self.statements.remove(&name),
            };
        }
    }
}

impl Shared {
    fn sent(
        &self,
        kind: u8,
        extra: usize,
        statements: Vec<(String, Option<Prepared>)>,
        lifecycle: Vec<String>,
    ) {
        let mut r = self.replies.lock().unwrap();
        // The SQL's statements are the server's, never rewritten ones.
        r.queue.extend(lifecycle.into_iter().map(|name| Reply {
            kind: b'x',
            extra: 0,
            statements: vec![(name, None)],
        }));
        let kind = match kind {
            b'P' => b'1',
            b'C' => b'3',
            b'D' => b't',
            b'Q' => b'Q',
            b'S' if !r.copy_in => b'S',
            b'c' | b'f' => return r.copy_in = false,
            _ => return,
        };
        r.queue.push_back(Reply {
            kind,
            extra,
            statements,
        });
    }
    /// Book one backend message; for a ParameterDescription, how many trailing
    /// parameters are the proxy's.
    fn settle(&self, kind: u8) -> usize {
        let extra = {
            let mut r = self.replies.lock().unwrap();
            match kind {
                // Past any lifecycle statement that ended without its tag.
                b'1' | b'3' | b't' => match std::iter::from_fn(|| r.queue.pop_front())
                    .find(|reply| reply.kind != b'x')
                {
                    Some(reply) => {
                        r.apply(reply.statements);
                        reply.extra
                    }
                    None => 0,
                },
                // Replies still queued before the marker were skipped after an
                // error. A failed unnamed Parse still ended the old unnamed
                // statement.
                b'Z' => {
                    r.copy_in = false;
                    while let Some(reply) = r.queue.pop_front() {
                        if matches!(reply.kind, b'S' | b'Q') {
                            r.apply(reply.statements);
                            break;
                        }
                        if reply.kind == b'1' && reply.statements.iter().any(|(n, _)| n.is_empty())
                        {
                            r.apply(vec![(String::new(), None)]);
                        }
                    }
                    0
                }
                b'G' => {
                    r.copy_in = true;
                    r.queue.retain(|reply| reply.kind != b'S');
                    0
                }
                _ => return 0,
            }
        };
        self.resolved.notify_waiters();
        extra
    }
    /// A CommandComplete: SQL PREPARE and DEALLOCATE settle the cycle's next
    /// lifecycle name; their ALL forms drop every statement.
    fn completed(&self, tag: &[u8]) {
        {
            let mut r = self.replies.lock().unwrap();
            match tag {
                b"PREPARE\0" | b"DEALLOCATE\0" => {
                    let at = r
                        .queue
                        .iter()
                        .take_while(|reply| !matches!(reply.kind, b'S' | b'Q'))
                        .position(|reply| reply.kind == b'x');
                    if let Some(reply) = at.and_then(|at| r.queue.remove(at)) {
                        r.apply(reply.statements);
                    }
                }
                b"DEALLOCATE ALL\0" | b"DISCARD ALL\0" => r.statements.clear(),
                _ => return,
            }
        }
        self.resolved.notify_waiters();
    }
    /// The rewrite of statement `name`: a Parse or Close of that name pending
    /// in the current Sync cycle (if it fails, the server skips what follows
    /// too), else the server-confirmed state. `None` while one is pending in
    /// an earlier cycle, whose outcome decides.
    fn prepared(&self, name: &str) -> Option<Option<Prepared>> {
        let r = self.replies.lock().unwrap();
        let mut earlier = false;
        for reply in r.queue.iter().rev() {
            earlier |= matches!(reply.kind, b'S' | b'Q');
            if let Some((_, prepared)) = reply.statements.iter().rev().find(|(n, _)| n == name) {
                return (!earlier).then(|| prepared.clone());
            }
        }
        Some(r.statements.get(name).cloned())
    }
    /// `prepared`, waiting out a pending earlier cycle.
    async fn statement(&self, name: &str) -> Option<Prepared> {
        loop {
            let resolved = self.resolved.notified();
            tokio::pin!(resolved);
            resolved.as_mut().enable();
            match self.prepared(name) {
                Some(known) => return known,
                None => resolved.await,
            }
        }
    }
}

/// One message; `max` is asked once its header has arrived.
async fn read_message<R: AsyncRead + Unpin>(
    r: &mut R,
    max: impl Fn() -> usize,
) -> Result<Option<(u8, Vec<u8>)>> {
    let kind = match r.read_u8().await {
        Ok(k) => k,
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    };
    let len = r.read_i32().await? as usize;
    ensure!((4..=max()).contains(&len), "invalid message length");
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

/// A NUL-terminated string, borrowed when it is valid UTF-8.
fn cstr(body: &[u8]) -> Result<(Cow<'_, str>, &[u8])> {
    let end = body
        .iter()
        .position(|b| *b == 0)
        .context("unterminated string")?;
    Ok((String::from_utf8_lossy(&body[..end]), &body[end + 1..]))
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
    while let Some((kind, mut body)) = read_message(&mut r, || MAX_ROW).await? {
        match kind {
            b'S' => {
                let (name, rest) = cstr(&body)?;
                let (value, _) = cstr(rest)?;
                let value = value.as_ref();
                match name.as_ref() {
                    "standard_conforming_strings" => {
                        shared.legacy_strings.store(value == "off", Relaxed)
                    }
                    "session_authorization" => {
                        *shared.session_user.lock().unwrap() = Some(value.into())
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
            b'1' | b'3' | b'Z' | b'G' => {
                shared.settle(kind);
            }
            b'C' => shared.completed(&body),
            // The server counts the proxy's vector parameters; the client must not.
            b't' => {
                let extra = shared.settle(kind);
                let n = u16::from_be_bytes([body[0], body[1]]) as usize;
                if extra > 0 && n >= extra {
                    body.truncate(2 + 4 * (n - extra));
                    body[..2].copy_from_slice(&((n - extra) as u16).to_be_bytes());
                }
            }
            _ => {}
        }
        w.write_all(&frame(kind, &body)).await?;
    }
    Ok(())
}

/// A client statement the proxy rewrote: the calls to embed, and how many
/// parameters the client binds; each call's vector is one more after those.
/// Or SQL that creates or drops the statements in `lifecycle` when executed.
#[derive(Clone)]
struct Prepared {
    calls: Vec<Call>,
    params: usize,
    lifecycle: Vec<String>,
}

/// Statements the proxy cannot rewrite are forwarded untouched: the managed
/// schema's own `search()` and `embed()` raise the explanatory error. A
/// rewritten Parse takes one extra text parameter per call; each Bind embeds
/// and appends them. The client's statements, portals and their lifetimes
/// stay the server's. Errors run in the database, in order with its replies.
async fn client_to_backend<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    proxy: &Proxy,
    mut r: R,
    mut w: W,
    shared: Arc<Shared>,
) -> Result<()> {
    let limit = || {
        if shared.authenticated.load(Relaxed) {
            MAX_MESSAGE
        } else {
            MAX_AUTH
        }
    };
    // Portals of lifecycle SQL, by name.
    let mut portals: HashMap<String, Vec<String>> = HashMap::new();
    while let Some((kind, body)) = read_message(&mut r, limit).await? {
        if !shared.authenticated.load(Relaxed) {
            ensure!(matches!(kind, b'p' | b'X'), "authentication required");
            // libpq on TLS, seeing no SCRAM-SHA-256-PLUS offered, flags `y`,
            // which a TLS upstream rejects with an opaque negotiation error.
            if shared.upstream_tls
                && body.starts_with(b"SCRAM-SHA-256\0")
                && body.get(18..20) == Some(b"y,")
            {
                bail!(BINDING);
            }
            w.write_all(&frame(kind, &body)).await?;
            continue;
        }
        let standard = !shared.legacy_strings.load(Relaxed);
        let (body, extra, statements, lifecycle) = match kind {
            // A simple query also ends the unnamed statement and portal.
            b'Q' => {
                portals.remove("");
                let lifecycle = rewrite::lifecycle(&cstr(&body)?.0, standard);
                let rewritten = match cstr(&body)?.0 {
                    Cow::Borrowed(sql) => {
                        let calls = rewrite::scan_with_strings(sql, standard);
                        if calls.is_empty() {
                            None
                        } else {
                            let literal = |v: &Vec<Option<Vec<f32>>>| -> Vec<String> {
                                v.iter()
                                    .map(|v| rewrite::literal_vector(v.as_deref()))
                                    .collect()
                            };
                            Some(cstring(
                                &match proxy.embed_all(&calls, &[], &shared).await {
                                    Ok(vectors) => rewrite::render(sql, &calls, &literal(&vectors)),
                                    // Statements before the failing one still run
                                    // (earlier calls with their vectors), so the
                                    // transaction state stays the server's.
                                    Err((vectors, e)) => {
                                        let failed = &calls[vectors.len()..];
                                        let at = rewrite::statement_start(sql, failed, standard);
                                        let done = calls.iter().take_while(|c| c.end <= at).count();
                                        let head = rewrite::render(
                                            &sql[..at],
                                            &calls[..done],
                                            &literal(&vectors),
                                        );
                                        format!("{head}{}", error_sql(&e))
                                    }
                                },
                            ))
                        }
                    }
                    Cow::Owned(_) => None,
                };
                (
                    rewritten.unwrap_or(body),
                    0,
                    vec![(String::new(), None)],
                    lifecycle,
                )
            }
            b'P' => {
                let (name, rest) = cstr(&body)?;
                let (sql, types) = cstr(rest)?;
                let mut calls = match &sql {
                    Cow::Borrowed(sql) => rewrite::scan_with_strings(sql, standard),
                    Cow::Owned(_) => Vec::new(),
                };
                // A `$n` text is embedded as sent only if PostgreSQL reads it as
                // text too: declared text/varchar, or undeclared and bare (then
                // the function's argument makes it text).
                let oids = param_types(types);
                calls.retain(|c| match c.text {
                    Arg::Param(n) => match oids.get(usize::from(n).wrapping_sub(1)) {
                        Some(25 | 1043) => true,
                        Some(0) | None => !c.text_cast,
                        Some(_) => false,
                    },
                    Arg::Literal(_) => true,
                });
                let name = name.into_owned();
                let lifecycle = rewrite::lifecycle(&sql, standard);
                if calls.is_empty() {
                    let prepared = (!lifecycle.is_empty()).then_some(Prepared {
                        calls,
                        params: 0,
                        lifecycle,
                    });
                    (body, 0, vec![(name, prepared)], Vec::new())
                } else {
                    let params = oids.len().max(rewrite::max_param(&sql, standard).into());
                    let vectors: Vec<String> = (1..=calls.len())
                        .map(|i| format!("postvec._proxy_vector(${}::text)", params + i))
                        .collect();
                    let parse = parse_body(&name, &rewrite::render(&sql, &calls, &vectors), types);
                    let prepared = Prepared {
                        calls,
                        params,
                        lifecycle,
                    };
                    (parse, 0, vec![(name, Some(prepared))], Vec::new())
                }
            }
            b'D' if body.first() == Some(&b'S') => {
                let name = cstr(&body[1..])?.0.into_owned();
                let extra = shared.statement(&name).await.map_or(0, |p| p.calls.len());
                (body, extra, Vec::new(), Vec::new())
            }
            // A portal Describe answers with a RowDescription, not tracked.
            b'D' => {
                w.write_all(&frame(kind, &body)).await?;
                continue;
            }
            b'B' => {
                let (portal, rest) = cstr(&body)?;
                let (name, rest) = cstr(rest)?;
                let prepared = shared.statement(&name).await;
                match prepared.as_ref().filter(|p| !p.lifecycle.is_empty()) {
                    Some(p) => portals.insert(portal.to_string(), p.lifecycle.clone()),
                    None => portals.remove(portal.as_ref()),
                };
                let bind = match prepared.filter(|p| !p.calls.is_empty()) {
                    Some(prepared) => rebind(proxy, &shared, &prepared, rest).await?,
                    None => None,
                };
                if let Some(bind) = bind {
                    let mut body = cstring(&portal);
                    body.extend(cstring(&name));
                    body.extend(bind);
                    w.write_all(&frame(kind, &body)).await?;
                    continue;
                }
                (body, 0, Vec::new(), Vec::new())
            }
            b'E' => {
                let lifecycle = portals.get(cstr(&body)?.0.as_ref()).cloned();
                (body, 0, Vec::new(), lifecycle.unwrap_or_default())
            }
            b'C' if body.first() == Some(&b'S') => {
                let name = cstr(&body[1..])?.0.into_owned();
                (body, 0, vec![(name, None)], Vec::new())
            }
            b'C' => {
                portals.remove(cstr(&body[1..])?.0.as_ref());
                (body, 0, Vec::new(), Vec::new())
            }
            _ => (body, 0, Vec::new(), Vec::new()),
        };
        shared.sent(kind, extra, statements, lifecycle);
        w.write_all(&frame(kind, &body)).await?;
    }
    Ok(())
}

/// A Bind of a rewritten statement with the vectors appended as text
/// parameters: an array literal each, or `!message` when inference failed,
/// which `postvec._proxy_vector()` raises. `None` leaves a Bind that does not
/// match the statement to the server, which rejects it.
async fn rebind(
    proxy: &Proxy,
    shared: &Shared,
    prepared: &Prepared,
    rest: &[u8],
) -> Result<Option<Vec<u8>>> {
    let (formats, params, results) = bind_parts(rest)?;
    if params.len() != prepared.params || !(formats.len() <= 1 || formats.len() == params.len()) {
        return Ok(None);
    }
    // Each call's failure is its own: it raises only if the database
    // evaluates that call.
    let mut extra = Vec::with_capacity(prepared.calls.len());
    for call in &prepared.calls {
        extra.push(match proxy.embed_one(call, &params, shared).await {
            Ok(v) => v.map(|v| {
                let v = postvec_core::registry::serialize_vector(&v);
                format!("{{{}}}", &v[1..v.len() - 1])
            }),
            Err(e) => Some(format!("!{e:#}")),
        });
    }
    // Explicit format codes gain a text code per proxy parameter.
    let formats: Vec<u16> = match formats[..] {
        [] | [0] => formats,
        _ => {
            let per_param = if formats.len() == 1 {
                vec![formats[0]; params.len()]
            } else {
                formats
            };
            per_param
                .into_iter()
                .chain(extra.iter().map(|_| 0))
                .collect()
        }
    };
    let mut out = (formats.len() as u16).to_be_bytes().to_vec();
    formats.iter().for_each(|f| out.extend(f.to_be_bytes()));
    out.extend(((params.len() + extra.len()) as u16).to_be_bytes());
    let values = params
        .iter()
        .map(|p| p.map(<[u8]>::to_vec))
        .chain(extra.into_iter().map(|v| v.map(String::into_bytes)));
    for value in values {
        match value {
            Some(v) => {
                out.extend((v.len() as i32).to_be_bytes());
                out.extend(v);
            }
            None => out.extend((-1i32).to_be_bytes()),
        }
    }
    out.extend_from_slice(results);
    Ok(Some(out))
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

/// The parameter type OIDs a Parse declares (0 = left to the server).
fn param_types(types: &[u8]) -> Vec<u32> {
    let n = types
        .get(..2)
        .map_or(0, |b| u16::from_be_bytes([b[0], b[1]]) as usize);
    types
        .get(2..2 + 4 * n)
        .unwrap_or_default()
        .chunks_exact(4)
        .map(|b| u32::from_be_bytes([b[0], b[1], b[2], b[3]]))
        .collect()
}

/// A Bind after its portal and statement names: the parameter format codes,
/// the values (text and binary carry the same bytes for text types), and the
/// result-format tail.
type BindParts<'a> = (Vec<u16>, Vec<Option<&'a [u8]>>, &'a [u8]);
fn bind_parts(rest: &[u8]) -> Result<BindParts<'_>> {
    let u16_at = |at: usize| -> Result<u16> {
        Ok(u16::from_be_bytes(
            rest.get(at..at + 2).context("short Bind")?.try_into()?,
        ))
    };
    let formats: Vec<u16> = (0..u16_at(0)? as usize)
        .map(|i| u16_at(2 + 2 * i))
        .collect::<Result<_>>()?;
    let mut at = 2 + 2 * formats.len();
    let count = u16_at(at)? as usize;
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
    Ok((formats, params, &rest[at..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parameter_descriptions_hide_the_proxy_parameters_in_order() {
        let shared = Shared::default();
        shared.sent(b'P', 0, Vec::new(), Vec::new());
        shared.sent(b'D', 2, Vec::new(), Vec::new());
        shared.sent(b'S', 0, Vec::new(), Vec::new());
        shared.sent(b'D', 0, Vec::new(), Vec::new());
        shared.sent(b'S', 0, Vec::new(), Vec::new());
        assert_eq!(shared.settle(b'1'), 0);
        assert_eq!(shared.settle(b't'), 2);
        shared.settle(b'Z');
        assert_eq!(shared.settle(b't'), 0);
    }

    #[test]
    fn syncs_during_copy_in_are_not_awaited() {
        let shared = Shared::default();
        shared.sent(b'S', 0, Vec::new(), Vec::new());
        shared.settle(b'G');
        shared.sent(b'S', 0, Vec::new(), Vec::new());
        shared.sent(b'c', 0, Vec::new(), Vec::new());
        shared.sent(b'S', 0, Vec::new(), Vec::new());
        shared.settle(b'Z');
        shared.sent(b'D', 1, Vec::new(), Vec::new());
        assert_eq!(
            shared.settle(b't'),
            1,
            "no stale Sync left ahead of the reply"
        );
    }

    #[test]
    fn statements_follow_the_server() {
        let prepared = |params| Prepared {
            calls: Vec::new(),
            params,
            lifecycle: Vec::new(),
        };
        let known =
            |shared: &Shared, name: &str| shared.prepared(name).map(|p| p.map(|p| p.params));
        let shared = Shared::default();
        shared.sent(b'P', 0, vec![("s".into(), Some(prepared(1)))], Vec::new());
        assert_eq!(
            known(&shared, "s"),
            Some(Some(1)),
            "a Bind in the Parse's own cycle"
        );
        shared.sent(b'S', 0, Vec::new(), Vec::new());
        assert_eq!(
            known(&shared, "s"),
            None,
            "a later cycle waits for the Parse's outcome"
        );
        shared.settle(b'1');
        shared.settle(b'Z');
        assert_eq!(known(&shared, "s"), Some(Some(1)));
        shared.sent(b'P', 0, vec![("s".into(), Some(prepared(2)))], Vec::new());
        shared.sent(b'S', 0, Vec::new(), Vec::new());
        shared.settle(b'Z');
        assert_eq!(
            known(&shared, "s"),
            Some(Some(1)),
            "a rejected Parse replaces nothing"
        );
        shared.sent(b'C', 0, vec![("s".into(), None)], Vec::new());
        assert_eq!(known(&shared, "s"), Some(None));
        shared.sent(
            b'P',
            0,
            vec![(String::new(), Some(prepared(0)))],
            Vec::new(),
        );
        shared.sent(b'S', 0, Vec::new(), Vec::new());
        shared.settle(b'1');
        shared.settle(b'Z');
        shared.sent(b'P', 0, vec![(String::new(), None)], Vec::new());
        shared.sent(b'S', 0, Vec::new(), Vec::new());
        shared.settle(b'Z');
        assert_eq!(
            known(&shared, ""),
            Some(None),
            "a failed unnamed Parse ends the old one"
        );
    }

    #[test]
    fn sql_lifecycle_follows_its_command_tags() {
        let shared = Shared::default();
        let rewritten = Prepared {
            calls: Vec::new(),
            params: 0,
            lifecycle: Vec::new(),
        };
        for name in ["a", "b", "c"] {
            shared.sent(
                b'P',
                0,
                vec![(name.into(), Some(rewritten.clone()))],
                Vec::new(),
            );
        }
        shared.sent(b'S', 0, Vec::new(), Vec::new());
        (0..3).for_each(|_| _ = shared.settle(b'1'));
        shared.settle(b'Z');
        let known = |name| shared.prepared(name).map(|p| p.is_some());
        shared.sent(b'Q', 0, Vec::new(), vec!["a".into(), "b".into()]);
        assert_eq!(known("a"), None, "a later cycle waits for the tags");
        shared.completed(b"DEALLOCATE\0");
        shared.settle(b'Z');
        assert_eq!(known("a"), Some(false));
        assert_eq!(known("b"), Some(true), "a statement that did not run");
        shared.completed(b"DISCARD ALL\0");
        assert_eq!(known("c"), Some(false));
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
