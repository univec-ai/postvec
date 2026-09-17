# postvec

A PostgreSQL extension (Rust + [pgrx](https://github.com/pgcentralfoundation/pgrx)).
Declare a text column semantic and postvec keeps a shadow
[`pgvector`](https://github.com/pgvector/pgvector) column in sync as rows
change, serves hybrid full-text and vector search in one call, and converts
stored vectors between embedding models in place.

`postvec.mode` defaults to **embedded**: the engine runs inside the postvec
launcher process. Published packages compile that feature in. Remote mode
(`postvec.mode = 'grpc'`) is a thin gRPC client to [postvec-server](../postvec-server)
nodes (`EmbedTexts` / `ConvertEmbeddings`) on a trusted private network.

Public documentation: [postvec.dev/docs](https://postvec.dev/docs/)
([SQL reference](https://postvec.dev/docs/reference/sql)).

Shipped: `enable` / `disable`, the trigger -> queue -> background-worker sync
loop, queue and cursor backfill, hybrid `search`, `status` / `stats`,
`migrate()` (in-database model conversion, with finalize/abort), `adopt()`
(take over an existing populated pgvector column), composite primary keys,
partitioned tables, per-database workers and trigger latch-kick. Embedded
mode is compiled into every published package. A plain `cargo` build without
`--features embedded` is the thin gRPC client only.

## Requirements

- PostgreSQL **16 / 17 / 18** with `postvec` in `shared_preload_libraries`
  (the background worker registers at postmaster start).
- **pgvector >= 0.8** in the same database (`CREATE EXTENSION postvec CASCADE`
  pulls it in).
- Embedded mode: engine root at `postvec.path` (default `/opt/postvec`).
  Remote mode: a reachable postvec-server (gRPC `:33333`, HTTP `:22222`).

## Quickstart

**One container.** PostgreSQL, pgvector, postvec and an embedding model in a
single image:

```bash
docker run -d --name postvec \
  -e POSTGRES_PASSWORD=change-me -e POSTGRES_DB=app \
  -v postvec-data:/var/lib/postgresql \
  -p 127.0.0.1:5432:5432 \
  ghcr.io/univec-ai/postvec:0.1.0-1-pg18-local
```

```sql
CREATE TABLE docs (id bigserial PRIMARY KEY, body text);
SELECT postvec.enable('docs', 'body',
                      model => 'sentence-transformers-all-minilm-l6-v2',
                      create_fts_index => true);
INSERT INTO docs (body) VALUES
  ('migrating embedding models normally requires re-embedding all source text'),
  ('the office plants need watering twice a week');

-- vectors fill asynchronously; then:
SELECT d.body FROM postvec.search('docs', 'body',
         'switching AI models without redoing the work') s
  JOIN docs d ON d.id = s.pk_value::bigint ORDER BY s.rrf_score DESC;
```

Mount `/var/lib/postgresql/data` for the PG 16 and 17 images; 18 uses
`/var/lib/postgresql`. The `:0.1.0-1-pg18-remote` tag is the same layout
pointed at remote inference nodes. The tag is
`<version>-<packaging revision>`; `pg18-local` / `pg18-remote` are the
moving equivalents.

**Packages, against your own cluster:**

```bash
sudo apt install ./postvec-cli_*.deb ./postgresql-18-postvec_*.deb
sudo postvec setup --database app --grpc 192.0.2.2:33333 --http https://192.0.2.2:22222
postvec doctor --database app
```

**From source:** [postvec.dev/docs/install/source](https://postvec.dev/docs/install/source).

Package and image reference:
[install packages](https://postvec.dev/docs/install/packages).
How artifacts are built: [`packaging/postvec/`](../packaging/postvec/README.md).

## Install and configure

Once the package assets are on the host, one command does the install:
database, extension, configuration, restart and verification, using the
[`postvec` CLI](../postvec-cli):

```bash
sudo postvec setup --database univec \
     --grpc 192.0.2.2:33333 --http https://192.0.2.2:22222
postvec doctor
```

It merges `shared_preload_libraries`, owns exactly one `conf.d` snippet,
creates the database before activating the launcher, and proves the worker
heartbeat advances before reporting success. `--dry-run` shows the plan.

The manual equivalent, for a cluster you configure by hand:

```sql
CREATE EXTENSION postvec CASCADE;         -- brings in `vector`
```

```conf
shared_preload_libraries = 'postvec'      # requires a restart
postvec.database                = 'univec'                 # one or more, comma-separated
postvec.grpc_endpoints = '192.0.2.2:33333'       # remote mode; comma-separated, round-robin
postvec.http_endpoints = 'https://192.0.2.2:22222'  # /config discovery
```

```sql
SELECT pg_reload_conf();
SELECT postvec.refresh_models();          -- populate the model cache
```

### GUCs

SIGHUP values apply at the next worker start (`ALTER SYSTEM` is enough).
`shared_preload_libraries` still needs a restart. Full list:
[GUC reference](https://postvec.dev/docs/reference/gucs).

| GUC | Default | Purpose |
|---|---|---|
| `postvec.mode` | `embedded` | `embedded` (in-process engine) or `grpc` (remote nodes) |
| `postvec.path` | `/opt/postvec` | Embedded: engine root (`libs/`, `models/`) |
| `postvec.database` | `''` | Comma-separated database(s); one worker per name |
| `postvec.grpc_endpoints` | `''` | Remote: `host:33333` list (round-robin) |
| `postvec.http_endpoints` | `''` | Remote: `http(s)://host:22222` list for `/config` |
| `postvec.providers_path` | `/etc/postvec/providers.d` | Connector files for hosted embedding APIs |
| `postvec.worker_enabled` | `on` | Pause or resume job processing |
| `postvec.poll_interval_ms` | `5000` | Idle poll; writers wake the worker at commit |
| `postvec.batch_size` | `64` | Texts per `EmbedTexts` call |
| `postvec.migrate_batch_size` | `256` | Vectors per `ConvertEmbeddings` |
| `postvec.embed_timeout_ms` | `30000` | Worker gRPC deadline |
| `postvec.query_timeout_ms` | `2000` | `search()` / `embed()` inline deadline |
| `postvec.max_retries` | `5` | Before a job moves to `jobs_dead` |
| `postvec.search_degrade_to_fts` | `on` | FTS-only fallback when inference is down |
| `postvec.embedded_models` | `''` | Embedded: models to preload; empty = every enabled model on disk |
| `postvec.embedded_listen` | `127.0.0.1:33433` | Embedded: loopback gRPC |
| `postvec.embedded_http_listen` | `127.0.0.1:33434` | Embedded: loopback `GET /config` |

## The SQL surface

```sql
-- 1. Make docs.body semantic. Adds body_semantic vector(N), triggers, backfill.
--    Options: distance => 'cosine'|'l2'|'ip', trigger_mode => 'statement'|'row',
--    create_fts_index => bool, backfill => bool,
--    index_mode => 'manual'|'immediate'|'auto',
--    format => an embedding template.
SELECT postvec.enable('public.docs', 'body', model => 'baai-bge-m3');
-- Build the vector index after the backfill drains (or declare
-- index_mode => 'auto' to let the worker build it once work drains, or
-- 'immediate' for a synchronous build at enable time - both are ordinary
-- blocking CREATE INDEX; keep 'manual' + CREATE INDEX CONCURRENTLY for
-- large or write-heavy tables):
SELECT postvec.create_vector_index('public.docs', 'body');

-- 2. Write normally. The shadow column fills asynchronously.
INSERT INTO docs (body) VALUES ('quarterly revenue guidance was raised');
UPDATE docs SET body = 'revised full-year guidance' WHERE id = 1;

-- 3. Hybrid search (FTS + vector, RRF-fused; query auto-embedded).
SELECT d.*, s.rrf_score
  FROM postvec.search('docs', 'body', 'guidance for the quarter', limit_n => 20) s
  JOIN docs d ON d.id = s.pk_value::bigint
 ORDER BY s.rrf_score DESC;

-- 3b. Typed metadata filters run inside both search legs, before ranking and
--     LIMIT. Values are bind parameters. AND-only; operators:
--     neq/gt/gte/lt/lte/in/like/ilike/is_not; null means IS NULL.
SELECT * FROM postvec.search('docs', 'body', 'quarterly guidance',
         filter => '{"category": "finance",
                     "published_at": {"gte": "2026-01-01"},
                     "region": ["EU", "UK"],
                     "archived_at": null}'::jsonb);

-- 3c. Embedding templates give short texts their document context. The
--     source column ($body here) anchors the lifecycle: NULL source = NULL
--     vector; changed context columns re-enqueue. Note the E-string: the
--     template itself does no backslash processing.
SELECT postvec.enable('public.articles', 'body', model => 'baai-bge-m3',
                      format => E'$title - $author\n\n$body');
-- Changing or clearing the template is an explicit, atomic full re-embed:
SELECT postvec.set_format('public.articles', 'body', E'$title\n\n$body');
SELECT postvec.set_format('public.articles', 'body', NULL);

-- 4. Observe the queue / worker.
SELECT * FROM postvec.status();

-- 5. Already have a populated pgvector column? Adopt it. postvec takes over
--    sync and backfills only the gaps. The declared vector(N) is
--    authoritative; `model` is your assertion.
SELECT postvec.adopt('public.legacy_docs', 'body',
                     vector_column => 'embedding', model => 'baai-bge-m3');

-- 5b. The same path searches a retired space. The adopted model can be a
--     converter target: search() embeds the query with the converter's source
--     model and converts it into the stored space via embed-bridge - one RPC.
--     backfill => 'none' keeps every existing vector. Default sync => true
--     keeps future writes current through the same bridge.
SELECT postvec.adopt('public.ada_docs', 'body', vector_column => 'embedding',
                     model => 'openai-text-embedding-ada-002',
                     backfill => 'none');
SELECT * FROM postvec.search('public.ada_docs', 'body', 'embedding migration risk');
--     For a frozen or NOT NULL legacy column, use
--     sync => false, backfill => 'none'.
--     A direct embed model wins over the bridge. migrate() later, when you
--     want to leave the deprecated space.

-- 6. Turn an adopted example off (keeps the column unless drop_column => true;
--    an adopted column is retained).
SELECT postvec.disable('legacy_docs', 'body');

-- 7. Documents bigger than one embedding: recursive chunking. One row becomes
--    many chunk vectors in a postvec-managed destination table with a join
--    view, FORCE RLS keyed on source visibility, and ownership-marker
--    comments. search() then returns one row per document plus the winning
--    chunk's seq/offsets/text. migrate() converts the chunk vectors in place.
SELECT postvec.enable('public.articles', 'body', model => 'baai-bge-m3',
                      chunking      => 'recursive',
                      chunk_size    => 2000,   -- Unicode chars, 64..100000
                      chunk_overlap => 200,    -- 0..chunk_size-1
                      destination   => 'articles_body_chunks',
                      format        => E'$title\n\n$chunk');  -- $chunk required
GRANT SELECT ON public.articles_body_chunks,
                public.articles_body_chunks_view TO app_role;  -- owner-only default
-- Teardown keeps the chunk data unless you prove-and-drop it:
SELECT postvec.disable('public.articles', 'body', drop_destination => true);
```

In chunked mode the writer's transaction deletes the changed documents' chunk
rows (bulk updates multiply that). An edited document is absent from search
until its refresh drains: current chunks or nothing for that document, not
stale text. Overlap duplicates stored text and every chunk is an index row.
A document caps at 10,000 non-blank chunks, 32 MiB of input and 4x output
amplification (overlaps above 75% of `chunk_size` dead-letter sufficiently
long documents, and `enable()` warns about them). Single-column primary keys
only. Splitter geometry is fixed per entry (disable, drop, re-enable to
change it).

### One-shot helpers

```sql
SELECT postvec.embed('hello world', 'baai-bge-m3')::vector;
SELECT postvec.convert(postvec.embed('hello','baai-bge-m3'), 'baai-bge-m3', 'cohere-embed-v4.0');
```

`embed` / `convert` / `refresh_models` drive network inference from the calling
backend. They are executable by superusers; grant per app role as needed:
`GRANT EXECUTE ON FUNCTION postvec.embed(text, text) TO app_role;`
`search()` is PUBLIC. It is the query path, bounded by
`postvec.query_timeout_ms`.

## Consistency model

Shadow columns are eventually consistent. A write to the source column
enqueues a job (statement-level trigger with transition tables). The
background worker batches, embeds and writes the vector back shortly after.
Between the write and the worker pass the vector is stale or NULL. The query
string in `search()` is embedded synchronously (bounded by
`postvec.query_timeout_ms`). That is the inline network call on the query
path.

- **Primary key required.** `enable()` refuses tables with no PK (`ctid` is
  unstable across `UPDATE` / `VACUUM FULL`). Composite PKs are supported:
  rows are keyed by `ROW(pk1,pk2,...)::text` (matching is by text key, so
  composite-PK write-backs skip the PK index; fine for modest tables).
- **Partitioned tables.** Prefer `trigger_mode => 'row'`: row triggers are
  cloned to every current and future partition, so DML addressed directly at
  a partition still syncs. Statement-level triggers live only on the parent
  and fire for parent-addressed DML (`enable()` warns about this).
- **Bulk loads coalesce.** One `COPY` / multi-row `INSERT` fires the statement
  trigger once and enqueues set-based. Repeated updates to one row collapse to
  a single pending job.
- **Failures back off.** Transient and config errors retry with exponential
  backoff (failing migration batches too). A per-row `ContextLengthExceeded`
  is isolated by bisection and moved to `postvec.jobs_dead` (during a reembed
  migration it is skipped and counted in `rows_skipped`). Permanent errors
  dead-letter immediately. A job whose claim expires with attempts already
  exhausted (worker crash loop) dead-letters.
- **NULL source -> NULL vector** (no inference call).
- **Schema drift quarantines.** If an enabled table (or its source/vector
  column) is dropped, the worker purges its jobs, removes leftover
  triggers/functions, marks the registry row `disabled` and logs a
  `WARNING`. Re-running `enable()` on a recreated table detects the stale
  entry and replaces it.
- **Sync latency.** The generated triggers nudge the worker's latch
  (`postvec.worker_kick()`), so typical latency is the write-back round trip.
  The poll interval is the backstop.
- **Worker GUC reloads.** Worker-side SIGHUP GUCs are live after
  `SELECT pg_reload_conf();`. `shared_preload_libraries` still needs a restart.

## Operations

- **Autovacuum.** Every write-back is a new row version; a 1024-dim vector is
  about 4 KB. For hot tables lower `autovacuum_vacuum_scale_factor` (about
  0.05) on the table.
- **HNSW index.** Create it after the initial backfill drains, or declare
  `index_mode => 'auto'` and the worker does that once the entry's
  queue/migration/backfill work drains (an ordinary blocking `CREATE INDEX`;
  a failed build parks in `status().index_error` until
  `create_vector_index()` repairs it, and an index you build yourself
  satisfies readiness). Keep the default `'manual'` plus
  `CREATE INDEX CONCURRENTLY` for large or write-heavy tables. After heavy
  churn, `REINDEX INDEX CONCURRENTLY` then `VACUUM`. `vector` HNSW is limited
  to 2000 dims; use a `halfvec` expression index above that.
- **Dead-letter queue.** Inspect `postvec.jobs_dead` (its own `dead_id` PK;
  `job_id` is the id the job had in `postvec.jobs`). After fixing the cause,
  re-drive with `SELECT postvec.retry_dead('docs', 'body');` (all dead rows
  for the entry) or `SELECT postvec.retry_dead('docs', 'body', ARRAY[17, 23]);`
  (selected `dead_id`s). It is table-owner-gated, consumes the dead rows
  atomically and coalesces with any already-pending job.
- **Logical replication.** Statement triggers skip subscriber-applied
  changes. Run postvec on the publisher; the vector column then replicates
  like any other column.
- **Security.** The extension is untrusted (cdylib). `enable` / `disable`
  require table ownership (or superuser) and quote every identifier. The
  worker connects as the bootstrap superuser and bypasses RLS. `enable()` on
  a column whose RLS is meant to hide text from administrators exposes that
  text to the worker. Text leaves the database host only to the configured
  inference nodes or external providers.
- **Grants.** The generated triggers run as the DML-issuing role, so the
  extension ships `USAGE` on schema `postvec`, `SELECT` on the control tables
  and `INSERT (registry_id, pk_value)` on `postvec.jobs`. The TRUNCATE purge
  runs as `SECURITY DEFINER` (pinned `search_path`), so truncating roles need
  no DELETE grant. Management functions (`enable` / `disable` / `migrate`)
  still require table ownership and write access to the control tables. Run
  them as a superuser or admin role.
- **Uninstall.** `sudo postvec uninstall --database univec` runs the SQL
  cleanup, drops the extension without `CASCADE`, removes the database from
  the launcher configuration and restarts. By hand:
  `SELECT postvec.uninstall(drop_columns => false)` before
  `DROP EXTENSION postvec`. Pass `drop_columns => true` to remove shadow
  vector columns too. It sweeps every registry entry and requires a
  superuser.
- **Diagnosis.** `postvec doctor` checks preload and pending-restart state,
  extension and pgvector versions, worker liveness, queue and dead-letter
  state, model-cache freshness, registry integrity, missing ANN indexes,
  migration state, endpoint reachability and, in embedded mode, the
  disk/loaded/cached model reconciliation. Stable check ids and
  `--format json` for automation. It writes nothing. See
  [install](https://postvec.dev/docs/install/).

## SQL surface

| Function | Returns |
|---|---|
| `version()` / `build_info()` | `text` / `jsonb` |
| `embed(text, model)` / `embed(text[], model)` | `real[]` / `setof real[]` |
| `convert(real[], source_model, target_model)` | `real[]` |
| `refresh_models()` | `int` |
| `enable(relation, column_name, model, ...)` | `bigint`. `chunking => 'recursive'` + `destination` switches to managed 1:N chunk mode |
| `adopt(relation, column_name, vector_column, model, ...)` | `bigint`. Take over an existing populated `vector(N)` column. `backfill` is `'missing'` (default) / `'all'` / `'none'`. `sync => false` adopts read-only |
| `disable(relation, column_name, drop_column?, drop_destination?)` | `void`. `drop_column` is refused for adopted columns and chunked entries. `drop_destination` removes a chunked entry's proven destination |
| `uninstall(drop_columns?, drop_destinations?)` | `bigint` cleaned entries |
| `create_vector_index(relation, column_name)` | `void` |
| `search(relation, column_name, query, ...)` | `TABLE(pk_value, rrf_score, semantic_rank, fts_rank, chunk_seq, chunk_start, chunk_end, chunk_text)` - winning chunk for a chunked entry, NULL in column mode |
| `search_with_vector(...)` | `TABLE(...)` |
| `set_format(relation, column_name, format)` | `void` - change or clear the embedding template; atomic full refresh |
| `retry_dead(relation regclass, column_name, dead_ids?)` | `bigint` dead rows consumed |
| `status()` | `TABLE(...)` |
| `stats()` | `TABLE(...)` - worker counters + queue totals |
| `worker_kick()` | `void` (latch nudge; called by the triggers) |
| `migrate(relation, column_name, new_model, ...)` | `bigint` (migration id) |
| `migration_status(migration_id?)` | `TABLE(...)` including progress and suggested index SQL |
| `migration_finalize(migration_id)` | `void` (column swap; call again after building the index) |
| `migration_abort(migration_id)` | `void` (drops the new column; old data stays) |

## Model migration

Switch a column's embedding model and convert the stored vectors in place:

```sql
-- pre-flights the convert path (direct converter, else a two-hop bridge),
-- adds body_semantic_new vector(M), and starts the background driver
SELECT postvec.migrate('docs', 'body', new_model => 'cohere-embed-v4.0');

SELECT * FROM postvec.migration_status();   -- rows_done / rows_total, state, errors

-- when state = 'awaiting_finalize': swap columns (drop old, rename new)
SELECT postvec.migration_finalize(<id>);
-- reindex => 'manual' (default): if the old column had a vector index the
-- migration parks in 'awaiting_index' and migration_status() carries the
-- exact CREATE INDEX CONCURRENTLY; build it, then finalize again.
-- reindex => 'blocking': the index is rebuilt inline at finalize.

SELECT postvec.migration_abort(<id>);       -- any time before the swap
```

Strategies: `'convert'` (default; error if no convert path), `'reembed'`
(re-embed source text with the new model; rows whose text is NULL are counted
as `rows_skipped`), `'auto'` (prefer convert, fall back to reembed).

The target model can be a converter's target only (for example
`cohere-embed-v4.0` with no provider API key on any inference node). The
column dimension comes from the converter's `target_dim`, stored vectors
convert as usual, and fresh writes are embedded through the engine's
**embed-bridge** executor (embed with the converter's source model, convert
engine-side, one RPC). Embed resolution is two-tier everywhere (worker,
`search()`, `enable()`, `embed()`): a hosted embed model wins; the bridge is
the fallback, re-resolved from the model cache on every batch. Configuring an
external provider for that space is how a column starts sending source text
to the provider, which is why `postvec provider add` lists the affected
columns and requires an acknowledgement. See
[external providers](https://postvec.dev/docs/models/providers).
`enable()` / `migrate()` still require an embeddable source on the convert
path.

Adopted columns migrate too: `adopt()` a column full of vectors from a
deprecated model, `migrate()` it to a current one. The swap drops every index
on the old column and leaves old defaults, constraints, comments,
privileges/security labels and statistics settings uncopied.
`migrate()` refuses while such column-local metadata exists, warns about
dependent views, and `migration_finalize()` re-checks under a table lock,
refusing (retryably, without `CASCADE`) while blocking dependents remain. An
**observed** entry (`adopt(sync => false)`) has no triggers, so online
migration cannot see application writes: `migrate()` demands
`observed_writes_quiesced => true`, and writes must stay stopped until
`migration_finalize()` commits. After finalize the replacement column is
postvec-owned, and a second `adopt(sync => true)` promotes the entry to
normal synced operation in place.

Writes arriving mid-migration are embedded with the **new** model into the
**new** column and always win over conversions (`new IS NULL` guards on both
the driver and its write-back). `search()` keeps using the old column until
the swap. Transient inference errors retry until they succeed, visible in
`migration_status().error`. Bridge-inventory errors (`BridgePathNotFound`,
`ConverterNotFound`: the chain is incomplete on every node right now, rollout
skew or a model load window) try the other nodes first. Permanent errors
(`TargetRestricted`, `InvalidInput`, malformed responses) mark the migration
`failed`, after which fresh writes route back to the old column and
`migration_abort()` reverts.

## Embedded mode

The background worker hosts the UniVec `engine` crate in-process. Queue,
search, migrate, embed-bridge routing and the error taxonomy are the same as
in gRPC mode.

The engine lives in exactly one process: the **launcher** (the static
background worker, which also supervises the per-database workers).
Connection backends (`search()`, `embed()`, `convert()`, `enable()`'s probe)
and every per-database worker keep the gRPC client, pointed at a loopback
gRPC server the launcher runs (`postvec.embedded_listen`, default
`127.0.0.1:33433`). One engine serves however many databases
`postvec.database` lists. The engine runs on its own multi-thread tokio
runtime whose threads are pure compute and leave Postgres state alone.

```bash
cargo pgrx package --no-default-features --features pg18,embedded \
    --pg-config ~/.pgrx/18.*/pgrx-install/bin/pg_config
```

```conf
shared_preload_libraries = 'postvec'
postvec.database        = 'univec,analytics'  # one or many - one shared engine either way
postvec.mode            = 'embedded'
postvec.path = '/opt/postvec'   # libs/**/libonnxruntime.so + models/
# optional:
postvec.embedded_models = 'baai-bge-m3,convert-bge-to-cohere,embed-bridge'
postvec.embedded_listen      = '127.0.0.1:33433'   # gRPC (embed/convert)
postvec.embedded_http_listen = '127.0.0.1:33434'   # GET /config (discovery)
```

- **One shared engine, any number of databases.** The launcher hosts the
  engine and listeners; each per-database worker is a thin loopback gRPC
  client, exactly like connection backends. Workers gate draining on a
  listener reachability probe, so jobs hold their retry budget while the
  engine is still loading, and they discover models via the launcher's
  loopback `GET /config`. Launcher engine-init failures appear in the server
  log (the launcher serves no database, so there is no `stats()` row for it;
  each worker's transport errors surface in its own `stats()`).
- **Model assets on disk.** The database host carries
  `$root/libs/**/libonnxruntime.*` and `$root/models/<backend>/<name>/...`.
  `postvec.embedded_models` empty means scan-load every enabled model under
  `models/`.
- **Model discovery is worker-driven.** The engine host refreshes
  `postvec.models` from the engine right after startup; per-database workers
  refresh from the host's loopback `/config`, both on the
  `model_refresh_interval_ms` cadence. SQL `refresh_models()` polls the
  loopback `/config` and returns a transport error while the engine host is
  down. Right after `embedded engine up`, queries may degrade for about one
  poll tick until the first cache refresh lands.
- **Preload what you use.** A model that is on disk but unloaded at engine
  start JIT-loads on first use; a cold load longer than the request budget
  costs that batch one retry plus backoff (the load finishes in the
  background). Scan mode loads everything up front. With an explicit
  `postvec.embedded_models` list, include every model your entries and
  migrations touch.
- **Failure containment.** Engine init failures (missing ORT library, bad
  root path, taken port) leave the worker up: it heartbeats, surfaces the
  error in `stats().last_error` and retries every 30 s. Prediction-time
  panics are contained by the engine and surface as ordinary
  retryable/dead-letter job errors. A native fault (segfault / OOM kill)
  inside ONNX Runtime takes the worker process down; postmaster respawns it
  in 5 s. Remote mode isolates that fault from PostgreSQL.
- **Resources.** Inference shares CPU and RAM with Postgres on the database
  host. Bound it via each model's `pool_size` and ORT thread settings in its
  `ninference.hub.json`; the engine's own runtime uses 2 threads (heavy work
  runs on bounded blocking pools).
- **Security.** The loopback server speaks plain gRPC with no auth: anything
  that can reach the socket can drive inference. Keep
  `postvec.embedded_listen` on 127.0.0.1 (postvec warns otherwise).
- **Model configs must be self-contained.** A tokenizer config with an empty
  `pretrained_vocab_file` makes the engine fall back to downloading the
  tokenizer from the Hugging Face hub (network egress and writes under the
  postgres OS user's `~/.cache/huggingface`). Ship complete model assets.
- **ORT C-side diagnostics.** The embedded build compiles `ort` without its
  `tracing` feature, so ONNX Runtime C-side messages are dropped.
  Engine-level logs (model load, prediction errors) land in the server log
  via the stderr bridge.

Limits: [postvec.dev/docs/limits](https://postvec.dev/docs/limits).

## Packaging

```bash
# one artifact per supported PG major (16 / 17 / 18)
cargo pgrx package --no-default-features --features pg18 \
    --pg-config ~/.pgrx/18.*/pgrx-install/bin/pg_config
```

`cargo pgrx package` emits the versioned install SQL, control file and `.so`
under `target/release/postvec-pg18/`. Upgrade scripts between released
versions are hand-written; see [`sql/README.md`](./sql/README.md).

## Local development

```bash
rustup update stable                              # Rust >= 1.96
# protobuf-compiler (`protoc`) is required: build.rs compiles the vendored
# proto/ninference.proto with tonic-build (e.g. `apt install protobuf-compiler`)
cargo install cargo-pgrx --version 0.18.1 --locked
cargo pgrx init --pg18 download

# pgvector into the managed cluster
PG_CONFIG=~/.pgrx/18.*/pgrx-install/bin/pg_config
git clone --branch v0.8.2 https://github.com/pgvector/pgvector /tmp/pgvector
make -C /tmp/pgvector PG_CONFIG="$PG_CONFIG" && make -C /tmp/pgvector PG_CONFIG="$PG_CONFIG" install

# Iterate
cargo pgrx run pg18        # opens psql against a dev cluster
./ci.sh                    # fmt + clippy + cargo pgrx test (the CI gate)
```

`#[pg_test]`s use `/config` fixtures and an in-process mock `InferenceClient`.
Worker and live end-to-end runs use a real inference node -
`packaging/postvec/scripts/build-server-image.sh` builds one - with the
endpoint GUCs set. CI is
[`.github/workflows/postvec-ci.yml`](../.github/workflows/postvec-ci.yml),
mirrored by `ci.sh`.
