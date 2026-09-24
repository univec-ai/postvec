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
use sqlx::{Column, Connection, Executor, PgConnection, Row};
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
    wait(
        &mut db,
        "SELECT n=1 FROM postvec.lexical_stats WHERE registry_id=1",
    )
    .await?;
    ensure!(
        sqlx::query_scalar::<_, bool>("SELECT fts_score>0 AND fts_rank=1 FROM postvec.search_with_vector('docs','body',ARRAY[1,0,0]::real[],'hello') WHERE pk_value='1'")
            .fetch_one(&mut db)
            .await?,
        "BM25 lexical leg after the worker's first stats pass"
    );
    db.execute("INSERT INTO postvec.jobs(registry_id,pk_value) VALUES(1,'invalid-integer')")
        .await?;
    wait(
        &mut db,
        "SELECT NOT EXISTS(SELECT FROM postvec.jobs WHERE pk_value='invalid-integer')",
    )
    .await?;
    // A write-back the table rejects comes back with its error and backoff,
    // without restarting the leader session.
    let leader_pid = "SELECT pid FROM postvec.worker_heartbeat";
    let leader: i32 = sqlx::query_scalar(leader_pid).fetch_one(&mut db).await?;
    db.execute("CREATE FUNCTION reject() RETURNS trigger LANGUAGE plpgsql AS $$BEGIN RAISE EXCEPTION 'rejected by trigger'; END$$;
        CREATE TRIGGER reject BEFORE UPDATE OF body_semantic ON docs FOR EACH ROW WHEN (NEW.body='reject') EXECUTE FUNCTION reject();
        INSERT INTO docs VALUES(90,'reject','ninety'),(91,'neighbour','ninety-one')").await?;
    wait(&mut db, "SELECT claimed_at IS NULL AND last_error LIKE 'write-back failed%rejected by trigger%' FROM postvec.jobs WHERE pk_value='90'").await?;
    ensure!(
        sqlx::query_scalar::<_, i32>(leader_pid)
            .fetch_one(&mut db)
            .await?
            == leader,
        "a rejected write-back restarted the leader session"
    );
    wait(
        &mut db,
        "SELECT body_semantic IS NOT NULL FROM docs WHERE id=91",
    )
    .await?;
    db.execute(
        "DROP TRIGGER reject ON docs; DROP FUNCTION reject(); DELETE FROM docs WHERE id IN (90,91)",
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
    // A dead letter is history: it does not hold the automatic index back.
    db.execute("INSERT INTO postvec.jobs_dead(job_id,registry_id,pk_value,last_error) VALUES(0,1,'999','old');UPDATE postvec.registry SET index_mode='auto' WHERE id=1")
        .await?;
    wait(&mut db,"SELECT EXISTS(SELECT FROM pg_index WHERE indrelid='docs'::regclass AND indexrelid<>'docs_pkey'::regclass AND indisvalid)").await?;
    db.execute("DELETE FROM postvec.jobs_dead WHERE pk_value='999'")
        .await?;
    db.execute("CREATE TABLE converted(id uuid PRIMARY KEY,body text,v vectors.vector(3));INSERT INTO converted VALUES('00000000-0000-0000-0000-000000000001','no embedding required','[1,2,3]');SELECT postvec.adopt('converted','body','v','origin',sync=>false,backfill=>'none');SELECT postvec.migrate('converted','body','next',observed_writes_quiesced=>true)").await?;
    wait(
        &mut db,
        "SELECT state='awaiting_finalize' FROM postvec.migrations WHERE id=2",
    )
    .await?;
    ensure!(
        sqlx::query_scalar::<_, String>(
            "SELECT resolved_via->>'kind' FROM postvec.migrations WHERE id=2"
        )
        .fetch_one(&mut db)
        .await?
            == "direct",
        "a converter route is recorded as direct, as in the extension"
    );
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
    // A cursor keeps feeding while trigger jobs stay pending.
    db.execute("CREATE TABLE busy(id int PRIMARY KEY,body text);INSERT INTO busy SELECT g,'row '||g FROM generate_series(1,3) g;
        SELECT postvec.enable('busy','body','fixture',backfill_mode=>'cursor');
        INSERT INTO postvec.jobs(registry_id,pk_value,not_before) SELECT id,'999',now()+interval '1 hour' FROM postvec.registry WHERE table_name='busy'").await?;
    wait(
        &mut db,
        "SELECT bool_and(body_semantic IS NOT NULL) FROM busy",
    )
    .await?;
    db.execute("DELETE FROM postvec.jobs WHERE pk_value='999'")
        .await?;
    // A key written under another DateStyle still names its own row.
    db.execute("CREATE TABLE dated(id date PRIMARY KEY,body text);INSERT INTO dated VALUES('2026-02-03','feb'),('2026-03-02','mar');SELECT postvec.enable('dated','body','fixture')").await?;
    wait(
        &mut db,
        "SELECT bool_and(body_semantic IS NOT NULL) FROM dated",
    )
    .await?;
    db.execute("SET DateStyle='SQL, DMY';UPDATE dated SET body='february changed' WHERE id='2026-02-03';RESET DateStyle").await?;
    wait(
        &mut db,
        "SELECT body_semantic::text='[16,1,2]' FROM dated WHERE id='2026-02-03'",
    )
    .await?;
    db.execute("CREATE TABLE refill(id int PRIMARY KEY,body text,v vectors.vector(3));INSERT INTO refill VALUES(1,'refilled','[99,99,99]');SELECT postvec.adopt('refill','body','v','fixture',backfill=>'all',backfill_mode=>'cursor')").await?;
    wait(&mut db, "SELECT v::text='[8,1,2]' FROM refill").await?;
    wait(
        &mut db,
        "SELECT NOT EXISTS(SELECT FROM postvec.settings WHERE key LIKE 'backfill_all:%')",
    )
    .await?;
    // Aborting keeps the stored vectors of an adopted column and re-embeds,
    // with the original model, the rows the migration wrote.
    paused.store(true, Ordering::SeqCst);
    db.execute("CREATE TABLE kept(id int PRIMARY KEY,body text,v vectors.vector(3));INSERT INTO kept VALUES(1,'kept row','[7,7,7]'),(2,'untouched','[6,6,6]');SELECT postvec.adopt('kept','body','v','fixture',backfill=>'none');
        DO $$DECLARE m postvec.migrations; mid bigint; BEGIN
            mid:=postvec.migrate('kept','body','next',strategy=>'reembed');
            SELECT * INTO m FROM postvec.migrations WHERE id=mid;
            EXECUTE format('UPDATE kept SET %I=''[5,5,5]'' WHERE id=1',m.new_column);
            PERFORM postvec.migration_abort(m.id);
        END$$").await?;
    ensure!(
        sqlx::query_scalar::<_, String>("SELECT string_agg(v::text,',' ORDER BY id) FROM kept")
            .fetch_one(&mut db)
            .await?
            == "[7,7,7],[6,6,6]",
        "abort discarded the adopted vectors"
    );
    paused.store(false, Ordering::SeqCst);
    wait(&mut db, "SELECT v::text='[8,1,2]' FROM kept WHERE id=1").await?;
    ensure!(
        sqlx::query_scalar::<_, String>("SELECT v::text FROM kept WHERE id=2")
            .fetch_one(&mut db)
            .await?
            == "[6,6,6]",
        "abort re-embedded a row the migration never wrote"
    );
    // A model no inference node serves fails the migration instead of
    // retrying the same batch forever.
    db.execute("INSERT INTO postvec.models(name,model_type,target_dim,raw) VALUES('ghost','embed',3,'{}') ON CONFLICT DO NOTHING;
        CREATE TABLE ghostly(id int PRIMARY KEY,body text);INSERT INTO ghostly VALUES(1,'boo');SELECT postvec.enable('ghostly','body','fixture')").await?;
    wait(&mut db, "SELECT body_semantic IS NOT NULL FROM ghostly").await?;
    db.execute("SELECT postvec.migrate('ghostly','body','ghost',strategy=>'reembed')")
        .await?;
    wait(&mut db, "SELECT m.state='failed' FROM postvec.migrations m JOIN postvec.registry r ON r.id=m.registry_id WHERE r.table_name='ghostly'").await?;
    // A migration batch the table rejects backs off with the error and
    // resumes once the table accepts writes again.
    db.execute("CREATE FUNCTION frozen() RETURNS trigger LANGUAGE plpgsql AS $$BEGIN RAISE EXCEPTION 'frozen table'; END$$;
        CREATE TRIGGER frozen BEFORE UPDATE ON kept FOR EACH ROW EXECUTE FUNCTION frozen();
        SELECT postvec.migrate('kept','body','next',strategy=>'reembed')").await?;
    wait(&mut db, "SELECT m.state='running' AND m.retry_failures>0 AND m.error LIKE 'write failed%frozen table%' FROM postvec.migrations m JOIN postvec.registry r ON r.id=m.registry_id WHERE r.table_name='kept' AND m.state<>'aborted'").await?;
    db.execute("DROP TRIGGER frozen ON kept; DROP FUNCTION frozen()")
        .await?;
    wait(&mut db, "SELECT m.state='awaiting_finalize' FROM postvec.migrations m JOIN postvec.registry r ON r.id=m.registry_id WHERE r.table_name='kept' AND m.state<>'aborted'").await?;
    db.execute("SELECT postvec.migration_abort(m.id) FROM postvec.migrations m JOIN postvec.registry r ON r.id=m.registry_id WHERE r.table_name='kept' AND m.state='awaiting_finalize'").await?;
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
        "UPDATE postvec.schema_version SET version=version+100;INSERT INTO docs VALUES(6,'parked','version')",
    )
    .await?;
    tokio::time::sleep(Duration::from_secs(2)).await;
    ensure!(
        sqlx::query_scalar::<_, bool>("SELECT body_semantic IS NULL FROM docs WHERE id=6")
            .fetch_one(&mut db)
            .await?,
        "newer schema drained"
    );
    db.execute("UPDATE postvec.schema_version SET version=version-100")
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
    db.execute("UPDATE postvec.registry SET model='retired-route',space=COALESCE(space,model) WHERE table_name='docs' AND source_column='body'").await?;
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
        unsupported.contains("served only by the postvec-server proxy"),
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
    // The rewritten call is the client's own function: its result name, its
    // EXECUTE privilege, and the server's prepared statements all stay real.
    let named = sqlx::raw_sql("SELECT postvec.embed('hello', 'fixture')")
        .fetch_one(&mut via)
        .await?;
    ensure!(named.columns()[0].name() == "embed", "embed result renamed");
    ensure!(
        via.execute("SELECT postvec.embed('hello'::char, 'fixture')")
            .await
            .is_err(),
        "a non-text cast was dropped before inference"
    );
    let dealloc = "SELECT postvec.embed($1, 'fixture')::text AS deallocated";
    sqlx::query(dealloc).bind("x").fetch_one(&mut via).await?;
    via.execute(sqlx::raw_sql("DEALLOCATE ALL")).await?;
    ensure!(
        sqlx::query(dealloc)
            .bind("x")
            .fetch_one(&mut via)
            .await
            .is_err(),
        "a deallocated statement ran from the proxy's copy"
    );
    ensure!(
        via.execute(sqlx::raw_sql("SELECT postvec.embed('ok', 'fixture'); BEGIN; CREATE TEMP TABLE committed_before(x int); COMMIT; SELECT postvec.embed('x', 'no-such-model')"))
            .await
            .is_err()
    );
    ensure!(
        sqlx::query_scalar::<_, bool>("SELECT to_regclass('pg_temp.committed_before') IS NOT NULL")
            .fetch_one(&mut via)
            .await?,
        "statements before a failing embed were dropped"
    );
    // Raw pipelines: a Bind follows the server's verdict on earlier cycles,
    // never runs SQL the server rejected or deallocated, and embeds a typed
    // parameter only when PostgreSQL reads it as the same text.
    let mut wire = Wire::connect(&proxied).await?;
    let orig = "SELECT postvec.embed('abcd', 'fixture')::text";
    wire.send(&[parse("orig", orig, &[]), sync()]).await?;
    wire.cycle().await?;
    wire.send(&[
        parse("", "SELECT pg_sleep(0.5)", &[]),
        bind("", "", &[]),
        execute(""),
        parse(
            "orig",
            "SELECT postvec.embed('longerwrong', 'fixture')::text",
            &[],
        ),
        sync(),
        bind("", "orig", &[]),
        execute(""),
        sync(),
    ])
    .await?;
    ensure!(wire.cycle().await?.error.contains("already exists"));
    ensure!(
        wire.cycle().await?.value == "{4,1,2}",
        "ran the rejected Parse"
    );
    wire.send(&[query("SELECT 1/0; DEALLOCATE orig")]).await?;
    wire.cycle().await?;
    wire.send(&[bind("", "orig", &[]), execute(""), sync()])
        .await?;
    ensure!(
        wire.cycle().await?.value == "{4,1,2}",
        "a DEALLOCATE that never ran"
    );
    wire.send(&[
        parse("", "DEALLOCATE orig", &[]),
        bind("", "", &[]),
        execute(""),
        sync(),
    ])
    .await?;
    wire.cycle().await?;
    wire.send(&[bind("", "orig", &[]), execute(""), sync()])
        .await?;
    ensure!(
        wire.cycle().await?.error.contains("does not exist"),
        "ran a deallocated statement"
    );
    // A Bind of a statement the server no longer has fails before anything
    // runs, even where the vector would never be evaluated.
    db.execute("CREATE TABLE guard_probe (x int)").await?;
    wire.send(&[
        parse("gone", "WITH ins AS (INSERT INTO guard_probe VALUES (1)) SELECT postvec.embed('abcd', 'fixture') WHERE false", &[]),
        sync(),
    ])
    .await?;
    wire.cycle().await?;
    wire.send(&[query("DEALLOCATE gone")]).await?;
    wire.cycle().await?;
    wire.send(&[bind("", "gone", &[]), execute(""), sync()])
        .await?;
    ensure!(wire.cycle().await?.error.contains("does not exist"));
    ensure!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM guard_probe")
            .fetch_one(&mut db)
            .await?
            == 0,
        "a deallocated statement ran"
    );
    // A portal outlives the statement it was bound from.
    wire.send(&[
        query("BEGIN"),
        parse("held_stmt", orig, &[]),
        bind("held", "held_stmt", &[]),
        close("held_stmt"),
        execute("held"),
        sync(),
    ])
    .await?;
    wire.cycle().await?;
    ensure!(
        wire.cycle().await?.value == "{4,1,2}",
        "a surviving portal was refused"
    );
    wire.send(&[query("COMMIT")]).await?;
    wire.cycle().await?;
    // A failed unnamed Parse leaves no unnamed statement behind.
    wire.send(&[
        parse("", orig, &[]),
        sync(),
        parse("", "SELECT syntax error !!!", &[]),
        sync(),
    ])
    .await?;
    wire.cycle().await?;
    wire.cycle().await?;
    wire.send(&[bind("", "", &[]), execute(""), sync()]).await?;
    ensure!(
        wire.cycle().await?.error.contains("does not exist"),
        "a failed unnamed Parse left the old statement executable"
    );
    let typed = "SELECT postvec.embed($1::text, 'fixture')::text";
    wire.send(&[
        parse("int4", typed, &[23]),
        parse("text", typed, &[25]),
        sync(),
    ])
    .await?;
    wire.cycle().await?;
    wire.send(&[bind("", "int4", &["00042"]), execute(""), sync()])
        .await?;
    ensure!(
        !wire.cycle().await?.error.is_empty(),
        "embedded an int4 parameter as its wire text"
    );
    wire.send(&[bind("", "text", &["00042"]), execute(""), sync()])
        .await?;
    ensure!(wire.cycle().await?.value == "{5,1,2}");
    // A portal Describe owes no ParameterDescription; the next statement's
    // Describe still hides the proxy's parameter.
    wire.send(&[
        parse("described", orig, &[]),
        bind("", "described", &[]),
        describe(b'P', ""),
        execute(""),
        parse("after", orig, &[]),
        sync(),
    ])
    .await?;
    ensure!(wire.cycle().await?.value == "{4,1,2}");
    wire.send(&[
        describe(b'S', "after"),
        bind("", "after", &[]),
        execute(""),
        sync(),
    ])
    .await?;
    let after = wire.cycle().await?;
    ensure!(
        after.params == Some(0) && after.value == "{4,1,2}",
        "statement after a portal Describe: {:?} {}",
        after.params,
        after.error
    );
    // SQL that replaces a rewritten statement gets only the client's own
    // parameters, by simple query or extended protocol.
    db.execute("CREATE TABLE replaced_probe (v text)").await?;
    wire.send(&[
        parse("replaced", orig, &[]),
        parse("replaced2", orig, &[]),
        sync(),
    ])
    .await?;
    wire.cycle().await?;
    wire.send(&[
        query(
            "DEALLOCATE replaced; PREPARE replaced(text) AS INSERT INTO replaced_probe VALUES ($1)",
        ),
        parse("", "DEALLOCATE PREPARE replaced2", &[]),
        bind("", "", &[]),
        execute(""),
        parse(
            "",
            "PREPARE replaced2(text) AS INSERT INTO replaced_probe VALUES ($1)",
            &[],
        ),
        bind("", "", &[]),
        execute(""),
        sync(),
    ])
    .await?;
    for _ in 0..2 {
        ensure!(wire.cycle().await?.error.is_empty());
    }
    for name in ["replaced", "replaced2"] {
        wire.send(&[bind("", name, &[]), execute(""), sync()])
            .await?;
        let error = wire.cycle().await?.error;
        ensure!(error.contains("requires 1"), "{name}: {error}");
    }
    ensure!(
        sqlx::query_scalar::<_, i64>("SELECT count(*) FROM replaced_probe")
            .fetch_one(&mut db)
            .await?
            == 0,
        "a replacement statement received the proxy's vector"
    );
    // A call's inference error raises only where that call is evaluated.
    let (ok, skipped) = (
        "postvec.embed('abcd', 'fixture')::text",
        "CASE WHEN false THEN postvec.embed('x', 'no-such-model')::text END",
    );
    for sql in [
        format!("SELECT coalesce({skipped}, {ok})"),
        format!("SELECT coalesce({ok}, {skipped})"),
    ] {
        wire.send(&[parse("", &sql, &[]), bind("", "", &[]), execute(""), sync()])
            .await?;
        let cycle = wire.cycle().await?;
        ensure!(cycle.value == "{4,1,2}", "{sql}: {}", cycle.error);
    }
    // Parentheses, spelled-out casts and an undeclared `$n::text` are
    // rewritten; a `$n` another use types as integer is not.
    for sql in [
        "SELECT postvec.embed(($1)::text, 'fixture')::text",
        "SELECT postvec.embed($1::character varying, 'fixture')::text",
        "SELECT postvec.embed($1::text, 'fixture')::text",
        "SELECT postvec.embed(($1), 'fixture')::text",
    ] {
        wire.send(&[
            parse("", sql, &[]),
            bind("", "", &["abcd"]),
            execute(""),
            sync(),
        ])
        .await?;
        let cycle = wire.cycle().await?;
        ensure!(cycle.value == "{4,1,2}", "{sql}: {}", cycle.error);
    }
    wire.send(&[
        parse(
            "",
            "SELECT postvec.embed($1::text, 'fixture')::text, $1::integer",
            &[],
        ),
        bind("", "", &["00042"]),
        execute(""),
        sync(),
    ])
    .await?;
    ensure!(
        wire.cycle()
            .await?
            .error
            .contains("served only by the postvec-server proxy"),
        "embedded a parameter PostgreSQL types as integer"
    );
    // A hex escape elsewhere in the query leaves the call rewritten.
    wire.send(&[query(
        "SELECT postvec.embed('abcd', 'fixture')::text, E'\\x41'",
    )])
    .await?;
    ensure!(wire.cycle().await?.value == "{4,1,2}");
    // An unnamed Parse an earlier error skipped leaves the old one in place.
    wire.send(&[parse("", orig, &[]), sync()]).await?;
    wire.cycle().await?;
    wire.send(&[
        bind("", "no_such_statement", &[]),
        parse("", "SELECT 2", &[]),
        sync(),
    ])
    .await?;
    ensure!(!wire.cycle().await?.error.is_empty());
    wire.send(&[bind("", "", &[]), execute(""), sync()]).await?;
    let kept = wire.cycle().await?;
    ensure!(
        kept.value == "{4,1,2}",
        "skipped unnamed Parse: {}",
        kept.error
    );
    // Deep nesting is PostgreSQL's to refuse; the proxy keeps serving.
    let deep = format!(
        "SELECT postvec.embed({}'abcd'{}, 'fixture')",
        "(".repeat(20_000),
        ")".repeat(20_000)
    );
    wire.send(&[query(&deep)]).await?;
    wire.cycle().await?;
    wire.send(&[query(orig)]).await?;
    ensure!(
        wire.cycle().await?.value == "{4,1,2}",
        "the proxy stopped serving after deep nesting"
    );
    // A rewritten Bind counts toward locating the failed message: here the
    // duplicate Parse failed, and the unnamed Parse after it was skipped.
    wire.send(&[parse("", orig, &[]), parse("taken", orig, &[]), sync()])
        .await?;
    wire.cycle().await?;
    wire.send(&[
        bind("", "", &[]),
        parse("taken", orig, &[]),
        parse("", "SELECT 2", &[]),
        sync(),
    ])
    .await?;
    ensure!(wire.cycle().await?.error.contains("already exists"));
    wire.send(&[bind("", "", &[]), execute(""), sync()]).await?;
    let kept = wire.cycle().await?;
    ensure!(
        kept.value == "{4,1,2}",
        "after a rewritten Bind: {}",
        kept.error
    );
    // PostgreSQL types `$1` as character(4) from its first use, so embed()
    // reads 'ab', not the bytes sent: the call raises instead of using them.
    for format in [0, 1] {
        wire.send(&[
            parse(
                "",
                "SELECT $1::character(4), postvec.embed($1, 'fixture')::text",
                &[],
            ),
            bind_format("", "", &["ab  "], format),
            execute(""),
            sync(),
        ])
        .await?;
        let error = wire.cycle().await?.error;
        ensure!(
            error.contains("differently from the text sent"),
            "format {format}: {error}"
        );
    }
    let reader = format!("{}_reader", via_db(&proxied));
    db.execute(format!("CREATE ROLE {reader} LOGIN; REVOKE EXECUTE ON FUNCTION postvec.embed(text,text,real[]) FROM PUBLIC").as_str())
        .await?;
    let mut as_reader = proxied.clone();
    as_reader.set_username(&reader).unwrap();
    let mut reader_conn = PgConnection::connect(as_reader.as_str()).await?;
    let denied = reader_conn
        .execute("SELECT postvec.embed('hello', 'fixture')")
        .await
        .err()
        .context("embed without EXECUTE accepted through the proxy")?;
    ensure!(
        denied
            .to_string()
            .contains("permission denied for function embed"),
        "unexpected refusal: {denied}"
    );
    // EXECUTE reached through SET ROLE, not inherited, is still EXECUTE.
    let executor = format!("{reader}_exec");
    db.execute(format!("CREATE ROLE {executor}; GRANT EXECUTE ON FUNCTION postvec.embed(text,text,real[]) TO {executor};
        GRANT {executor} TO {reader} WITH INHERIT FALSE, SET TRUE").as_str()).await?;
    reader_conn
        .execute(format!("SET ROLE {executor}").as_str())
        .await?;
    reader_conn
        .execute("SELECT postvec.embed('hello', 'fixture')")
        .await
        .context("embed after SET ROLE to a role with EXECUTE")?;
    reader_conn.close().await?;
    db.execute(format!("GRANT EXECUTE ON FUNCTION postvec.embed(text,text,real[]) TO PUBLIC; DROP OWNED BY {executor}; DROP ROLE {executor}, {reader}").as_str())
        .await?;
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

fn via_db(url: &reqwest::Url) -> String {
    url.path().trim_start_matches('/').to_string()
}

/// A bare pgwire client, for pipelines no driver sends.
struct Wire(tokio::net::TcpStream);
#[derive(Default)]
struct Cycle {
    value: String,
    error: String,
    params: Option<u16>,
}
fn message(kind: u8, body: &[u8]) -> Vec<u8> {
    let mut m = vec![kind];
    m.extend((body.len() as i32 + 4).to_be_bytes());
    m.extend(body);
    m
}
fn cstring(s: &str) -> Vec<u8> {
    let mut v = s.as_bytes().to_vec();
    v.push(0);
    v
}
fn query(sql: &str) -> Vec<u8> {
    message(b'Q', &cstring(sql))
}
fn parse(name: &str, sql: &str, oids: &[u32]) -> Vec<u8> {
    let mut body = [cstring(name), cstring(sql)].concat();
    body.extend((oids.len() as u16).to_be_bytes());
    oids.iter().for_each(|o| body.extend(o.to_be_bytes()));
    message(b'P', &body)
}
fn bind(portal: &str, statement: &str, params: &[&str]) -> Vec<u8> {
    bind_format(portal, statement, params, 0)
}
/// A Bind whose parameters all use `format` (0 text, 1 binary).
fn bind_format(portal: &str, statement: &str, params: &[&str], format: u16) -> Vec<u8> {
    let mut body = [cstring(portal), cstring(statement)].concat();
    body.extend(1u16.to_be_bytes());
    body.extend(format.to_be_bytes());
    body.extend((params.len() as u16).to_be_bytes());
    for p in params {
        body.extend((p.len() as i32).to_be_bytes());
        body.extend(p.as_bytes());
    }
    body.extend(0u16.to_be_bytes());
    message(b'B', &body)
}
fn execute(portal: &str) -> Vec<u8> {
    message(b'E', &[cstring(portal), vec![0, 0, 0, 0]].concat())
}
fn describe(kind: u8, name: &str) -> Vec<u8> {
    message(b'D', &[vec![kind], cstring(name)].concat())
}
fn close(statement: &str) -> Vec<u8> {
    message(b'C', &[vec![b'S'], cstring(statement)].concat())
}
fn sync() -> Vec<u8> {
    message(b'S', &[])
}
impl Wire {
    async fn connect(url: &reqwest::Url) -> Result<Self> {
        use tokio::io::AsyncWriteExt;
        let mut tcp =
            tokio::net::TcpStream::connect((url.host_str().unwrap(), url.port().unwrap())).await?;
        let mut body = 196608i32.to_be_bytes().to_vec();
        body.extend(
            [
                cstring("user"),
                cstring(url.username()),
                cstring("database"),
                cstring(&via_db(url)),
                vec![0],
            ]
            .concat(),
        );
        let mut startup = (body.len() as i32 + 4).to_be_bytes().to_vec();
        startup.extend(body);
        tcp.write_all(&startup).await?;
        let mut wire = Wire(tcp);
        wire.cycle().await?;
        Ok(wire)
    }
    async fn send(&mut self, messages: &[Vec<u8>]) -> Result<()> {
        use tokio::io::AsyncWriteExt;
        Ok(self.0.write_all(&messages.concat()).await?)
    }
    /// Messages up to the next ReadyForQuery: the first column of the first
    /// row, and the error message if any.
    async fn cycle(&mut self) -> Result<Cycle> {
        use tokio::io::AsyncReadExt;
        let mut cycle = Cycle::default();
        loop {
            let kind = self.0.read_u8().await?;
            let mut body = vec![0; self.0.read_i32().await? as usize - 4];
            self.0.read_exact(&mut body).await?;
            match kind {
                b'D' if cycle.value.is_empty() => {
                    let len = i32::from_be_bytes(body[2..6].try_into()?);
                    cycle.value = String::from_utf8_lossy(&body[6..6 + len.max(0) as usize]).into();
                }
                b'E' => {
                    let at = body
                        .windows(2)
                        .position(|w| w[0] == 0 && w[1] == b'M')
                        .map_or(0, |i| i + 2);
                    cycle.error = String::from_utf8_lossy(&body[at..])
                        .split('\0')
                        .next()
                        .unwrap_or_default()
                        .into();
                }
                b't' => cycle.params = Some(u16::from_be_bytes([body[0], body[1]])),
                b'Z' => return Ok(cycle),
                _ => {}
            }
        }
    }
}
