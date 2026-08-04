-- SPDX-License-Identifier: PostgreSQL
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

CREATE TABLE postvec.schema_version (
    id boolean PRIMARY KEY DEFAULT true CHECK (id),
    version integer NOT NULL CHECK (version > 0),
    mode text NOT NULL CHECK (mode IN ('extension', 'managed'))
);
INSERT INTO postvec.schema_version (version, mode) VALUES (1, 'extension');
CREATE TABLE postvec.settings (
    key text PRIMARY KEY,
    value jsonb NOT NULL
);
GRANT SELECT ON postvec.schema_version, postvec.settings TO PUBLIC;
