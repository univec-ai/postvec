//! postvec-server — the inference node postvec's remote (`grpc`) mode dials.
//!
//! ```text
//!                     postvec (customer's PostgreSQL)
//!                              │
//!            gRPC :33333       │       HTTPS :22222  GET /config
//!            Embed / Convert   │       model discovery
//!                              ▼
//!               ┌──────────────────────────────────────┐
//!               │  postvec-server                      │
//!               │    InferenceEngine over disk models  │
//!               │    gossip :11111   admin :22223 (lo) │
//!               │    no hub, no S3, no UI              │
//!               └──────────────┬───────────────────────┘
//!                              │ gossip, identical ports
//!                     other postvec-server nodes
//! ```
//!
//! A fleet of identical nodes, not a control plane and workers. Every node
//! binds the same three ports and runs the same command; only `--advertise`
//! differs, and usually not even that. Models are files on disk — put there
//! by `postvec model pull`, a shared volume, or any copy step you already
//! have. This process never fetches weights.
//!
//! ## Boot order, and what is fatal
//!
//! Sockets are reserved first, before anything slow, so a port conflict
//! fails in the first second rather than after a multi-minute model load.
//! Then ONNX Runtime initialises (fatal — a node with no backend is not a
//! node), then models load, then warmup, then the cluster, then serving
//! begins. Between reservation and serving the ports are open but silent;
//! a healthcheck sees a refused connection, which is the honest answer for
//! "still starting".
//!
//! Failing to join the cluster is **not** fatal: a node that cannot see its
//! peers still serves every client that can see it.

pub mod admin;
pub mod cli;
pub mod client;
pub mod cluster;
pub mod config;
pub mod engine_host;
pub mod grpc;
pub mod http;
pub mod limits;
pub mod metrics;
pub mod models;
pub mod net;
pub mod state;

pub mod proto {
    tonic::include_proto!("ninference");
}

use crate::cli::{Cli, Command, ServeArgs};
use crate::config::{ProcessEnv, Settings};
use crate::net::AdvertiseSource;
use crate::state::{NodeIdentity, ServerState};
use clap::Parser;
use std::net::SocketAddr;
use std::process::ExitCode;
use std::sync::Arc;

/// Stack size for engine threads. tokio's 2 MiB default is too small:
/// tokenizer regex recursion runs on the worker and blocking threads, and
/// the blocking pool inherits this size. Same value the embedded engine uses.
const THREAD_STACK_SIZE: usize = 8 * 1024 * 1024;
/// Ceiling on the blocking pool. tokio's default is 512, which on a
/// predict-heavy node would mean hundreds of 8 MiB stacks; excess demand
/// should queue behind the admission limit and hit the request deadline
/// instead of fanning out.
const MAX_BLOCKING_THREADS: usize = 64;

pub fn run() -> ExitCode {
    let cli = Cli::parse();
    match cli.command {
        None => run_serve(cli.serve),
        Some(Command::Serve(args)) => run_serve(*args),
        Some(Command::Status(args)) => run_client(|| async move { client::status(&args).await }),
        Some(Command::Load(args)) => run_client(|| async move { client::load(&args).await }),
        Some(Command::Unload(args)) => run_client(|| async move { client::unload(&args).await }),
    }
}

/// The node-local subcommands are short-lived HTTP calls; a single-threaded
/// runtime is the right size for them.
fn run_client<F, Fut>(build: F) -> ExitCode
where
    F: FnOnce() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<i32>>,
{
    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            eprintln!("cannot start the async runtime: {e}");
            return ExitCode::from(2);
        }
    };
    match runtime.block_on(build()) {
        Ok(code) => ExitCode::from(code as u8),
        Err(e) => {
            eprintln!("{e:#}");
            ExitCode::from(client::EXIT_DEGRADED as u8)
        }
    }
}

fn run_serve(args: ServeArgs) -> ExitCode {
    let settings = match config::load(&args, &ProcessEnv) {
        Ok(settings) => Arc::new(settings),
        Err(e) => {
            // The logger is not up yet — configuration errors are the one
            // class that has to be readable without it.
            eprintln!("postvec-server: {e}");
            return ExitCode::FAILURE;
        }
    };

    init_logging(&settings.log_level);

    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .thread_stack_size(THREAD_STACK_SIZE)
        .max_blocking_threads(MAX_BLOCKING_THREADS)
        .thread_name("postvec-server")
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(e) => {
            log::error!("cannot start the async runtime: {e}");
            return ExitCode::FAILURE;
        }
    };

    match runtime.block_on(serve(settings)) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            log::error!("{e}");
            ExitCode::FAILURE
        }
    }
}

fn init_logging(default_filter: &str) {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or(default_filter))
        .format_timestamp_millis()
        .init();
}

/// Reserve a socket before anything slow happens, with an error that names
/// the flag rather than the address.
fn reserve(flag: &str, addr: SocketAddr) -> Result<std::net::TcpListener, String> {
    std::net::TcpListener::bind(addr).map_err(|e| {
        format!(
            "cannot bind {flag} to {addr}: {e}\n\
             (another process may already hold port {}; check with `ss -lntp`)",
            addr.port()
        )
    })
}

async fn serve(settings: Arc<Settings>) -> Result<(), String> {
    log::info!(
        "postvec-server {} ({})",
        env!("CARGO_PKG_VERSION"),
        metrics::features()
    );
    log::info!("engine root: {}", settings.root.display());
    match &settings.config_path {
        Some(path) => log::info!("configuration file: {}", path.display()),
        None => log::info!("configuration file: none found; using flags, environment and defaults"),
    }

    // --- 1. Identity ---------------------------------------------------
    let advertise = net::resolve_advertise(&settings)?;
    // The guess only matters to peers. A single-node deployment — the common
    // container case — would otherwise be warned, on every boot, about a NIC
    // choice that affects nothing it does.
    if advertise.source == AdvertiseSource::Autodetected && !settings.peers.is_empty() {
        log::warn!(
            "advertising {} to the cluster, autodetected from the routing table. On a host \
             with more than one interface (WireGuard, Docker, a second NIC) this is the \
             usual reason a node ends up seeing only itself — pass --advertise <IP> to pin it.",
            advertise.ip
        );
    } else {
        log::info!("advertising {}", advertise.ip);
    }
    let identity = NodeIdentity::new(&settings, advertise);
    log::info!("gRPC address: {}", identity.grpc_address);
    log::info!("HTTP address: {}", identity.frontend);

    // --- 2. Reserve every socket ---------------------------------------
    let grpc_socket = reserve("--grpc", SocketAddr::new(settings.bind, settings.grpc_port))?;
    let http_socket = reserve("--http", SocketAddr::new(settings.bind, settings.http_port))?;
    // The admin socket is loopback whatever --bind says. These routes mutate
    // the engine and have no authentication; a routable bind is not an
    // option the operator gets.
    let admin_addr = SocketAddr::new(std::net::Ipv4Addr::LOCALHOST.into(), settings.admin_port);
    let admin_socket = reserve("--admin", admin_addr)?;
    http::warn_about_exposure(settings.bind, settings.grpc_port, settings.http_port);

    // --- 3. ONNX Runtime -----------------------------------------------
    engine::initialize_onnx(&settings.root).map_err(|e| {
        format!(
            "cannot initialise ONNX Runtime from {}/libs: {e}\n\
             (the shared library is expected at <root>/libs/**/libonnxruntime.so — install \
             the postvec-onnxruntime package or point --root at a tree that has one)",
            settings.root.display()
        )
    })?;

    // --- 4. Models ------------------------------------------------------
    let (engine, _inventory, report) = engine_host::build(&settings).await?;
    if !report.failures.is_empty() {
        log::warn!(
            "{} of {} model(s) did not load; the node is serving the rest",
            report.failures.len(),
            report.requested.len()
        );
    }

    let metrics = Arc::new(metrics::Metrics::new());
    if settings.warmup {
        engine_host::warm_up(&engine, &metrics).await;
    }

    // External providers (docs/external-providers.md §8): one gateway per
    // node, mounted beside the engine. `load` isolates per-provider/per-file
    // failures internally — a broken provider file never degrades local
    // models — and a missing directory is the ordinary zero-config case.
    let local_models = crate::models::reserved_local_names(&settings.root, &engine);
    let gateway = Arc::new(providers::gateway::Gateway::load(
        &settings.providers_path,
        &local_models,
    ));

    // --- 5. Cluster ------------------------------------------------------
    let (seeds, problems) = net::resolve_peers(&settings.peers, settings.gossip_port).await;
    for problem in &problems {
        log::warn!("{problem}");
    }
    let cluster = match cluster::ClusterManager::start(&settings, &identity, seeds).await {
        Ok(manager) => {
            let manager = Arc::new(manager);
            if let Err(e) = manager.join().await {
                log::warn!(
                    "could not join the cluster on startup: {e}. This node serves normally; \
                     the maintenance worker retries every {}s.",
                    cluster::MAINTENANCE_INTERVAL.as_secs()
                );
            }
            Some(manager)
        }
        Err(e) => {
            // Gossip is an observability feature here, not a serving
            // dependency — refusing to start would trade a working node for
            // a missing one.
            log::error!("gossip did not start: {e}. Continuing without cluster membership.");
            None
        }
    };

    let state = ServerState::new(
        engine.clone(),
        settings.clone(),
        identity,
        metrics.clone(),
        cluster.clone(),
        gateway.clone(),
    );

    // --- 6. Serve ---------------------------------------------------------
    let public = http::spawn(state.clone(), http_socket)?;
    let admin = admin::spawn(state.clone(), admin_socket)?;
    // Split the join handles out of the listeners: `select!` consumes them,
    // and the drain below still needs the shutdown handles.
    let public_handle = public.handle.clone();
    let admin_handle = admin.handle.clone();

    let grpc_bound = grpc_socket
        .local_addr()
        .map_err(|e| format!("local_addr on the gRPC listener: {e}"))?;
    let (grpc_shutdown_tx, grpc_shutdown_rx) = tokio::sync::oneshot::channel::<()>();
    let grpc_task = tokio::spawn(grpc::serve(
        engine.clone(),
        metrics.clone(),
        grpc_socket,
        settings.predict_timeout,
        settings.max_inflight,
        gateway,
        async {
            let _ = grpc_shutdown_rx.await;
        },
    ));

    let maintenance = cluster
        .clone()
        .map(|manager| tokio::spawn(cluster::maintenance_worker(manager)));

    log::info!(
        "serving: {} model(s) ready, gRPC on {}, discovery on {}, admin on {}, max-inflight \
         {}, execution ceiling {:?}",
        state.ready_models().len(),
        grpc_bound,
        public.bound,
        admin.bound,
        settings.max_inflight,
        settings.predict_timeout
    );

    // --- 7. Run until told to stop, or until a listener dies -------------
    let (public_task, admin_task) = (public.task, admin.task);
    let stop_reason = tokio::select! {
        signal = wait_for_signal() => signal,
        result = public_task => format!("the discovery listener exited: {result:?}"),
        result = admin_task => format!("the admin listener exited: {result:?}"),
        result = grpc_task => format!("the gRPC listener exited: {result:?}"),
    };
    log::info!("shutting down: {stop_reason}");

    // --- 8. Drain ---------------------------------------------------------
    //
    // `/ready` flips to 503 first so healthchecks and load balancers stop
    // sending work, while `/health` stays 200 and in-flight requests finish.
    //
    // `/config` deliberately keeps advertising this node's models. Emptying
    // it would look tidier, but postvec's discovery prunes its SQL model
    // cache on a complete refresh — so a single-node deployment restarting
    // would have its cache emptied mid-restart, which is a far worse outcome
    // than a few requests routed at a node that is about to close. The
    // mechanism that actually reroutes traffic is the transport error, which
    // postvec already retries.
    state.begin_drain();
    if let Some(maintenance) = maintenance {
        maintenance.abort();
    }
    if let Some(cluster) = &cluster {
        // Announce first, tear down last. The announcement rides the gossip
        // loop's next tick, so it needs the transport to outlive it by more
        // than an instant; the drain window below is that time. Announcing
        // and shutting down together would leave peers to notice through
        // anti-entropy instead, which is thirty seconds of routing at a node
        // that has gone.
        let _ = cluster.announce_departure().await;
    }

    // Keep serving for a moment while `/ready` already answers 503. Without
    // this the listener stops accepting the instant the signal arrives, so
    // the 503 is never observable from outside and a load balancer learns
    // the node is leaving from a refused connection instead of from a health
    // check. A second signal skips the wait, because an operator pressing
    // ctrl-c twice means it.
    if !settings.drain_delay.is_zero() {
        log::info!(
            "draining: /ready is 503 for {:?} before the listeners close",
            settings.drain_delay
        );
        tokio::select! {
            _ = tokio::time::sleep(settings.drain_delay) => {}
            _ = wait_for_signal() => log::info!("second signal; closing now"),
        }
    }

    if let Some(cluster) = &cluster {
        cluster.shutdown().await;
    }
    let _ = grpc_shutdown_tx.send(());
    public_handle.graceful_shutdown(Some(http::GRACEFUL_TIMEOUT));
    admin_handle.graceful_shutdown(Some(http::GRACEFUL_TIMEOUT));
    await_quiet(
        &metrics,
        http::GRACEFUL_TIMEOUT.max(settings.predict_timeout),
    )
    .await;
    log::info!("stopped");
    Ok(())
}

/// Wait for in-flight inference to finish, up to `budget`.
///
/// Polling beats a flat sleep in both directions: an idle node stops
/// immediately instead of padding every restart with the worst case, and a
/// node with a slow request still gets the whole budget.
async fn await_quiet(metrics: &metrics::Metrics, budget: std::time::Duration) {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        let in_flight = metrics.in_flight();
        if in_flight == 0 {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            log::warn!("{in_flight} request(s) still in flight after {budget:?}; exiting anyway");
            return;
        }
        log::info!("waiting for {in_flight} in-flight request(s)");
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

/// SIGTERM (systemd, Docker, Kubernetes) and ctrl-c.
async fn wait_for_signal() -> String {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{signal, SignalKind};
        let mut terminate = match signal(SignalKind::terminate()) {
            Ok(stream) => stream,
            Err(e) => {
                log::warn!("cannot listen for SIGTERM: {e}");
                let _ = tokio::signal::ctrl_c().await;
                return "interrupt".to_string();
            }
        };
        tokio::select! {
            _ = terminate.recv() => "SIGTERM".to_string(),
            _ = tokio::signal::ctrl_c() => "interrupt".to_string(),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        "interrupt".to_string()
    }
}
