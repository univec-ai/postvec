-- SPDX-License-Identifier: PostgreSQL
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

