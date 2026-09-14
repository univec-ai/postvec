// SPDX-License-Identifier: BUSL-1.1

use anyhow::{ensure, Context, Result};
use postvec_server::managed::{self, Command, ConnectionArgs};
use sqlx::{Connection, Executor, PgConnection};

fn command(dsn: &str, action: &str) -> Command {
    let args = ConnectionArgs {
        dsn: dsn.into(),
        password_file: None,
        timeout: 10,
    };
    match action {
        "install" => Command::Install(args),
        "status" => Command::Status(args),
        "uninstall" => Command::Uninstall(args),
        _ => unreachable!(),
    }
}

#[tokio::test]
#[ignore = "POSTVEC_MANAGED_TEST_DSN must name a disposable PostgreSQL 18 admin database with pgvector installed on disk"]
async fn managed_lifecycle() -> Result<()> {
    let dsn = std::env::var("POSTVEC_MANAGED_TEST_DSN").context("set POSTVEC_MANAGED_TEST_DSN")?;
    let mut admin = PgConnection::connect(&dsn).await?;
    let name = format!("managed_{}", uuid::Uuid::new_v4().simple());
    admin
        .execute(format!("CREATE ROLE {name} LOGIN PASSWORD '{name}'").as_str())
        .await?;
    admin
        .execute(format!("CREATE DATABASE {name} OWNER {name}").as_str())
        .await?;
    let mut url = reqwest::Url::parse(&dsn)?;
    url.set_path(&format!("/{name}"));
    let mut setup = PgConnection::connect(url.as_str()).await?;
    setup.execute("CREATE SCHEMA vectors; CREATE EXTENSION vector SCHEMA vectors; GRANT USAGE ON SCHEMA vectors TO PUBLIC").await?;
    setup.close().await?;
    url.set_username(&name).unwrap();
    url.set_password(Some(&name)).unwrap();
    let result = async {
        exercise(url.as_str()).await?;
        if std::env::var_os("POSTVEC_MANAGED_TEST_EXTENSION").is_some() {
            managed::run(command(url.as_str(), "install")).await?;
            let mut managed_db = PgConnection::connect(url.as_str()).await?;
            let contract = table_contract(&mut managed_db).await?;
            managed::run(command(url.as_str(), "uninstall")).await?;
            managed_db.close().await?;
            let mut extension_url = reqwest::Url::parse(&dsn)?;
            extension_url.set_path(&format!("/{name}"));
            let mut extension_db = PgConnection::connect(extension_url.as_str()).await?;
            extension_db.execute("CREATE EXTENSION postvec").await?;
            ensure!(
                table_contract(&mut extension_db).await? == contract,
                "durable schema drift"
            );
            for action in ["install", "status", "uninstall"] {
                ensure!(
                    managed::run(command(url.as_str(), action)).await.is_err(),
                    "extension accepted"
                );
            }
            extension_db.close().await?;
        }
        Ok(())
    }
    .await;
    let cleanup = async {
        admin
            .execute(format!("DROP DATABASE {name} WITH (FORCE)").as_str())
            .await?;
        admin.execute(format!("DROP ROLE {name}").as_str()).await?;
        Ok(())
    }
    .await;
    result.and(cleanup)
}

async fn exercise(dsn: &str) -> Result<()> {
    let mut db = PgConnection::connect(dsn).await?;
    db.execute("CREATE SCHEMA postvec").await?;
    ensure!(
        managed::run(command(dsn, "install")).await.is_err(),
        "foreign schema accepted"
    );
    db.execute("DROP SCHEMA postvec").await?;
    managed::run(command(dsn, "install")).await?;
    let truncate: String =
        sqlx::query_scalar("SELECT pg_get_functiondef('postvec.trg_truncate()'::regprocedure)")
            .fetch_one(&mut db)
            .await?;
    db.execute("CREATE OR REPLACE FUNCTION postvec.trg_truncate() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'stale'; END $$").await?;
    managed::run(command(dsn, "install")).await?;
    ensure!(
        sqlx::query_scalar::<_, String>(
            "SELECT pg_get_functiondef('postvec.trg_truncate()'::regprocedure)"
        )
        .fetch_one(&mut db)
        .await?
            == truncate,
        "truncate function was not refreshed"
    );
    managed::run(command(dsn, "status")).await?;
    db.execute("UPDATE postvec.schema_version SET version = 2")
        .await?;
    for action in ["install", "status", "uninstall"] {
        ensure!(
            managed::run(command(dsn, action)).await.is_err(),
            "newer version accepted"
        );
    }
    db.execute("UPDATE postvec.schema_version SET version = 1")
        .await?;
    db.execute(r#"
        CREATE TABLE docs (id bigint PRIMARY KEY, body text, category varchar(10), v vectors.vector(3));
        INSERT INTO docs VALUES (1,'reset password','account','[1,0,0]'), (2,'billing invoice','billing','[0,1,0]');
        INSERT INTO postvec.registry (table_schema,table_name,source_column,vector_column,pk_columns,pk_types,model,dim)
        VALUES ('public','docs','body','v',ARRAY['id'],ARRAY['bigint'],'fixture',3);
        CREATE TRIGGER postvec_trunc_1 AFTER TRUNCATE ON docs FOR EACH STATEMENT EXECUTE FUNCTION postvec.trg_truncate('1');
        INSERT INTO postvec.jobs_dead (job_id,registry_id,pk_value,last_error) VALUES (1,1,'1','bad'),(2,1,'1','bad');
    "#).await.context("create docs")?;
    db.execute(r#"
        DO $$ BEGIN
            IF (SELECT worker_alive FROM postvec.status()) THEN RAISE EXCEPTION 'false liveness'; END IF;
            IF (SELECT queue_dead FROM postvec.stats()) <> 2 THEN RAISE EXCEPTION 'dead count'; END IF;
            IF (SELECT count(*) FROM postvec.migration_status()) <> 0 THEN RAISE EXCEPTION 'migrations'; END IF;
            IF postvec.retry_dead('docs','body',ARRAY[1,1,2]) <> 2 THEN RAISE EXCEPTION 'retry count'; END IF;
            IF (SELECT count(*) FROM postvec.jobs) <> 1 THEN RAISE EXCEPTION 'dedup'; END IF;
        END $$;
    "#).await.context("status/retry assertions")?;
    for filter in [
        r#"{"category":"account"}"#,
        r#"{"category":{"neq":"billing"}}"#,
        r#"{"category":["account"]}"#,
        r#"{"category":{"ilike":"Acc%"}}"#,
        r#"{"id":{"gte":1,"lt":2}}"#,
    ] {
        let keys: Vec<String> = sqlx::query_scalar("SELECT pk_value FROM postvec.search_with_vector('docs','body',ARRAY[1,0,0]::real[], 'password', filter => $1::jsonb)")
            .bind(filter).fetch_all(&mut db).await.with_context(|| format!("search filter {filter}"))?;
        ensure!(keys == ["1"], "filter mismatch: {filter}");
    }
    let scored: bool = sqlx::query_scalar("SELECT semantic_distance=0 AND fts_score>0 FROM postvec.search_with_vector('docs','body',ARRAY[1,0,0]::real[],'password') WHERE pk_value='1'")
        .fetch_one(&mut db).await?;
    ensure!(scored, "per-leg scores");
    for query in [
        "SELECT * FROM postvec.search_with_vector('docs','body',ARRAY[1,0]::real[])",
        "SELECT * FROM postvec.search_with_vector('docs','body',ARRAY[1,NULL,0]::real[])",
        "SELECT * FROM postvec.search_with_vector('docs','body',ARRAY[1,'NaN',0]::real[])",
        "SELECT * FROM postvec.search_with_vector('docs','body',ARRAY[1,0,0]::real[],filter => '{\"category\":{\"sql\":\"true\"}}')",
        "SELECT * FROM postvec.search_with_vector('docs','body',ARRAY[1,0,0]::real[],filter => '{\"category\":\"more than ten chars\"}')",
        "SELECT postvec.retry_dead('docs','body',ARRAY[999])",
    ] { ensure!(db.execute(query).await.is_err(), "invalid input accepted: {query}"); }
    db.execute(r#"
        ALTER TABLE docs ENABLE ROW LEVEL SECURITY;
        ALTER TABLE docs FORCE ROW LEVEL SECURITY;
        CREATE POLICY visible ON docs USING (id = 1);
        DO $$ BEGIN
            IF (SELECT count(*) FROM postvec.search_with_vector('docs','body',ARRAY[1,0,0]::real[], 'billing')) <> 1
            THEN RAISE EXCEPTION 'search bypassed RLS'; END IF;
        END $$;
        ALTER TABLE docs DISABLE ROW LEVEL SECURITY;
        CREATE TABLE chunk_docs (id bigint PRIMARY KEY, body text);
        CREATE TABLE chunks (postvec_chunk_id bigint PRIMARY KEY, postvec_source_pk bigint,
            postvec_chunk_seq integer, postvec_char_start bigint, postvec_char_end bigint,
            chunk_text text, v vectors.vector(3));
        INSERT INTO chunk_docs VALUES (1,'password reset'),(2,'billing');
        INSERT INTO chunks VALUES (1,1,0,0,8,'password','[1,0,0]'),(2,1,1,8,14,'reset','[1,0.1,0]'),
                                  (3,2,0,0,7,'billing','[0,1,0]');
        INSERT INTO postvec.registry (table_schema,table_name,source_column,vector_column,pk_columns,pk_types,model,dim,
            chunking,chunk_size,chunk_overlap,destination_schema,destination_table,destination_view,destination_token)
        VALUES ('public','chunk_docs','body','v',ARRAY['id'],ARRAY['bigint'],'fixture',3,
            'recursive',64,0,'public','chunks','chunk_view','test');
        CREATE TRIGGER postvec_trunc_2 AFTER TRUNCATE ON chunk_docs FOR EACH STATEMENT EXECUTE FUNCTION postvec.trg_truncate('2');
        INSERT INTO postvec.jobs_dead (job_id,registry_id,pk_value,op,chunk_id)
        VALUES (3,2,'1','embed',1),(4,2,'1','embed',999),(5,2,'1','refresh',NULL);
        DO $$ BEGIN
            IF (SELECT count(*) FROM postvec.search_with_vector('chunk_docs','body',ARRAY[1,0,0]::real[], 'password')) <> 2
            THEN RAISE EXCEPTION 'chunk collapse'; END IF;
            IF (SELECT chunk_text FROM postvec.search_with_vector('chunk_docs','body',ARRAY[1,0,0]::real[], 'password') LIMIT 1) <> 'password'
            THEN RAISE EXCEPTION 'winning chunk text'; END IF;
            IF postvec.retry_dead('chunk_docs','body') <> 3 THEN RAISE EXCEPTION 'chunk retry count'; END IF;
            IF (SELECT count(*) FROM postvec.jobs WHERE registry_id=2) <> 2 THEN RAISE EXCEPTION 'obsolete chunk retry'; END IF;
        END $$;
        TRUNCATE chunk_docs;
        DO $$ BEGIN
            IF EXISTS (SELECT FROM chunks) OR EXISTS (SELECT FROM postvec.jobs WHERE registry_id=2)
            THEN RAISE EXCEPTION 'truncate cleanup'; END IF;
        END $$;
    "#).await?;
    db.execute(r#"
        INSERT INTO postvec.models(name,model_type,target_model,target_dim,raw) VALUES('fixture','embed','fixture',3,'{}');
        CREATE TABLE pk_changes(id integer PRIMARY KEY,body text,note text);
        SELECT postvec.enable('pk_changes','body','fixture');
        DO $$ BEGIN
            IF postvec.enable('pk_changes','body','fixture',if_not_exists=>true)<>(SELECT id FROM postvec.registry WHERE table_name='pk_changes')
            THEN RAISE EXCEPTION 'if_not_exists returned another id'; END IF;
        END $$;
        CREATE TABLE row_changes(id integer PRIMARY KEY,body text,note text);
        SELECT postvec.enable('row_changes','body','fixture',trigger_mode=>'row',format=>'$note: $body');
        INSERT INTO pk_changes VALUES(1,'before',NULL),(2,NULL,'no source');
        INSERT INTO row_changes VALUES(1,'before',NULL),(2,NULL,'no source');
        DO $$ BEGIN
            IF (SELECT count(*) FROM postvec.jobs j JOIN postvec.registry r ON r.id=j.registry_id WHERE r.table_name IN ('pk_changes','row_changes')) <> 2
            THEN RAISE EXCEPTION 'NULL-source insert enqueued'; END IF;
        END $$;
        DELETE FROM postvec.jobs;
        UPDATE pk_changes SET note='unrelated';
        UPDATE row_changes SET id=id;
        DO $$ BEGIN
            IF EXISTS(SELECT FROM postvec.jobs) THEN RAISE EXCEPTION 'unrelated column update enqueued'; END IF;
        END $$;
        UPDATE row_changes SET note='context';
        DO $$ BEGIN
            IF (SELECT count(*) FROM postvec.jobs WHERE registry_id=(SELECT id FROM postvec.registry WHERE table_name='row_changes')) <> 2
            THEN RAISE EXCEPTION 'context column update was not enqueued'; END IF;
        END $$;
        SELECT postvec.disable('row_changes','body');
        CREATE FUNCTION move_pk() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN NEW.id:=NEW.id+10; RETURN NEW; END $$;
        CREATE TRIGGER move_pk BEFORE UPDATE ON pk_changes FOR EACH ROW EXECUTE FUNCTION move_pk();
        UPDATE pk_changes SET body='after';
        DO $$ BEGIN
            IF (SELECT array_agg(pk_value ORDER BY pk_value) FROM postvec.jobs WHERE registry_id=(SELECT id FROM postvec.registry WHERE table_name='pk_changes')) IS DISTINCT FROM ARRAY['11','12']
            THEN RAISE EXCEPTION 'BEFORE-trigger primary key change was lost'; END IF;
        END $$;
    "#).await?;
    for repeat in [
        "SELECT postvec.enable('pk_changes','body','fixture')",
        "SELECT postvec.enable('pk_changes','body','other',if_not_exists=>true)",
        "SELECT postvec.enable('pk_changes','body','fixture',distance=>'l2',if_not_exists=>true)",
    ] {
        let error = db
            .execute(repeat)
            .await
            .err()
            .context(format!("accepted: {repeat}"))?;
        ensure!(
            error.to_string().contains("already enabled"),
            "{repeat}: {error}"
        );
    }
    db.execute("SELECT postvec.disable('pk_changes','body')")
        .await?;
    db.execute(r#"
        CREATE TABLE partitioned(id integer PRIMARY KEY,body text,v vectors.vector(3)) PARTITION BY RANGE(id);
        CREATE TABLE partition_child PARTITION OF partitioned FOR VALUES FROM(0) TO(10);
        SELECT postvec.adopt('partitioned','body','v','fixture',sync=>false,backfill=>'none');
        ALTER TABLE partitioned ADD COLUMN new_v vectors.vector(3);
        INSERT INTO postvec.migrations(registry_id,old_model,new_model,old_dim,new_dim,strategy,new_column,rows_total,state)
            SELECT id,model,'next',3,3,'reembed','new_v',0,'awaiting_finalize' FROM postvec.registry WHERE table_name='partitioned';
        COMMENT ON COLUMN partition_child.v IS 'retain this';
    "#).await?;
    ensure!(db.execute("SELECT postvec.migration_finalize(id) FROM postvec.migrations WHERE new_column='new_v'").await.is_err(), "cutover discarded partition metadata");
    db.execute("COMMENT ON COLUMN partition_child.v IS NULL;SELECT postvec.migration_finalize(id) FROM postvec.migrations WHERE new_column='new_v'").await?;
    ensure!(
        sqlx::query_scalar::<_, Option<String>>(
            "SELECT space FROM postvec.registry WHERE table_name='partitioned'"
        )
        .fetch_one(&mut db)
        .await?
        .is_none(),
        "cutover retained the old space"
    );
    db.execute(r#"INSERT INTO postvec.models(name,model_type,target_model,target_dim,raw) VALUES
        ('hint','embed','unrelated',3,'{"extra":{"priority":1}}'),
        ('preferred','embed','hint',3,'{"extra":{"priority":2}}'),
        ('bad-priority','embed','bad-space',3,'{"extra":{"provider":"p","priority":1e50,"priority_explicit":"bad"}}')"#).await?;
    ensure!(
        sqlx::query_scalar::<_, String>("SELECT route FROM postvec._route('removed','hint')")
            .fetch_one(&mut db)
            .await?
            == "preferred",
        "hint resolved as a route"
    );
    ensure!(
        sqlx::query_scalar::<_, i32>(
            "SELECT priority FROM postvec.routes WHERE route='bad-priority'"
        )
        .fetch_one(&mut db)
        .await?
            == 200,
        "invalid priority did not fall back"
    );
    let mut other = PgConnection::connect(dsn).await?;
    let pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut other)
        .await?;
    let mut tx = db.begin().await?;
    tx.execute("LOCK TABLE partitioned IN ROW SHARE MODE;SELECT id FROM postvec.registry WHERE table_name='partitioned' FOR UPDATE").await?;
    let lifecycle = tokio::spawn(async move {
        other
            .execute("SELECT postvec.disable('partitioned','body')")
            .await
    });
    let waiting = tokio::time::timeout(std::time::Duration::from_secs(3), async {
        loop {
            let blocked: bool = sqlx::query_scalar("SELECT EXISTS(SELECT FROM pg_locks WHERE pid=$1 AND relation='partitioned'::regclass AND mode='AccessExclusiveLock' AND NOT granted)")
                .bind(pid).fetch_one(&mut *tx).await?;
            if blocked { return Ok::<_, sqlx::Error>(()); }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    }).await;
    tx.rollback().await?;
    lifecycle.await??;
    waiting.context("lifecycle waited on the registry before taking its DDL lock")??;
    db.execute("CREATE VIEW user_dependency AS SELECT * FROM postvec.registry")
        .await?;
    ensure!(
        managed::run(command(dsn, "uninstall")).await.is_err(),
        "external view cascaded"
    );
    let triggers: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM pg_trigger WHERE tgrelid='docs'::regclass AND NOT tgisinternal",
    )
    .fetch_one(&mut db)
    .await?;
    ensure!(triggers == 1, "failed uninstall did not roll back");
    db.execute("DROP VIEW user_dependency").await?;
    managed::run(command(dsn, "uninstall")).await?;
    let vectors: i64 = sqlx::query_scalar("SELECT count(*) FROM docs WHERE v IS NOT NULL")
        .fetch_one(&mut db)
        .await?;
    ensure!(vectors == 2, "uninstall removed vectors");
    ensure!(managed::run(command(dsn, "status")).await.is_err());
    db.close().await?;
    Ok(())
}

async fn table_contract(db: &mut PgConnection) -> Result<Vec<String>> {
    Ok(sqlx::query_scalar(r#"
        SELECT jsonb_build_array(c.relname,a.attname,a.attnum,format_type(a.atttypid,a.atttypmod),
            a.attnotnull,a.attidentity,pg_get_expr(d.adbin,d.adrelid))::text AS contract
        FROM pg_class c JOIN pg_namespace n ON n.oid=c.relnamespace
        JOIN pg_attribute a ON a.attrelid=c.oid AND a.attnum>0 AND NOT a.attisdropped
        LEFT JOIN pg_attrdef d ON d.adrelid=c.oid AND d.adnum=a.attnum
        WHERE n.nspname='postvec' AND c.relkind='r'
        UNION ALL
        SELECT jsonb_build_array(c.relname,con.conname,pg_get_constraintdef(con.oid))::text
        FROM pg_constraint con JOIN pg_class c ON c.oid=con.conrelid
        JOIN pg_namespace n ON n.oid=c.relnamespace WHERE n.nspname='postvec'
        UNION ALL
        SELECT jsonb_build_array(tablename,indexname,indexdef)::text FROM pg_indexes WHERE schemaname='postvec'
        ORDER BY 1
    "#).fetch_all(db).await?)
}
