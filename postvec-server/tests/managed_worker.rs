// SPDX-License-Identifier: BUSL-1.1

use anyhow::{ensure, Context, Result};
use axum::{http::StatusCode, routing::post, Json, Router};
use postvec_server::{
    cli::ServeArgs,
    config,
    managed::{self, Command, ConnectionArgs, ManagedDb},
    metrics::Metrics,
    state::{NodeIdentity, ServerState},
};
use serde_json::{json, Value};
use sqlx::{Connection, Executor, PgConnection, Row};
use std::{
    os::unix::fs::PermissionsExt,
    path::PathBuf,
    sync::{
        atomic::{AtomicBool, Ordering},
        Arc,
    },
    time::Duration,
};

struct Tasks(Vec<tokio::task::JoinHandle<()>>);
impl Drop for Tasks {
    fn drop(&mut self) {
        for t in &self.0 {
            t.abort();
        }
    }
}
async fn wait(db: &mut PgConnection, sql: &str) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(40), async {
        loop {
            if sqlx::query_scalar::<_, bool>(&format!("SELECT coalesce(({sql}),false)"))
                .fetch_one(&mut *db)
                .await?
            {
                return Ok::<_, anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .with_context(|| format!("timed out: {sql}"))??;
    Ok(())
}

#[tokio::test]
#[ignore = "POSTVEC_MANAGED_TEST_DSN must name a disposable PostgreSQL 18 admin database"]
async fn worker_fleet_lifecycle() -> Result<()> {
    let dsn = std::env::var("POSTVEC_MANAGED_TEST_DSN")?;
    let name = format!("worker_{}", uuid::Uuid::new_v4().simple());
    let mut admin = PgConnection::connect(&dsn).await?;
    admin
        .execute(format!("CREATE DATABASE {name}").as_str())
        .await?;
    let mut url = reqwest::Url::parse(&dsn)?;
    url.set_path(&format!("/{name}"));
    let result = exercise(url.as_str()).await;
    admin
        .execute(format!("DROP DATABASE {name} WITH(FORCE)").as_str())
        .await?;
    result
}
async fn exercise(dsn: &str) -> Result<()> {
    let _ = env_logger::builder().is_test(true).try_init();
    let mut db = PgConnection::connect(dsn).await?;
    db.execute("CREATE SCHEMA vectors;CREATE EXTENSION vector SCHEMA vectors;GRANT USAGE ON SCHEMA vectors TO PUBLIC").await?;
    managed::run(Command::Install(ConnectionArgs {
        dsn: dsn.into(),
        password_file: None,
        timeout: 10,
    }))
    .await?;
    let paused = Arc::new(AtomicBool::new(false));
    let entered = Arc::new(AtomicBool::new(false));
    let p = paused.clone();
    let e = entered.clone();
    let mock=Router::new().route("/v1/embeddings",post(move|Json(v):Json<Value>|{
        let paused=p.clone();let entered=e.clone();async move {
            entered.store(true,Ordering::SeqCst);
            while paused.load(Ordering::SeqCst){tokio::time::sleep(Duration::from_millis(20)).await;}
            let texts=v["input"].as_array().cloned().unwrap_or_default();
            if texts.iter().any(|v|v.as_str()==Some("poison")){return (StatusCode::BAD_REQUEST,Json(json!({"error":{"message":"maximum context length exceeded: too many tokens"}})));}
            (StatusCode::OK,Json(json!({"data":texts.iter().enumerate().map(|(i,t)|json!({"index":i,"embedding":[t.as_str().unwrap().len() as f32,1.0,2.0]})).collect::<Vec<_>>()})))
        }
    }));
    let mock = mock.route("/v1/convert",post(|Json(v):Json<Value>|async move {
        Json(json!({"success":true,"data":{"embeddings":v["embeddings"].as_array().unwrap().iter().map(|r|r.as_array().unwrap().iter().map(|n|n.as_f64().unwrap()+10.0).collect::<Vec<_>>()).collect::<Vec<_>>()}}))
    }));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let mock_task = tokio::spawn(async move {
        axum::serve(listener, mock).await.unwrap();
    });
    let root = tempfile::tempdir()?;
    std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o755))?;
    let providers = root.path().join("providers.d");
    std::fs::create_dir(&providers)?;
    std::fs::set_permissions(&providers, std::fs::Permissions::from_mode(0o700))?;
    let file = providers.join("mock.toml");
    std::fs::write(&file,format!("provider=\"openai\"\napi_key=\"fixture\"\nbase_url=\"http://{addr}\"\n[[models]]\nname=\"fixture\"\nprovider_model_id=\"fixture\"\ndim=3\n[[models]]\nname=\"next\"\nprovider_model_id=\"next\"\ndim=3\n"))?;
    std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o600))?;
    let file = providers.join("univec.toml");
    std::fs::write(&file,format!("provider=\"univec\"\napi_key=\"uv-test\"\nbase_url=\"http://{addr}\"\n[[models]]\nname=\"converter\"\nkind=\"convert\"\nprovider_model_id=\"next\"\nprovider_source_id=\"origin\"\nsource_model=\"origin\"\ntarget_model=\"next\"\nsource_dim=3\ndim=3\n"))?;
    std::fs::set_permissions(file, std::fs::Permissions::from_mode(0o600))?;
    let gateway = Arc::new(providers::gateway::Gateway::load(
        &providers,
        &Default::default(),
    ));
    ensure!(!gateway.is_empty(), "mock provider did not load");
    let make = |port, dsn: String, poll_only, proxy_port, tls: Option<(PathBuf, PathBuf)>| {
        let settings = Arc::new(
            config::resolve(
                &ServeArgs {
                    insecure: tls.is_none(),
                    ssl_cert: tls.as_ref().map(|t| t.0.clone()),
                    ssl_cert_key: tls.as_ref().map(|t| t.1.clone()),
                    ..Default::default()
                },
                &config::FileConfig {
                    managed: Some(vec![ManagedDb {
                        name: "test".into(),
                        dsn,
                        poll_interval_ms: 100,
                        batch_size: 8,
                        poll_only,
                        proxy_port,
                        sync: proxy_port.is_none(),
                        ..Default::default()
                    }]),
                    ..Default::default()
                },
                &std::collections::BTreeMap::<String, String>::new(),
                root.path().into(),
                None,
            )
            .unwrap(),
        );
        ServerState::new(
            Arc::new(engine::InferenceEngine::new(Arc::new(
                engine::EngineConfig {
                    root_path: root.path().into(),
                    host_policy: Default::default(),
                },
            ))),
            settings,
            NodeIdentity {
                advertise: "127.0.0.1".parse().unwrap(),
                api_address: format!("http://127.0.0.1:{port}"),
                grpc_address: format!("127.0.0.1:{port}"),
                frontend: String::new(),
            },
            Arc::new(Metrics::new()),
            None,
            gateway.clone(),
        )
    };
    let mut standby = reqwest::Url::parse(dsn)?;
    standby
        .query_pairs_mut()
        .append_pair("application_name", "standby");
    let first = make(31101, dsn.into(), false, None, None);
    let second = make(31102, standby.into(), true, None, None);
    let mut tasks = Tasks(vec![mock_task]);
    tasks.0.extend(managed::start(&first, Vec::new()));
    tasks.0.extend(managed::start(&second, Vec::new()));
    wait(&mut db, "SELECT count(*)=3 FROM postvec.models").await?;
    db.execute("CREATE TABLE docs(id int PRIMARY KEY,body text,title text);INSERT INTO docs VALUES(1,'hello','one'),(2,NULL,'two');SELECT postvec.enable('docs','body','fixture',backfill_mode=>'cursor');").await?;
    wait(
        &mut db,
        "SELECT body_semantic IS NOT NULL FROM docs WHERE id=1",
    )
    .await?;
    ensure!(
        sqlx::query_scalar::<_, String>("SELECT body_semantic::text FROM docs WHERE id=1")
            .fetch_one(&mut db)
            .await?
            == "[5,1,2]"
    );
    db.execute("INSERT INTO postvec.jobs(registry_id,pk_value) VALUES(1,'invalid-integer')")
        .await?;
    wait(
        &mut db,
        "SELECT NOT EXISTS(SELECT FROM postvec.jobs WHERE pk_value='invalid-integer')",
    )
    .await?;
    paused.store(true, Ordering::SeqCst);
    entered.store(false, Ordering::SeqCst);
    db.execute("UPDATE docs SET body='old' WHERE id=1").await?;
    tokio::time::timeout(Duration::from_secs(10), async {
        while !entered.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?;
    db.execute("UPDATE docs SET body='new value' WHERE id=1")
        .await?;
    paused.store(false, Ordering::SeqCst);
    wait(
        &mut db,
        "SELECT body_semantic::text='[9,1,2]' FROM docs WHERE id=1",
    )
    .await?;
    db.execute("INSERT INTO docs VALUES(3,'poison','bad'),(4,'good','ok')")
        .await?;
    wait(&mut db,"SELECT EXISTS(SELECT FROM postvec.jobs_dead) AND (SELECT body_semantic IS NOT NULL FROM docs WHERE id=4)").await?;
    db.execute("UPDATE docs SET body='fixed' WHERE id=3;UPDATE docs SET body=NULL WHERE id=1")
        .await?;
    wait(&mut db,"SELECT (SELECT body_semantic IS NULL FROM docs WHERE id=1) AND NOT EXISTS(SELECT FROM postvec.jobs_dead) AND NOT EXISTS(SELECT FROM postvec.jobs)").await?;
    db.execute("SELECT postvec.set_format('docs','body','$title: $body')")
        .await?;
    wait(
        &mut db,
        "SELECT body_semantic::text='[10,1,2]' FROM docs WHERE id=3",
    )
    .await?;
    db.execute("CREATE TABLE chunks(id int PRIMARY KEY,body text);INSERT INTO chunks VALUES(1,repeat('chunk text ',30));SELECT postvec.enable('chunks','body','fixture',chunking=>'recursive',chunk_size=>64,chunk_overlap=>8)").await?;
    wait(
        &mut db,
        "SELECT count(*)>1 AND bool_and(body_semantic IS NOT NULL) FROM chunks_body_chunks",
    )
    .await?;
    db.execute("UPDATE chunks SET body='replacement' WHERE id=1")
        .await?;
    wait(&mut db,"SELECT count(*)=1 AND bool_and(chunk_text='replacement') AND bool_and(body_semantic IS NOT NULL) FROM chunks_body_chunks").await?;
    db.execute("SELECT postvec.migrate('docs','body','next',strategy=>'reembed')")
        .await?;
    wait(
        &mut db,
        "SELECT state='awaiting_finalize' FROM postvec.migrations WHERE registry_id=1",
    )
    .await?;
    db.execute("ALTER TABLE docs ALTER COLUMN body_semantic SET STATISTICS 100")
        .await?;
    ensure!(
        db.execute("SELECT postvec.migration_finalize(1)")
            .await
            .is_err(),
        "cutover discarded custom statistics"
    );
    db.execute("ALTER TABLE docs ALTER COLUMN body_semantic SET STATISTICS -1;SELECT postvec.migration_finalize(1)").await?;
    ensure!(
        sqlx::query_scalar::<_, String>("SELECT model FROM postvec.registry WHERE id=1")
            .fetch_one(&mut db)
            .await?
            == "next"
    );
    db.execute("UPDATE postvec.registry SET index_mode='auto' WHERE id=1")
        .await?;
    wait(&mut db,"SELECT EXISTS(SELECT FROM pg_index WHERE indrelid='docs'::regclass AND indexrelid<>'docs_pkey'::regclass AND indisvalid)").await?;
    db.execute("CREATE TABLE converted(id uuid PRIMARY KEY,body text,v vectors.vector(3));INSERT INTO converted VALUES('00000000-0000-0000-0000-000000000001','no embedding required','[1,2,3]');SELECT postvec.adopt('converted','body','v','origin',sync=>false,backfill=>'none');SELECT postvec.migrate('converted','body','next',observed_writes_quiesced=>true)").await?;
    wait(
        &mut db,
        "SELECT state='awaiting_finalize' FROM postvec.migrations WHERE id=2",
    )
    .await?;
    db.execute("SELECT postvec.migration_finalize(2)").await?;
    ensure!(
        sqlx::query_scalar::<_, String>("SELECT v::text FROM converted")
            .fetch_one(&mut db)
            .await?
            == "[11,12,13]",
        "conversion output"
    );
    db.execute("SELECT postvec.migrate('docs','body','fixture',strategy=>'reembed')")
        .await?;
    wait(
        &mut db,
        "SELECT state='awaiting_finalize' FROM postvec.migrations WHERE id=3",
    )
    .await?;
    db.execute("SELECT postvec.migration_finalize(3)").await?;
    ensure!(
        sqlx::query_scalar::<_, String>("SELECT state FROM postvec.migrations WHERE id=3")
            .fetch_one(&mut db)
            .await?
            == "awaiting_index",
        "cutover with an indexed old column must await the rebuilt index"
    );
    ensure!(
        db.execute("SELECT postvec.migration_abort(3)")
            .await
            .is_err(),
        "abort accepted after the column swap"
    );
    wait(
        &mut db,
        "SELECT state='done' FROM postvec.migrations WHERE id=3",
    )
    .await?;
    db.execute("CREATE TABLE composite(a text,b int,body text,PRIMARY KEY(a,b));INSERT INTO composite VALUES(E'back\\\\slash''quote',1,'composite');SELECT postvec.enable('composite','body','fixture',backfill_mode=>'cursor')").await?;
    wait(
        &mut db,
        "SELECT body_semantic::text='[9,1,2]' FROM composite",
    )
    .await?;
    db.execute("CREATE TABLE refill(id int PRIMARY KEY,body text,v vectors.vector(3));INSERT INTO refill VALUES(1,'refilled','[99,99,99]');SELECT postvec.adopt('refill','body','v','fixture',backfill=>'all',backfill_mode=>'cursor')").await?;
    wait(&mut db, "SELECT v::text='[8,1,2]' FROM refill").await?;
    wait(
        &mut db,
        "SELECT NOT EXISTS(SELECT FROM postvec.settings WHERE key LIKE 'backfill_all:%')",
    )
    .await?;
    paused.store(true, Ordering::SeqCst);
    entered.store(false, Ordering::SeqCst);
    db.execute("UPDATE docs SET body='in flight at failover' WHERE id=4")
        .await?;
    tokio::time::timeout(Duration::from_secs(10), async {
        while !entered.load(Ordering::SeqCst) {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?;
    let pid: i32 = sqlx::query_scalar("SELECT pid FROM postvec.worker_heartbeat")
        .fetch_one(&mut db)
        .await?;
    sqlx::query("SELECT pg_terminate_backend($1)")
        .bind(pid)
        .execute(&mut db)
        .await?;
    wait(&mut db,&format!("SELECT pid<>{pid} AND last_beat>now()-interval '10 seconds' FROM postvec.worker_heartbeat")).await?;
    paused.store(false, Ordering::SeqCst);
    wait(
        &mut db,
        "SELECT body_semantic::text='[25,1,2]' FROM docs WHERE id=4",
    )
    .await?;
    let leaders:i64=sqlx::query_scalar("SELECT count(*) FROM pg_locks WHERE locktype='advisory' AND classid=1886615158 AND objid=2 AND objsubid=2 AND granted AND database=(SELECT oid FROM pg_database WHERE datname=current_database())").fetch_one(&mut db).await?;
    ensure!(leaders == 1, "multiple leaders");
    db.execute("INSERT INTO docs VALUES(5,'after failover','node')")
        .await?;
    wait(
        &mut db,
        "SELECT body_semantic IS NOT NULL FROM docs WHERE id=5",
    )
    .await?;
    db.execute(
        "UPDATE postvec.schema_version SET version=2;INSERT INTO docs VALUES(6,'parked','version')",
    )
    .await?;
    tokio::time::sleep(Duration::from_secs(2)).await;
    ensure!(
        sqlx::query_scalar::<_, bool>("SELECT body_semantic IS NULL FROM docs WHERE id=6")
            .fetch_one(&mut db)
            .await?,
        "newer schema drained"
    );
    db.execute("UPDATE postvec.schema_version SET version=1")
        .await?;
    wait(
        &mut db,
        "SELECT body_semantic IS NOT NULL FROM docs WHERE id=6",
    )
    .await?;
    db.execute("INSERT INTO postvec.jobs(registry_id,pk_value) SELECT id,'(gone,1)' FROM postvec.registry WHERE table_name='composite';ALTER TABLE composite DROP COLUMN body").await?;
    wait(
        &mut db,
        "SELECT state='disabled' FROM postvec.registry WHERE table_name='composite'",
    )
    .await?;
    let proxy_port = std::net::TcpListener::bind("127.0.0.1:0")?
        .local_addr()?
        .port();
    let third = make(31103, dsn.into(), true, Some(proxy_port), None);
    tasks.0.extend(managed::start(
        &third,
        managed::reserve(&third.settings).map_err(anyhow::Error::msg)?,
    ));
    let mut proxied = reqwest::Url::parse(dsn)?;
    proxied.set_host(Some("127.0.0.1"))?;
    proxied.set_port(Some(proxy_port)).unwrap();
    proxied.set_query(None);
    let via = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(c) = PgConnection::connect(proxied.as_str()).await {
                return c;
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await;
    let mut via = via.context("proxy did not accept connections")?;
    wait(&mut via, "SELECT count(*)=3 FROM postvec.models").await?;
    let simple: Vec<String> =
        sqlx::raw_sql("SELECT pk_value FROM postvec.search('docs', 'body', 'hello', limit_n => 1)")
            .fetch_all(&mut via)
            .await?
            .iter()
            .map(|r| r.get::<String, _>("pk_value"))
            .collect();
    ensure!(simple == ["3"], "simple-query search: {simple:?}");
    for _ in 0..2 {
        let extended: String = sqlx::query_scalar(
            "SELECT pk_value FROM postvec.search('public.docs', 'body', $1, limit_n => 1)",
        )
        .bind("hello")
        .fetch_one(&mut via)
        .await?;
        ensure!(extended == "3", "extended-query search: {extended}");
    }
    for (text, expected) in [("hello", "{5,1,2}"), ("different", "{9,1,2}")] {
        let embedded: String = sqlx::query_scalar("SELECT postvec.embed($1, 'fixture')::text")
            .bind(text)
            .fetch_one(&mut via)
            .await?;
        ensure!(embedded == expected, "parameterized embed: {embedded}");
    }
    let embedded: String = sqlx::query_scalar("SELECT postvec.embed('hello', 'fixture')::text")
        .fetch_one(&mut via)
        .await?;
    ensure!(embedded == "{5,1,2}", "embed: {embedded}");
    via.execute("SET standard_conforming_strings=off").await?;
    let legacy = sqlx::raw_sql(r"SELECT postvec.embed('line\nnext','fixture')::text AS v")
        .fetch_one(&mut via)
        .await?;
    ensure!(
        legacy.get::<String, _>("v") == "{9,1,2}",
        "legacy string was embedded literally"
    );
    via.execute("SET standard_conforming_strings=on;SET client_encoding=LATIN1")
        .await?;
    ensure!(
        via.execute("SELECT postvec.embed('text','fixture')")
            .await
            .is_err(),
        "non-UTF8 inference accepted"
    );
    via.execute("SET client_encoding=UTF8").await?;
    let rows = sqlx::raw_sql(
        "BEGIN; SELECT count(*) AS n FROM postvec.search('docs', 'body', 'x'); ROLLBACK;",
    )
    .fetch_all(&mut via)
    .await?;
    ensure!(
        rows.len() == 1 && rows[0].get::<i64, _>("n") > 0,
        "search inside a transaction"
    );
    let unsupported = sqlx::raw_sql("SELECT postvec.search(t.body, 'body', 'x') FROM docs t")
        .execute(&mut via)
        .await
        .err()
        .context("unsupported arguments accepted")?
        .to_string();
    ensure!(
        unsupported.contains("not supported by the proxy"),
        "unsupported arguments: {unsupported}"
    );
    for bad in [
        "SELECT postvec.search(t.body, 'body', 'x') FROM docs t",
        "SELECT postvec.search('missing', 'body', 'x')",
        "SELECT postvec.embed('x', 'no-such-model')",
    ] {
        let error = via
            .execute(bad)
            .await
            .err()
            .context(format!("accepted: {bad}"))?;
        ensure!(
            error.to_string().contains("postvec"),
            "unexpected error for {bad}: {error}"
        );
        ensure!(
            sqlx::query_scalar::<_, i32>(bad)
                .fetch_one(&mut via)
                .await
                .is_err(),
            "accepted: {bad}"
        );
    }
    let still: i32 = sqlx::query_scalar("SELECT $1::int")
        .bind(7)
        .fetch_one(&mut via)
        .await?;
    ensure!(still == 7, "connection unusable after proxy errors");
    ensure!(
        db.execute("SELECT postvec.search('docs', 'body', 'x')")
            .await
            .is_err(),
        "direct search() did not fail"
    );
    if let Ok(out) = tokio::process::Command::new("psql")
        .arg(proxied.as_str())
        .args([
            "-Atc",
            "SELECT pk_value FROM postvec.search('docs','body','hello',limit_n=>1)",
        ])
        .output()
        .await
    {
        ensure!(
            String::from_utf8_lossy(&out.stdout).trim() == "3",
            "psql via proxy: {}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    let proxy = third
        .managed
        .snapshot()
        .into_iter()
        .find(|s| s.name == "test")
        .unwrap()
        .proxy;
    ensure!(
        proxy["rewrites"]["search"].as_u64() >= Some(4) && proxy["connections"].as_i64() >= Some(1),
        "proxy stats: {proxy}"
    );
    via.close().await?;
    let (crt, key) = (
        root.path().join("server.crt"),
        root.path().join("server.key"),
    );
    let certificate = tokio::process::Command::new("openssl")
        .args([
            "req",
            "-x509",
            "-newkey",
            "rsa:2048",
            "-nodes",
            "-days",
            "1",
            "-subj",
            "/CN=localhost",
        ])
        .args([
            "-addext",
            "subjectAltName=DNS:localhost,IP:127.0.0.1",
            "-keyout",
        ])
        .arg(&key)
        .arg("-out")
        .arg(&crt)
        .output()
        .await;
    if certificate.is_ok_and(|o| o.status.success()) {
        let tls_port = std::net::TcpListener::bind("127.0.0.1:0")?
            .local_addr()?
            .port();
        let fourth = make(31104, dsn.into(), true, Some(tls_port), Some((crt, key)));
        tasks.0.extend(managed::start(
            &fourth,
            managed::reserve(&fourth.settings).map_err(anyhow::Error::msg)?,
        ));
        proxied.set_port(Some(tls_port)).unwrap();
        proxied.set_query(Some("sslmode=disable"));
        ensure!(
            PgConnection::connect(proxied.as_str()).await.is_err(),
            "TLS proxy accepted plaintext startup"
        );
        proxied.set_query(Some("sslmode=require"));
        let mut secure = PgConnection::connect(proxied.as_str())
            .await
            .context("TLS proxy")?;
        let hit: String = sqlx::query_scalar(
            "SELECT pk_value FROM postvec.search('docs', 'body', $1, limit_n => 1)",
        )
        .bind("hello")
        .fetch_one(&mut secure)
        .await?;
        ensure!(hit == "3", "search through the TLS proxy: {hit}");
        secure.close().await?;
        if let Ok(out) = tokio::process::Command::new("psql")
            .arg(proxied.as_str())
            .args([
                "-Atc",
                "SELECT pk_value FROM postvec.search('docs','body','hello',limit_n=>1)",
            ])
            .output()
            .await
        {
            ensure!(
                String::from_utf8_lossy(&out.stdout).trim() == "3",
                "psql via TLS proxy: {}",
                String::from_utf8_lossy(&out.stderr)
            );
        }
        fourth.begin_drain();
    }
    first.begin_drain();
    second.begin_drain();
    third.begin_drain();
    drop(tasks);
    tokio::time::sleep(Duration::from_millis(100)).await;
    Ok(())
}
