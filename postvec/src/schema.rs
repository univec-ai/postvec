//! Extension SQL shipped with CREATE EXTENSION.
//!
//! The model cache (`postvec.models`, `postvec.worker_heartbeat`) and the
//! control-plane tables (`registry`, `jobs`, `jobs_dead`, `migrations`) plus
//! the shared TRUNCATE trigger. The four user-data tables are registered
//! with `pg_catalog.pg_extension_config_dump(...)` so `pg_dump` carries
//! their rows. `postvec.models` is a refreshable cache and is not dumped.

use pgrx::extension_sql;

extension_sql!(
    r#"
-- The generated enqueue triggers and the observability functions run as the
-- DML-issuing role (SECURITY INVOKER), so every role that can write to an
-- enabled table needs to reach into this schema.
GRANT USAGE ON SCHEMA postvec TO PUBLIC;

CREATE TABLE postvec.models (
    name          text PRIMARY KEY,          -- internal engine model name
    model_type    text NOT NULL,             -- embed|convert|embed-bridge|convert-bridge|legacy
    source_model  text,                      -- public source name (convert)
    target_model  text,                      -- public target name (embed/convert)
    source_dim    int,
    target_dim    int,
    sequence_len  int,
    raw           jsonb NOT NULL,            -- full HubModel snapshot
    last_seen     timestamptz NOT NULL DEFAULT now()
);

GRANT SELECT ON postvec.models TO PUBLIC;

CREATE TABLE postvec.worker_heartbeat (
    -- singleton key: the worker writes this row as one INSERT ... ON CONFLICT
    -- upsert. `id` never changes, so updates stay HOT-eligible.
    id             int PRIMARY KEY DEFAULT 1 CHECK (id = 1),
    pid            int,
    last_beat      timestamptz,
    started_at     timestamptz,
    jobs_done      bigint,
    errors         bigint,
    jobs_embedded  bigint NOT NULL DEFAULT 0,
    jobs_nulled    bigint NOT NULL DEFAULT 0,
    jobs_retried   bigint NOT NULL DEFAULT 0,
    jobs_dead      bigint NOT NULL DEFAULT 0,
    rows_converted bigint NOT NULL DEFAULT 0,  -- migration driver output
    rows_skipped   bigint NOT NULL DEFAULT 0,  -- migration driver skips
    model_refreshes bigint NOT NULL DEFAULT 0,
    last_error     text,
    -- Local refresh output: documents split and chunk rows created.
    documents_chunked bigint NOT NULL DEFAULT 0,
    chunks_created    bigint NOT NULL DEFAULT 0,
    -- When the model cache last completed a discovery refresh. The cache
    -- upsert is diff-aware (unchanged model rows are not rewritten), so
    -- models.last_seen alone no longer proves freshness — this does.
    models_refreshed_at timestamptz
);

GRANT SELECT ON postvec.worker_heartbeat TO PUBLIC;
"#,
    name = "postvec_models_table"
);

extension_sql!(
    r#"
-- One row per enabled (table, column). Dumpable so enable() state survives
-- pg_dump/restore.
CREATE TABLE postvec.registry (
    id            bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    table_schema  text NOT NULL,
    table_name    text NOT NULL,
    source_column text NOT NULL,
    vector_column text NOT NULL,
    -- Primary-key columns in index order. Single-column PKs are keyed as
    -- pk::text; composite PKs as ROW(a,b,...)::text (record text output is
    -- unambiguous and unique per key).
    pk_columns    text[] NOT NULL,
    pk_types      text[] NOT NULL,           -- cast targets for watermarks etc.
    model         text NOT NULL,             -- public model name (embed key)
    dim           int  NOT NULL,
    fts_config    regconfig NOT NULL DEFAULT 'pg_catalog.english',
    create_fts_index bool NOT NULL DEFAULT false,
    distance      text NOT NULL DEFAULT 'cosine'
                  CHECK (distance IN ('cosine','l2','ip')),
    -- 'none' is an adopted entry in observed mode: no DML enqueue triggers,
    -- only the TRUNCATE relation-identity sentinel.
    trigger_mode  text NOT NULL DEFAULT 'statement'
                  CHECK (trigger_mode IN ('statement','row','none')),
    -- Backfill of pre-existing rows: 'queue' enqueues everything at enable()
    -- time; 'cursor' lets the worker enqueue watermark-ordered chunks on
    -- large tables; 'none' skips; 'done' is the cursor terminal state.
    backfill_mode text NOT NULL DEFAULT 'none'
                  CHECK (backfill_mode IN ('none','queue','cursor','done')),
    backfill_watermark text,
    state         text NOT NULL DEFAULT 'active'
                  CHECK (state IN ('active','migrating','disabled')),
    created_at    timestamptz NOT NULL DEFAULT now(),
    -- Teardown may drop vector_column only when this is true. enable() (which
    -- created the column) inserts true; adopt() inserts false; migration
    -- finalization sets it true (the replacement column is postvec-built).
    owns_vector_column boolean NOT NULL DEFAULT true,
    -- Document-embedding template, stored exactly as validated. There is no
    -- canonical serializer: `$body` and `${body}` are bytewise different, and
    -- changing between them triggers the normal full refresh. NULL embeds
    -- the raw source column.
    format        text,
    -- Vector-index policy. 'manual' never builds implicitly. 'immediate' ran
    -- an ordinary CREATE INDEX at enable/adopt time (intent/audit only; no
    -- later reconciliation). 'auto' lets the worker run a blocking CREATE
    -- INDEX once the entry's work drains, and rebuilds later if the index
    -- is dropped.
    index_mode    text NOT NULL DEFAULT 'manual'
                  CHECK (index_mode IN ('manual', 'immediate', 'auto')),
    -- The last automatic-build failure. Set in a fresh transaction after a
    -- failed build; parks further auto attempts until an operator fixes the
    -- cause and runs create_vector_index() (which clears it on success).
    index_error   text,
    -- 'none' is one-row-one-vector column mode; 'recursive' is the 1:N
    -- chunked mode with a postvec-managed destination table.
    chunking      text NOT NULL DEFAULT 'none'
                  CHECK (chunking IN ('none','recursive')),
    chunk_size    integer,
    chunk_overlap integer,
    destination_schema text,
    destination_table  text,
    destination_view   text,
    -- Ownership proof for the managed destination: an unpredictable token
    -- written into exact comments on the destination table and view at
    -- enable() time. Destructive teardown requires the comment to carry this
    -- exact token (names are never ownership; OIDs do not survive
    -- pg_dump/restore — comments do). Collision protection, not a secret.
    destination_token  text,
    UNIQUE (table_schema, table_name, source_column),
    -- Column mode carries no chunk state; recursive mode carries all of it
    -- with valid geometry.
    CONSTRAINT registry_chunking_shape CHECK (
        (chunking = 'none'
             AND chunk_size IS NULL AND chunk_overlap IS NULL
             AND destination_schema IS NULL AND destination_table IS NULL
             AND destination_view IS NULL AND destination_token IS NULL)
        OR
        (chunking = 'recursive'
             AND chunk_size BETWEEN 64 AND 100000
             AND chunk_overlap >= 0 AND chunk_overlap < chunk_size
             AND destination_schema IS NOT NULL AND destination_table IS NOT NULL
             AND destination_view IS NOT NULL AND destination_token IS NOT NULL)
    )
);
GRANT SELECT ON postvec.registry TO PUBLIC;

-- One active writer per physical vector target is a database invariant, not
-- a preflight query: two concurrent adopt()/enable() calls must not
-- register two writers against the same vector column. The relation is the
-- vector target (source table in column mode, destination in recursive
-- mode), so a recursive entry protects its destination column instead of
-- forbidding an unrelated same-named column on the source. Disabled audit
-- rows are excluded so re-adoption stays possible.
CREATE UNIQUE INDEX registry_active_vector_target_key
    ON postvec.registry (
        COALESCE(destination_schema, table_schema),
        COALESCE(destination_table, table_name),
        vector_column
    )
    WHERE state <> 'disabled';

CREATE TABLE postvec.jobs (
    id           bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    registry_id  bigint NOT NULL REFERENCES postvec.registry(id) ON DELETE CASCADE,
    pk_value     text   NOT NULL,
    -- 'embed' is a source-row (column mode) or single-chunk (recursive)
    -- embedding; 'refresh' is the local split-and-replace of one document's
    -- chunk set. The op/chunk_id validity matrix is enforced at claim time.
    -- It cannot be a SQL constraint because the chunk target is a different
    -- per-entry relation.
    op           text   NOT NULL DEFAULT 'embed' CHECK (op IN ('embed','refresh')),
    -- Destination chunk identity for a recursive child embed job; NULL for
    -- column-mode embeds and refresh jobs.
    chunk_id     bigint CHECK (chunk_id IS NULL OR chunk_id > 0),
    attempts     int    NOT NULL DEFAULT 0,
    not_before   timestamptz NOT NULL DEFAULT now(),
    claimed_at   timestamptz,
    last_error   text,
    created_at   timestamptz NOT NULL DEFAULT now()
)
-- Queue-table autovacuum. Rows churn constantly (claim release and batch
-- deferral are DELETE + re-INSERT, the race-safe shape under the partial
-- unique index), so size-proportional defaults let dead tuples pile up on
-- a large but mostly pending queue. Threshold-led settings keep vacuum
-- cadence tied to churn instead.
WITH (
    autovacuum_vacuum_scale_factor = 0.05,
    autovacuum_vacuum_threshold = 1000,
    autovacuum_analyze_scale_factor = 0.05,
    autovacuum_analyze_threshold = 1000
);
-- At most one pending (unclaimed) job per (entry, op, row, chunk).
-- NULLS NOT DISTINCT (PG15+; postvec's floor is 16) treats the NULL
-- chunk_id of column-mode and refresh jobs as a real key, so one pending
-- job per row still holds while a chunked document's N child jobs coexist.
CREATE UNIQUE INDEX jobs_pending_dedup
    ON postvec.jobs (registry_id, op, pk_value, chunk_id)
    NULLS NOT DISTINCT
    WHERE claimed_at IS NULL;
-- exactly the claim queries' shapes: each claimant orders by (not_before,
-- id) over its own op only, so neither ever scans through the other's
-- backlog.
CREATE INDEX jobs_embed_claim_order
    ON postvec.jobs (not_before, id)
    WHERE claimed_at IS NULL AND op = 'embed';
CREATE INDEX jobs_refresh_claim_order
    ON postvec.jobs (not_before, id)
    WHERE claimed_at IS NULL AND op = 'refresh';
-- the stale-claim reclaim scan; only in-flight jobs live here, so it is tiny.
CREATE INDEX jobs_reclaim
    ON postvec.jobs (claimed_at) WHERE claimed_at IS NOT NULL;
-- per-entry-and-row lookups: the leading column serves status, purges, the
-- cursor queue-empty probe, and the registry FK cascade; the full key makes
-- the recursive triggers' synchronous per-document invalidation indexed
-- even with a deep queued backlog.
CREATE INDEX jobs_registry_pk
    ON postvec.jobs (registry_id, pk_value);
-- the bounded refresh-backpressure probe (count live child embed jobs,
-- capped): skips refresh rows entirely and includes claimed children.
CREATE INDEX jobs_live_embed_registry
    ON postvec.jobs (registry_id) WHERE op = 'embed';
GRANT SELECT ON postvec.jobs TO PUBLIC;
-- The generated triggers enqueue as the DML-issuing role: writers need INSERT
-- on exactly the two columns the triggers set (SELECT — granted above — covers
-- the dedup index's predicate column, which ON CONFLICT arbitration reads).
-- Deliberately NOT op or chunk_id: PUBLIC can only create the default
-- column-mode-looking embed job; claim-time validation dead-letters a
-- NULL-chunk embed job on a recursive entry as malformed.
GRANT INSERT (registry_id, pk_value) ON postvec.jobs TO PUBLIC;

-- Dead-letter queue. Its own identity PK (jobs.id is carried as job_id) so
-- rows stay stably addressable and a manual re-drive cannot collide.
CREATE TABLE postvec.jobs_dead (
    dead_id      bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    job_id       bigint NOT NULL,             -- id the job had in postvec.jobs
    registry_id  bigint NOT NULL,
    pk_value     text   NOT NULL,
    op           text   NOT NULL DEFAULT 'embed',
    chunk_id     bigint,
    attempts     int    NOT NULL DEFAULT 0,
    not_before   timestamptz,
    claimed_at   timestamptz,
    last_error   text,
    created_at   timestamptz,
    failed_at    timestamptz NOT NULL DEFAULT now()
);
-- Source DML also purges obsolete dead rows for the changed document; a full
-- jobs_dead scan inside every application write is not acceptable.
CREATE INDEX jobs_dead_registry_pk
    ON postvec.jobs_dead (registry_id, pk_value);
GRANT SELECT ON postvec.jobs_dead TO PUBLIC;

CREATE TABLE postvec.migrations (
    id           bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    registry_id  bigint NOT NULL REFERENCES postvec.registry(id) ON DELETE CASCADE,
    old_model    text NOT NULL,
    new_model    text NOT NULL,
    old_dim      int NOT NULL,
    new_dim      int NOT NULL,
    strategy     text NOT NULL CHECK (strategy IN ('convert','reembed','auto')),
    resolved_via jsonb,
    new_column   text NOT NULL,
    reindex      text NOT NULL DEFAULT 'manual' CHECK (reindex IN ('manual','blocking')),
    state        text NOT NULL DEFAULT 'running'
                 CHECK (state IN ('running','awaiting_finalize','awaiting_index',
                                  'done','aborted','failed')),
    rows_total   bigint NOT NULL,
    rows_done    bigint NOT NULL DEFAULT 0,
    -- Rows the driver deliberately left NULL and moved past (non-finite
    -- conversion output; NULL source text under strategy 'reembed').
    rows_skipped bigint NOT NULL DEFAULT 0,
    -- Transient-failure retry bookkeeping (same idiom as jobs.not_before):
    -- the worker skips a running migration until not_before; retry_failures
    -- drives the exponential backoff and resets on a successful batch.
    retry_failures int NOT NULL DEFAULT 0,
    not_before   timestamptz NOT NULL DEFAULT now(),
    last_pk      text,
    error        text,
    started_at   timestamptz NOT NULL DEFAULT now(),
    finished_at  timestamptz
);
GRANT SELECT ON postvec.migrations TO PUBLIC;

-- Shared TRUNCATE trigger function: registry id arrives via TG_ARGV[0].
-- Purges pending (unclaimed) jobs for the entry when its table is truncated.
-- SECURITY DEFINER (owned by the extension-installing superuser): the
-- truncating role must not need DELETE on postvec.jobs. search_path is pinned
-- as SECURITY DEFINER hygiene demands.
--
-- Deputy check: plpgsql functions are PUBLIC-executable by default, so any
-- table owner could attach this function to their own table with a victim's
-- registry id as TG_ARGV[0] and purge the victim's pending jobs by TRUNCATEing
-- their own table. The firing relation must therefore match the registry row
-- the argument names; anything else is ignored with a WARNING.
CREATE FUNCTION postvec.trg_truncate() RETURNS trigger LANGUAGE plpgsql
SECURITY DEFINER SET search_path = pg_catalog AS $body$
DECLARE
    r record;
BEGIN
    SELECT reg.chunking, reg.destination_schema, reg.destination_table
      INTO r
      FROM postvec.registry reg
     WHERE reg.id = TG_ARGV[0]::bigint
       AND reg.table_schema = TG_TABLE_SCHEMA::text
       AND reg.table_name = TG_TABLE_NAME::text;
    IF NOT FOUND THEN
        RAISE WARNING 'postvec: trg_truncate(%) fired on %.%, which is not that registry entry''s table; ignoring',
            TG_ARGV[0], TG_TABLE_SCHEMA, TG_TABLE_NAME;
        RETURN NULL;
    END IF;
    IF r.chunking = 'recursive' THEN
        -- A truncated source has no rows, so every chunk, queued job and
        -- dead letter for the entry is obsolete. Claimed refresh rows are
        -- skipped: the refresh worker holds a lock on that row in the one
        -- transaction that processes it while waiting FOR SHARE on the
        -- source. Deleting it here would deadlock; it self-resolves by
        -- finding the source row gone. Claimed child embeds hold no lock
        -- (the claim transaction committed before inference) and their
        -- write-back is a chunk-keyed obsolete no-op either way.
        EXECUTE pg_catalog.format('DELETE FROM %I.%I',
                                  r.destination_schema, r.destination_table);
        DELETE FROM postvec.jobs
         WHERE registry_id = TG_ARGV[0]::bigint
           AND NOT (op = 'refresh' AND claimed_at IS NOT NULL);
        DELETE FROM postvec.jobs_dead
         WHERE registry_id = TG_ARGV[0]::bigint;
    ELSE
        DELETE FROM postvec.jobs
         WHERE registry_id = TG_ARGV[0]::bigint
           AND claimed_at IS NULL;
    END IF;
    RETURN NULL;
END $body$;

SELECT pg_catalog.pg_extension_config_dump('postvec.registry', '');
SELECT pg_catalog.pg_extension_config_dump('postvec.jobs', '');
SELECT pg_catalog.pg_extension_config_dump('postvec.jobs_dead', '');
SELECT pg_catalog.pg_extension_config_dump('postvec.migrations', '');
SELECT pg_catalog.pg_extension_config_dump(pg_catalog.pg_get_serial_sequence('postvec.registry', 'id')::regclass, '');
SELECT pg_catalog.pg_extension_config_dump(pg_catalog.pg_get_serial_sequence('postvec.jobs', 'id')::regclass, '');
SELECT pg_catalog.pg_extension_config_dump(pg_catalog.pg_get_serial_sequence('postvec.jobs_dead', 'dead_id')::regclass, '');
SELECT pg_catalog.pg_extension_config_dump(pg_catalog.pg_get_serial_sequence('postvec.migrations', 'id')::regclass, '');
"#,
    name = "postvec_control_tables",
    requires = ["postvec_models_table"]
);

extension_sql!(
    r#"
-- The four shared recursive-entry DML trigger functions. Installed by
-- CREATE EXTENSION, so they are owned by the extension installer. A
-- chunked invalidation needs DELETE on the managed destination, DELETE
-- on postvec.jobs/jobs_dead, and INSERT of an `op` column PUBLIC has no
-- grant on, none of which an application writer holds. The registry id
-- arrives in TG_ARGV[0]; per-entry identity (destination, PK/source
-- columns) is read from postvec.registry inside the pinned pg_catalog
-- search path, and every identifier goes through quote_ident /
-- format('%I'). Per-entry DDL is only CREATE TRIGGER.
--
-- Confused-deputy check (same as trg_truncate): the firing relation must
-- be the recursive registry row's source relation, else WARNING + RETURN
-- NULL. Without it any table owner could attach a function to their own
-- table with a victim's registry id and purge the victim's chunks and
-- jobs.
--
-- The UPDATE function's statement-mode change set arrives as TG_ARGV[1..]
-- (referenced column names, regenerated by set_format()). Each is
-- rendered through format('%I'), so the arguments can only select which
-- columns are compared, never inject SQL. Attaching triggers requires
-- TRIGGER privilege on the table, which is already table-owner trust.
--
-- Job hygiene inside the invalidation is asymmetric:
--   - child embed jobs (op='embed') for the document are deleted, pending
--     or claimed. A claimed child holds no row lock (its claim transaction
--     committed before inference) and its write-back is a chunk-keyed
--     obsolete no-op either way.
--   - a pending refresh job is left for the dedup index to coalesce with
--     the one this invalidation enqueues. Deleting and re-inserting the
--     same pending-dedup key inside one statement would race its own
--     ON CONFLICT arbitration (the deleted row is still index-visible to
--     the command).
--   - a claimed refresh job is never touched: the refresh worker holds
--     that row's lock inside the one transaction that processes it while
--     it waits FOR SHARE on the source row this trigger's statement has
--     locked. Deleting it here would deadlock. It self-resolves: the
--     refresh re-reads the committed source state.
--   - dead rows for the document are deleted wholesale (their input is
--     superseded), except on plain content updates where the enqueued
--     refresh replaces them anyway.

CREATE FUNCTION postvec.trg_chunk_ins() RETURNS trigger LANGUAGE plpgsql
SECURITY DEFINER SET search_path = pg_catalog AS $body$
DECLARE
    r record;
    rid bigint := TG_ARGV[0]::bigint;
BEGIN
    SELECT reg.source_column, reg.pk_columns[1] AS pk_col
      INTO r
      FROM postvec.registry reg
     WHERE reg.id = rid
       AND reg.chunking = 'recursive'
       AND (
            (reg.table_schema = TG_TABLE_SCHEMA::text
                 AND reg.table_name = TG_TABLE_NAME::text)
            -- Row triggers are cloned to partitions, where the firing
            -- relation is the partition itself; any relation inside the
            -- registered source's partition tree is that source. Attaching
            -- a partition requires ownership of both sides, so this widens
            -- nothing an attacker can reach.
            OR EXISTS (
                SELECT 1 FROM pg_catalog.pg_partition_tree(
                    pg_catalog.to_regclass(
                        pg_catalog.quote_ident(reg.table_schema) || '.' ||
                        pg_catalog.quote_ident(reg.table_name))) pt
                 WHERE pt.relid = TG_RELID)
       );
    IF NOT FOUND THEN
        RAISE WARNING 'postvec: trg_chunk_ins(%) fired on %.%, which is not that recursive registry entry''s table; ignoring',
            TG_ARGV[0], TG_TABLE_SCHEMA, TG_TABLE_NAME;
        RETURN NULL;
    END IF;
    IF TG_LEVEL = 'STATEMENT' THEN
        EXECUTE pg_catalog.format(
            'INSERT INTO postvec.jobs (registry_id, pk_value, op)
             SELECT $1, n.%1$I::text, ''refresh'' FROM new_table n
              WHERE n.%2$I IS NOT NULL
             ON CONFLICT (registry_id, op, pk_value, chunk_id)
             WHERE claimed_at IS NULL DO NOTHING',
            r.pk_col, r.source_column) USING rid;
    ELSE
        EXECUTE pg_catalog.format(
            'INSERT INTO postvec.jobs (registry_id, pk_value, op)
             SELECT $1, ($2).%1$I::text, ''refresh''
              WHERE ($2).%2$I IS NOT NULL
             ON CONFLICT (registry_id, op, pk_value, chunk_id)
             WHERE claimed_at IS NULL DO NOTHING',
            r.pk_col, r.source_column) USING rid, NEW;
    END IF;
    PERFORM postvec.worker_kick();
    RETURN NULL;
END $body$;

CREATE FUNCTION postvec.trg_chunk_upd() RETURNS trigger LANGUAGE plpgsql
SECURITY DEFINER SET search_path = pg_catalog AS $body$
DECLARE
    r record;
    rid bigint := TG_ARGV[0]::bigint;
    pred text := '';
    i int;
BEGIN
    SELECT reg.source_column, reg.pk_columns[1] AS pk_col,
           pg_catalog.quote_ident(reg.destination_schema) || '.' ||
           pg_catalog.quote_ident(reg.destination_table) AS qdest
      INTO r
      FROM postvec.registry reg
     WHERE reg.id = rid
       AND reg.chunking = 'recursive'
       AND (
            (reg.table_schema = TG_TABLE_SCHEMA::text
                 AND reg.table_name = TG_TABLE_NAME::text)
            -- Row triggers are cloned to partitions, where the firing
            -- relation is the partition itself; any relation inside the
            -- registered source's partition tree is that source. Attaching
            -- a partition requires ownership of both sides, so this widens
            -- nothing an attacker can reach.
            OR EXISTS (
                SELECT 1 FROM pg_catalog.pg_partition_tree(
                    pg_catalog.to_regclass(
                        pg_catalog.quote_ident(reg.table_schema) || '.' ||
                        pg_catalog.quote_ident(reg.table_name))) pt
                 WHERE pt.relid = TG_RELID)
       );
    IF NOT FOUND THEN
        RAISE WARNING 'postvec: trg_chunk_upd(%) fired on %.%, which is not that recursive registry entry''s table; ignoring',
            TG_ARGV[0], TG_TABLE_SCHEMA, TG_TABLE_NAME;
        RETURN NULL;
    END IF;
    IF TG_LEVEL = 'STATEMENT' THEN
        IF TG_NARGS < 2 THEN
            pred := pg_catalog.format('n.%1$I IS DISTINCT FROM o.%1$I', r.source_column);
        ELSE
            FOR i IN 1 .. TG_NARGS - 1 LOOP
                IF i > 1 THEN pred := pred || ' OR '; END IF;
                pred := pred || pg_catalog.format('n.%1$I IS DISTINCT FROM o.%1$I', TG_ARGV[i]);
            END LOOP;
        END IF;
        EXECUTE pg_catalog.format(
            'WITH changed AS (
                 SELECT n.%1$I AS pk, n.%2$I AS src
                   FROM new_table n JOIN old_table o ON o.%1$I = n.%1$I
                  WHERE %4$s
             ), dc AS (
                 DELETE FROM %3$s c USING changed x
                  WHERE c.postvec_source_pk = x.pk
             ), dj AS (
                 DELETE FROM postvec.jobs j USING changed x
                  WHERE j.registry_id = $1 AND j.op = ''embed''
                    AND j.pk_value = x.pk::text
             ), dd AS (
                 DELETE FROM postvec.jobs_dead j USING changed x
                  WHERE j.registry_id = $1 AND j.pk_value = x.pk::text
             )
             INSERT INTO postvec.jobs (registry_id, pk_value, op)
             SELECT $1, x.pk::text, ''refresh'' FROM changed x
              WHERE x.src IS NOT NULL
             ON CONFLICT (registry_id, op, pk_value, chunk_id)
             WHERE claimed_at IS NULL DO NOTHING',
            r.pk_col, r.source_column, r.qdest, pred) USING rid;
    ELSE
        -- Row mode: the trigger's OF-list/WHEN guard already restricted this
        -- firing to real referenced-column changes.
        EXECUTE pg_catalog.format(
            'WITH dc AS (
                 DELETE FROM %3$s c WHERE c.postvec_source_pk = ($2).%1$I
             ), dj AS (
                 DELETE FROM postvec.jobs j
                  WHERE j.registry_id = $1 AND j.op = ''embed''
                    AND j.pk_value = ($2).%1$I::text
             ), dd AS (
                 DELETE FROM postvec.jobs_dead j
                  WHERE j.registry_id = $1 AND j.pk_value = ($2).%1$I::text
             )
             INSERT INTO postvec.jobs (registry_id, pk_value, op)
             SELECT $1, ($3).%1$I::text, ''refresh''
              WHERE ($3).%2$I IS NOT NULL
             ON CONFLICT (registry_id, op, pk_value, chunk_id)
             WHERE claimed_at IS NULL DO NOTHING',
            r.pk_col, r.source_column, r.qdest) USING rid, OLD, NEW;
    END IF;
    PERFORM postvec.worker_kick();
    RETURN NULL;
END $body$;

-- PK-change companion: always row-level (both trigger modes), fired WHEN the
-- PK actually changed. The OLD identity's whole chunk set and queue state are
-- obsolete (including any pending refresh keyed by the old rendering — its
-- key differs from the NEW-keyed insert, so deleting it is arbitration-safe);
-- the NEW identity gets one refresh when the source is non-NULL.
CREATE FUNCTION postvec.trg_chunk_pk() RETURNS trigger LANGUAGE plpgsql
SECURITY DEFINER SET search_path = pg_catalog AS $body$
DECLARE
    r record;
    rid bigint := TG_ARGV[0]::bigint;
BEGIN
    SELECT reg.source_column, reg.pk_columns[1] AS pk_col,
           pg_catalog.quote_ident(reg.destination_schema) || '.' ||
           pg_catalog.quote_ident(reg.destination_table) AS qdest
      INTO r
      FROM postvec.registry reg
     WHERE reg.id = rid
       AND reg.chunking = 'recursive'
       AND (
            (reg.table_schema = TG_TABLE_SCHEMA::text
                 AND reg.table_name = TG_TABLE_NAME::text)
            -- Row triggers are cloned to partitions, where the firing
            -- relation is the partition itself; any relation inside the
            -- registered source's partition tree is that source. Attaching
            -- a partition requires ownership of both sides, so this widens
            -- nothing an attacker can reach.
            OR EXISTS (
                SELECT 1 FROM pg_catalog.pg_partition_tree(
                    pg_catalog.to_regclass(
                        pg_catalog.quote_ident(reg.table_schema) || '.' ||
                        pg_catalog.quote_ident(reg.table_name))) pt
                 WHERE pt.relid = TG_RELID)
       );
    IF NOT FOUND THEN
        RAISE WARNING 'postvec: trg_chunk_pk(%) fired on %.%, which is not that recursive registry entry''s table; ignoring',
            TG_ARGV[0], TG_TABLE_SCHEMA, TG_TABLE_NAME;
        RETURN NULL;
    END IF;
    EXECUTE pg_catalog.format(
        'WITH dc AS (
             DELETE FROM %3$s c WHERE c.postvec_source_pk = ($2).%1$I
         ), dj AS (
             DELETE FROM postvec.jobs j
              WHERE j.registry_id = $1 AND j.pk_value = ($2).%1$I::text
                AND NOT (j.op = ''refresh'' AND j.claimed_at IS NOT NULL)
         ), dd AS (
             DELETE FROM postvec.jobs_dead j
              WHERE j.registry_id = $1 AND j.pk_value = ($2).%1$I::text
         )
         INSERT INTO postvec.jobs (registry_id, pk_value, op)
         SELECT $1, ($3).%1$I::text, ''refresh''
          WHERE ($3).%2$I IS NOT NULL
         ON CONFLICT (registry_id, op, pk_value, chunk_id)
         WHERE claimed_at IS NULL DO NOTHING',
        r.pk_col, r.source_column, r.qdest) USING rid, OLD, NEW;
    PERFORM postvec.worker_kick();
    RETURN NULL;
END $body$;

CREATE FUNCTION postvec.trg_chunk_del() RETURNS trigger LANGUAGE plpgsql
SECURITY DEFINER SET search_path = pg_catalog AS $body$
DECLARE
    r record;
    rid bigint := TG_ARGV[0]::bigint;
BEGIN
    SELECT reg.pk_columns[1] AS pk_col,
           pg_catalog.quote_ident(reg.destination_schema) || '.' ||
           pg_catalog.quote_ident(reg.destination_table) AS qdest
      INTO r
      FROM postvec.registry reg
     WHERE reg.id = rid
       AND reg.chunking = 'recursive'
       AND (
            (reg.table_schema = TG_TABLE_SCHEMA::text
                 AND reg.table_name = TG_TABLE_NAME::text)
            -- Row triggers are cloned to partitions, where the firing
            -- relation is the partition itself; any relation inside the
            -- registered source's partition tree is that source. Attaching
            -- a partition requires ownership of both sides, so this widens
            -- nothing an attacker can reach.
            OR EXISTS (
                SELECT 1 FROM pg_catalog.pg_partition_tree(
                    pg_catalog.to_regclass(
                        pg_catalog.quote_ident(reg.table_schema) || '.' ||
                        pg_catalog.quote_ident(reg.table_name))) pt
                 WHERE pt.relid = TG_RELID)
       );
    IF NOT FOUND THEN
        RAISE WARNING 'postvec: trg_chunk_del(%) fired on %.%, which is not that recursive registry entry''s table; ignoring',
            TG_ARGV[0], TG_TABLE_SCHEMA, TG_TABLE_NAME;
        RETURN NULL;
    END IF;
    IF TG_LEVEL = 'STATEMENT' THEN
        EXECUTE pg_catalog.format(
            'WITH gone AS (
                 SELECT o.%1$I AS pk FROM old_table o
             ), dc AS (
                 DELETE FROM %2$s c USING gone x
                  WHERE c.postvec_source_pk = x.pk
             ), dj AS (
                 DELETE FROM postvec.jobs j USING gone x
                  WHERE j.registry_id = $1 AND j.pk_value = x.pk::text
                    AND NOT (j.op = ''refresh'' AND j.claimed_at IS NOT NULL)
             )
             DELETE FROM postvec.jobs_dead j USING gone x
              WHERE j.registry_id = $1 AND j.pk_value = x.pk::text',
            r.pk_col, r.qdest) USING rid;
    ELSE
        EXECUTE pg_catalog.format(
            'WITH dc AS (
                 DELETE FROM %2$s c WHERE c.postvec_source_pk = ($2).%1$I
             ), dj AS (
                 DELETE FROM postvec.jobs j
                  WHERE j.registry_id = $1 AND j.pk_value = ($2).%1$I::text
                    AND NOT (j.op = ''refresh'' AND j.claimed_at IS NOT NULL)
             )
             DELETE FROM postvec.jobs_dead j
              WHERE j.registry_id = $1 AND j.pk_value = ($2).%1$I::text',
            r.pk_col, r.qdest) USING rid, OLD;
    END IF;
    RETURN NULL;
END $body$;

-- EXECUTE stays granted (explicitly, not via default privileges):
-- PostgreSQL requires the role issuing CREATE TRIGGER (the table owner
-- inside a non-superuser enable()) to hold EXECUTE on the trigger
-- function. A trigger-returning function cannot be called as an ordinary
-- function, and the firing-relation/registry deputy check above is the
-- security boundary.
GRANT EXECUTE ON FUNCTION postvec.trg_chunk_ins() TO PUBLIC;
GRANT EXECUTE ON FUNCTION postvec.trg_chunk_upd() TO PUBLIC;
GRANT EXECUTE ON FUNCTION postvec.trg_chunk_pk() TO PUBLIC;
GRANT EXECUTE ON FUNCTION postvec.trg_chunk_del() TO PUBLIC;
"#,
    name = "postvec_chunk_triggers",
    requires = ["postvec_control_tables"]
);

#[cfg(any(test, feature = "pg_test"))]
#[pgrx::pg_schema]
mod tests {
    use pgrx::prelude::*;

    #[pg_test]
    fn control_tables_exist() {
        for t in [
            "postvec.registry",
            "postvec.jobs",
            "postvec.jobs_dead",
            "postvec.migrations",
            "postvec.models",
            "postvec.worker_heartbeat",
        ] {
            let oid =
                Spi::get_one_with_args::<pg_sys::Oid>("SELECT to_regclass($1)::oid", &[t.into()])
                    .unwrap();
            assert!(
                oid.is_some() && oid.unwrap() != pg_sys::Oid::INVALID,
                "{t} missing"
            );
        }
    }

    #[pg_test]
    fn config_dump_includes_control_tables_and_sequences() {
        let count = Spi::get_one::<i64>(
            "SELECT count(*)
               FROM pg_extension e
               CROSS JOIN LATERAL unnest(e.extconfig) cfg(oid)
               JOIN pg_class c ON c.oid = cfg.oid
              WHERE e.extname = 'postvec'
                AND c.relname IN (
                    'registry', 'jobs', 'jobs_dead', 'migrations',
                    'registry_id_seq', 'jobs_id_seq',
                    'jobs_dead_dead_id_seq', 'migrations_id_seq'
                )",
        )
        .unwrap();
        assert_eq!(count, Some(8));
    }

    /// Exact queue/dead index inventory. Claim, probe and invalidation
    /// queries are written against these shapes; a dropped or renamed one
    /// degrades them to sequential scans.
    #[pg_test]
    fn queue_and_dead_indexes_have_the_specified_shapes() {
        for (index, must_contain) in [
            (
                "jobs_pending_dedup",
                vec![
                    "UNIQUE",
                    "registry_id, op, pk_value, chunk_id",
                    "NULLS NOT DISTINCT",
                    "claimed_at IS NULL",
                ],
            ),
            (
                "jobs_embed_claim_order",
                vec!["not_before, id", "claimed_at IS NULL", "op = 'embed'"],
            ),
            (
                "jobs_refresh_claim_order",
                vec!["not_before, id", "claimed_at IS NULL", "op = 'refresh'"],
            ),
            ("jobs_reclaim", vec!["claimed_at IS NOT NULL"]),
            ("jobs_registry_pk", vec!["registry_id, pk_value"]),
            (
                "jobs_live_embed_registry",
                vec!["(registry_id)", "op = 'embed'"],
            ),
            ("jobs_dead_registry_pk", vec!["registry_id, pk_value"]),
        ] {
            let def = Spi::get_one_with_args::<String>(
                "SELECT pg_get_indexdef(to_regclass('postvec.' || $1))",
                &[index.into()],
            )
            .unwrap()
            .unwrap_or_default();
            for frag in must_contain {
                assert!(def.contains(frag), "{index} must contain {frag:?}: {def}");
            }
        }
        // No superseded index should survive.
        for gone in ["jobs_claim_order", "jobs_registry"] {
            assert_eq!(
                Spi::get_one_with_args::<bool>(
                    "SELECT to_regclass('postvec.' || $1) IS NULL",
                    &[gone.into()],
                )
                .unwrap(),
                Some(true),
                "superseded index {gone} must be gone"
            );
        }
    }

    /// The partial unique index collapses repeated pending jobs for one row
    /// into a single entry, while allowing a fresh job once the prior one is
    /// claimed (claimed_at IS NOT NULL leaves the predicate).
    #[pg_test]
    fn dedup_index_coalesces_pending() {
        Spi::run(
            "INSERT INTO postvec.registry
               (table_schema, table_name, source_column, vector_column,
                pk_columns, pk_types, model, dim)
             VALUES ('public','t','body','body_semantic',
                     ARRAY['id'], ARRAY['bigint'], 'm', 4)",
        )
        .unwrap();
        let rid = Spi::get_one::<i64>("SELECT id FROM postvec.registry LIMIT 1")
            .unwrap()
            .unwrap();

        let ins = format!(
            "INSERT INTO postvec.jobs (registry_id, pk_value) VALUES ({rid}, '7')
             ON CONFLICT (registry_id, op, pk_value, chunk_id) WHERE claimed_at IS NULL DO NOTHING"
        );
        Spi::run(&ins).unwrap();
        Spi::run(&ins).unwrap();
        Spi::run(&ins).unwrap();
        let pending =
            Spi::get_one::<i64>("SELECT count(*) FROM postvec.jobs WHERE claimed_at IS NULL")
                .unwrap();
        assert_eq!(
            pending,
            Some(1),
            "three pending inserts must coalesce to one"
        );

        // Claim it; a new pending job for the same row is now allowed.
        Spi::run("UPDATE postvec.jobs SET claimed_at = now()").unwrap();
        Spi::run(&ins).unwrap();
        let total = Spi::get_one::<i64>("SELECT count(*) FROM postvec.jobs").unwrap();
        assert_eq!(
            total,
            Some(2),
            "a fresh job is allowed once the prior is claimed"
        );
    }
}
