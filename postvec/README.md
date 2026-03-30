# postvec

A PostgreSQL extension (Rust + [pgrx](https://github.com/pgcentralfoundation/pgrx))
that makes an embedding column **transparent**: declare a text column semantic,
and postvec keeps a shadow [`pgvector`](https://github.com/pgvector/pgvector)
column in sync as rows change, offers hybrid (full-text + vector) search in one
call, and — the headline feature — **migrates existing vectors between embedding
models in place** by converting them through UniVec's `/convert` instead of
re-embedding source text.

`postvec.mode` defaults to **embedded**: the UniVec engine runs inside the
postvec launcher process — no external inference dependency, no raw-text
egress off the DB host (see [Embedded mode](#embedded-mode-arch-b)); every
published package carries the `embedded` build feature this needs. Remote
mode (`postvec.mode = 'grpc'`) is instead a thin gRPC client to running
**ninference** nodes on a trusted private network (`EmbedTexts` /
`ConvertEmbeddings`) — the availability-oriented deployment, since a native
engine fault in embedded mode shares PostgreSQL's crash domain.
The maintainer's code walkthrough (file map, call chains, invariants,
adaptation recipes) is
[`docs/postvec-description.md`](../docs/postvec-description.md)
§11. Design rationale and the original execution plan live in UniVec's
internal design records.

> **Status.** Phases 0–2 are implemented: `enable`/`disable`, the trigger →
> queue → background-worker sync loop, queue & cursor backfill, hybrid
> `search`, `status`/`stats`, **`migrate()`** (in-DB model migration via
> convert, with finalize/abort), **`adopt()`** (take over an existing
> populated pgvector column without re-embedding it — P4), composite-PK &
> partitioned-table support, per-database workers, and trigger latch-kick. Embedded mode (Arch B) is
> implemented behind the `embedded` build feature, which every published
> package enables; a plain `cargo` build without it serves only the thin
> gRPC client (Arch C). The embedded live-engine acceptance run (R5,
> real model assets) is still owed.

## Requirements

- PostgreSQL **16 / 17 / 18** with `postvec` in `shared_preload_libraries`
  (the background worker registers at postmaster start).
- **pgvector ≥ 0.8** in the same database (`CREATE EXTENSION postvec CASCADE`
  pulls it in).
- A reachable **ninference** node (gRPC `:33333`, HTTP `:22222`) on the mesh.

## Quickstart

Three ways in, shortest first. All of them end at the same place.

**One container, nothing else.** PostgreSQL, pgvector, postvec and an embedding
model in a single image — no inference service to run, no API key:

```bash
docker run -d --name postvec \
  -e POSTGRES_PASSWORD=change-me -e POSTGRES_DB=app \
  -v postvec-data:/var/lib/postgresql \
  -p 127.0.0.1:5432:5432 \
  ghcr.io/univec-ai/postvec:0.1.0-1-pg18-complete
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

(Mount `/var/lib/postgresql/data` instead for the PG 16 and 17 images — the
official image changed its layout in 18. The `:0.1.0-1-pg18` tag, without
`-complete`, is the same thing pointed at remote ninference nodes; the tag is
`<version>-<packaging revision>`, and `pg18` is the moving equivalent.)

**Packages, against your own cluster:**

```bash
sudo apt install ./postvec-cli_*.deb ./postgresql-18-postvec_*.deb
sudo postvec setup --database app --grpc 192.0.2.2:33333 --http https://192.0.2.2:22222
postvec doctor --database app
```

**From source:** [`docs/quick-install.md` §5](../docs/quick-install.md#5-source-lane--local-cargo-build-and-manual-copy).

Full package and image reference — variants, environment variables, volumes,
upgrades, verification — is
[install.md §2](../docs/install.md#2-install-released-packages);
how the artifacts are built is
[`packaging/postvec/`](../packaging/postvec/README.md).

## Install & configure

Once the package assets are on the host, one command does the whole install —
database, extension, configuration, restart and verification — using the
[`postvec` CLI](../postvec-cli):

```bash
sudo postvec setup --database univec \
     --grpc 192.0.2.2:33333 --http https://192.0.2.2:22222
postvec doctor                      # read-only health check, any time later
```

It merges (never clobbers) `shared_preload_libraries`, owns exactly one
`conf.d` snippet, creates the database *before* activating the launcher, and
proves the worker heartbeat advances before reporting success. `--dry-run`
shows the plan and changes nothing.

The manual equivalent, for a cluster you configure by hand:

```sql
CREATE EXTENSION postvec CASCADE;         -- brings in `vector`
```

Point postvec at ninference and name the database(s) to serve (in
`postgresql.conf` or via `ALTER SYSTEM`):

```conf
shared_preload_libraries = 'postvec'      # requires a restart
postvec.database                = 'univec'                 # one or more, comma-separated (POSTMASTER)
postvec.ninference_grpc_endpoints = '192.0.2.2:33333'       # comma-separated, round-robin
postvec.ninference_http_endpoints = 'https://192.0.2.2:22222'  # /config discovery
```

```sql
SELECT pg_reload_conf();
SELECT postvec.refresh_models();          -- populate the model cache
```

### GUCs

| GUC | Default | Context | Purpose |
|---|---|---|---|
| `postvec.ninference_grpc_endpoints` | `''` | SIGHUP | `host:33333` list (round-robin) |
| `postvec.ninference_http_endpoints` | `''` | SIGHUP | `http(s)://host:22222` list for `/config` |
| `postvec.database` | `''` | POSTMASTER | Comma-separated database(s); the launcher runs one worker per database |
| `postvec.worker_enabled` | `on` | SIGHUP | Pause/resume job processing |
| `postvec.poll_interval_ms` | `5000` | SIGHUP | Worker idle poll interval (writers wake the worker at commit; the poll is the backstop) |
| `postvec.batch_size` | `64` | SIGHUP | Texts per `EmbedTexts` call |
| `postvec.migrate_batch_size` | `256` | SIGHUP | Vectors per `ConvertEmbeddings` (Phase 2) |
| `postvec.embed_timeout_ms` | `30000` | SIGHUP | Worker gRPC deadline |
| `postvec.query_timeout_ms` | `2000` | USERSET | `search()`/`embed()` inline deadline |
| `postvec.max_retries` | `5` | SIGHUP | Before a job → `jobs_dead` |
| `postvec.retry_backoff_ms` | `5000` | SIGHUP | Base for exponential backoff |
| `postvec.job_visibility_timeout_ms` | `300000` | SIGHUP | Stale-claim reclamation |
| `postvec.model_refresh_interval_ms` | `60000` | SIGHUP | `/config` poll cadence |
| `postvec.search_degrade_to_fts` | `on` | USERSET | FTS-only fallback when ninference is down |
| `postvec.discovery_timeout_ms` | `5000` | SIGHUP | Per-node HTTP timeout for `GET /config` |
| `postvec.worker_lock_timeout_ms` | `10000` | SIGHUP | `lock_timeout` for worker transactions (0 disables) — a blocked user table backs the batch off instead of freezing the worker |
| `postvec.notify_on_write` | `off` | SIGHUP | `NOTIFY postvec, '<registry_id>'` after each write-back batch |
| `postvec.mode` | `grpc` | POSTMASTER | `grpc` (remote ninference) or `embedded` (in-worker engine; needs the `embedded` build feature) |
| `postvec.ninference_path` | `''` | POSTMASTER | Embedded: engine root (`libs/`, `models/`); falls back to `$NINFERENCE_PATH` |
| `postvec.embedded_models` | `''` | POSTMASTER | Embedded: comma-separated models to preload; empty = load every enabled model on disk |
| `postvec.embedded_listen` | `127.0.0.1:33433` | POSTMASTER | Embedded: loopback address of the engine host's gRPC server (dialed by backends and, in launcher mode, per-DB workers) |
| `postvec.embedded_http_listen` | `127.0.0.1:33434` | POSTMASTER | Embedded: loopback address of the engine host's `GET /config` listener (model discovery, `refresh_models()`) |

## The SQL surface

```sql
-- 1. Make docs.body semantic. Adds body_semantic vector(N), triggers, backfill.
--    Options: distance => 'cosine'|'l2'|'ip', trigger_mode => 'statement'|'row',
--    create_fts_index => bool, backfill => bool,
--    index_mode => 'manual'|'immediate'|'auto',
--    format => an embedding template (see below).
SELECT postvec.enable('public.docs', 'body', model => 'baai-bge-m3');
-- Build the vector index after the backfill drains (or declare
-- index_mode => 'auto' to let the worker build it once work drains, or
-- 'immediate' for a synchronous build at enable time — both are ordinary
-- blocking CREATE INDEX; keep 'manual' + CREATE INDEX CONCURRENTLY for
-- large/write-heavy tables):
SELECT postvec.create_vector_index('public.docs', 'body');

-- 2. Write normally — the shadow column fills asynchronously (see below).
INSERT INTO docs (body) VALUES ('quarterly revenue guidance was raised');
UPDATE docs SET body = 'revised full-year guidance' WHERE id = 1;

-- 3. Hybrid search (FTS + vector, RRF-fused; query auto-embedded).
SELECT d.*, s.rrf_score
  FROM postvec.search('docs', 'body', 'guidance for the quarter', limit_n => 20) s
  JOIN docs d ON d.id = s.pk_value::bigint
 ORDER BY s.rrf_score DESC;

-- 3b. Typed metadata filters run INSIDE both search legs, before ranking and
--     LIMIT — values are bound parameters, never SQL. AND-only; operators:
--     neq/gt/gte/lt/lte/in/like/ilike/is_not; null means IS NULL.
SELECT * FROM postvec.search('docs', 'body', 'quarterly guidance',
         filter => '{"category": "finance",
                     "published_at": {"gte": "2026-01-01"},
                     "region": ["EU", "UK"],
                     "archived_at": null}'::jsonb);

-- 3c. Embedding templates give short texts their document context. The
--     source column ($body here) anchors the lifecycle: NULL source = NULL
--     vector; changed context columns re-enqueue. Note the E-string — the
--     template itself does no backslash processing.
SELECT postvec.enable('public.articles', 'body', model => 'baai-bge-m3',
                      format => E'$title — $author\n\n$body');
-- Changing/clearing the template is an explicit, atomic full re-embed:
SELECT postvec.set_format('public.articles', 'body', E'$title\n\n$body');
SELECT postvec.set_format('public.articles', 'body', NULL);

-- 4. Observe the queue / worker.
SELECT * FROM postvec.status();

-- 5. On a different table, already have a populated pgvector column? Adopt
--    it instead of re-embedding: postvec takes over sync and backfills only
--    the gaps.
--    The declared vector(N) is authoritative; `model` is your assertion.
SELECT postvec.adopt('public.legacy_docs', 'body',
                     vector_column => 'embedding', model => 'baai-bge-m3');

-- 5b. The same adoption path searches stale/deprecated vectors WITHOUT
--     migrating them. The adopted model doesn't have to be directly
--     embeddable: if a converter targets its space and the converter's source
--     is embeddable, search() embeds the query with the source model and
--     converts it into the stored space via ninference's embed-bridge executor
--     — one RPC, same search() call.
--     backfill => 'none' preserves every existing vector while the default
--     sync => true keeps future writes current through the same bridge.
SELECT postvec.adopt('public.ada_docs', 'body', vector_column => 'embedding',
                     model => 'openai-text-embedding-ada-002',
                     backfill => 'none');
SELECT * FROM postvec.search('public.ada_docs', 'body', 'embedding migration risk');
--     For a frozen or NOT NULL legacy column, use
--     sync => false, backfill => 'none' instead.
--     A direct embed model always wins over the bridge; migrate() in place
--     later, when you're ready to leave the deprecated space.

-- 6. Turn an adopted example off (keeps the column unless drop_column => true;
--    an adopted column is never dropped by postvec).
SELECT postvec.disable('legacy_docs', 'body');

-- 7. Documents bigger than one embedding? Recursive chunking (P5): one row
--    becomes many chunk vectors in a postvec-managed destination table with
--    a join view, FORCE RLS keyed on source visibility, and ownership-marker
--    comments. search() then returns ONE row per document plus the winning
--    chunk's seq/offsets/text; migrate() converts the chunk vectors in place.
SELECT postvec.enable('public.articles', 'body', model => 'baai-bge-m3',
                      chunking      => 'recursive',
                      chunk_size    => 2000,   -- Unicode chars, 64..100000
                      chunk_overlap => 200,    -- 0..chunk_size-1
                      destination   => 'articles_body_chunks',
                      format        => E'$title\n\n$chunk');  -- $chunk required
GRANT SELECT ON public.articles_body_chunks,
                public.articles_body_chunks_view TO app_role;  -- owner-only default
-- Teardown keeps the chunk data unless you prove-and-drop it explicitly:
SELECT postvec.disable('public.articles', 'body', drop_destination => true);
```

Chunked-mode honesty: the **writer's transaction deletes the changed
documents' chunk rows** (bulk updates multiply that); an edited document is
absent from search until its refresh drains (false negatives, never stale
text); overlap duplicates stored text and every chunk is an index row; a
document caps at 10,000 non-blank chunks, 32 MiB of input, and 4× output
amplification (overlaps above 75% of `chunk_size` dead-letter sufficiently
long documents, and `enable()` warns about them); single-column
primary keys only;
splitter geometry is fixed per entry (disable/drop/re-enable to change it).

### One-shot helpers (debugging)

```sql
SELECT postvec.embed('hello world', 'baai-bge-m3')::vector;
SELECT postvec.convert(postvec.embed('hello','baai-bge-m3'), 'baai-bge-m3', 'cohere-embed-v4.0');
```

`embed`/`convert`/`refresh_models` drive network inference from the calling
backend and are **not** PUBLIC-executable (any DB role could otherwise use
them as a resource amplifier). Superusers have them; grant per app role as
needed: `GRANT EXECUTE ON FUNCTION postvec.embed(text, text) TO app_role;`
`search()` stays PUBLIC — it is the query path, bounded by
`postvec.query_timeout_ms`.

## Consistency model

Shadow columns are **eventually consistent** — the same trade-off
`pg_vectorize` and `pgai` make. A write to the source column enqueues a job
(via a statement-level trigger with transition tables); the background worker
batches, embeds, and writes the vector back shortly after. Between the write
and the worker pass there is a window where the vector is stale or NULL. The
query string in `search()` is embedded **synchronously** (bounded by
`postvec.query_timeout_ms`) — the one place inline network I/O happens.

Requirements and behavior:

- **Primary key required.** `enable()` refuses tables with no PK (`ctid` is
  not stable across `UPDATE`/`VACUUM FULL`). Composite PKs are supported —
  rows are keyed by `ROW(pk1,pk2,…)::text` (note: matching is by text key, so
  composite-PK write-backs do not use the PK index; fine for modest tables).
- **Partitioned tables are supported.** Prefer `trigger_mode => 'row'`:
  row triggers are cloned to every (current and future) partition, so DML
  addressed directly at a partition still syncs. Statement-level triggers
  live only on the parent and fire only for parent-addressed DML (`enable()`
  warns about this).
- **Bulk loads coalesce.** One `COPY`/multi-row `INSERT` fires the statement
  trigger once and enqueues set-based; repeated updates to one row collapse to
  a single pending job.
- **Failures back off.** Transient/config errors retry with exponential
  backoff (failing migration batches too); a per-row `ContextLengthExceeded`
  is isolated by bisection and moved to `postvec.jobs_dead` (during a reembed
  migration it is skipped and counted in `rows_skipped`); permanent errors
  dead-letter immediately. A job whose claim expires with attempts already
  exhausted (worker crash loop) dead-letters instead of redelivering forever.
- **NULL source → NULL vector** (no inference call).
- **Schema drift quarantines, never crash-loops.** If an enabled table (or its
  source/vector column) is dropped, the worker *quarantines* the entry — purges
  its jobs, removes leftover triggers/functions, marks the registry row
  `disabled`, and logs a `WARNING` — instead of dying on the same error every
  pass. Re-running `enable()` on a recreated table detects the stale entry and
  replaces it.
- **Sync latency.** The generated triggers nudge the worker's latch
  (`postvec.worker_kick()`), so typical latency is the write-back round trip,
  not the 1 s poll interval (which remains the backstop).
- **Worker GUC reloads.** Worker-side SIGHUP GUCs are live after
  `SELECT pg_reload_conf();`; only `shared_preload_libraries`,
  `postvec.database`, and the embedded GUCs require restart.

## Operations

- **Autovacuum.** Every write-back is a new row version; a 1024-dim vector is
  ~4 KB. For hot tables lower `autovacuum_vacuum_scale_factor` (≈ 0.05) on the
  table.
- **HNSW index.** Create it *after* the initial backfill drains (faster, less
  bloat) — or declare `index_mode => 'auto'` and the worker does exactly that
  once the entry's queue/migration/backfill work drains (an ordinary blocking
  `CREATE INDEX`; a failed build parks in `status().index_error` until
  `create_vector_index()` repairs it, and an index you build yourself
  satisfies readiness without postvec claiming it). Keep the default
  `'manual'` plus `CREATE INDEX CONCURRENTLY` for large or write-heavy
  tables. After heavy churn, `REINDEX INDEX CONCURRENTLY` then `VACUUM`
  (pgvector guidance). Note `vector` HNSW is limited to 2000 dims — use a
  `halfvec` expression index above that.
- **Dead-letter queue.** Inspect `postvec.jobs_dead` (its own `dead_id` PK;
  `job_id` is the id the job had in `postvec.jobs`); after fixing the cause,
  re-drive safely with `SELECT postvec.retry_dead('docs', 'body');` (all dead
  rows for the entry) or `SELECT postvec.retry_dead('docs', 'body',
  ARRAY[17, 23]);` (selected `dead_id`s). It is table-owner-gated, consumes
  the dead rows atomically, and coalesces with any already-pending job; never
  hand-INSERT from `jobs_dead` into `jobs`.
- **Logical replication (R6).** Statement triggers do not fire for
  subscriber-applied changes — run postvec on the **publisher**; the vector
  column then replicates like any other column.
- **Security.** The extension is untrusted (cdylib). `enable`/`disable` require
  table ownership (or superuser) and quote every identifier. The worker
  connects as the bootstrap superuser and bypasses RLS — do not `enable()`
  columns whose RLS is meant to hide text from administrators. Text leaves the
  DB host only to ninference over the mesh (no third-party egress).
- **Grants (what app roles get).** The generated triggers run as the
  DML-issuing role, so the extension ships the grants that make writes by
  plain application roles work out of the box: `USAGE` on schema `postvec`,
  `SELECT` on the control tables, and `INSERT (registry_id, pk_value)` —
  those two columns only — on `postvec.jobs`. The TRUNCATE purge runs as
  `SECURITY DEFINER` (pinned `search_path`), so truncating roles need no
  DELETE grant. Management functions (`enable`/`disable`/`migrate`) still
  require table ownership and write access to the postvec control tables —
  in practice, run them as a superuser/admin role.
- **Uninstall.** `sudo postvec uninstall --database univec` does the whole
  thing: runs the cleanup below, drops the extension (never with `CASCADE`),
  removes the database from the launcher configuration, and restarts. By hand:
  `SELECT postvec.uninstall(drop_columns => false)` before
  `DROP EXTENSION postvec` for one-shot cleanup of generated
  triggers/functions, jobs, and unfinished migrations. Pass
  `drop_columns => true` to remove shadow vector columns too. It sweeps every
  registry entry in the database and therefore requires a superuser.
- **Diagnosis.** `postvec doctor` turns the troubleshooting tables in
  [install.md](../docs/install.md) into an executable: preload and
  pending-restart state, extension and pgvector versions, worker liveness,
  queue and dead-letter state, model-cache freshness, registry integrity,
  missing ANN indexes, migration state, endpoint reachability, and — in
  embedded mode — the disk/loaded/cached model reconciliation. Stable check
  ids and `--format json` for automation; it never writes anything.

## SQL surface

| Function | Returns | Phase |
|---|---|---|
| `version()` / `build_info()` | `text` / `jsonb` | 0 |
| `embed(text, model)` / `embed(text[], model)` | `real[]` / `setof real[]` | 0 |
| `convert(real[], source_model, target_model)` | `real[]` | 0 |
| `refresh_models()` | `int` | 0 |
| `enable(relation, column_name, model, vector_column?, fts_config?, create_fts_index?, backfill?, distance?, trigger_mode?, index_mode?, backfill_mode?, format?, chunking?, chunk_size?, chunk_overlap?, destination?)` | `bigint` — `chunking => 'recursive'` + `destination` switches to the managed 1:N chunk mode | 1 |
| `adopt(relation, column_name, vector_column, model, sync?, backfill?, backfill_mode?, distance?, trigger_mode?, fts_config?, create_fts_index?, format?, index_mode?)` | `bigint` — take over an existing populated `vector(N)` column; `backfill` is `'missing'` (default) \| `'all'` \| `'none'`; `sync => false` adopts read-only (observed) | P4 |
| `disable(relation, column_name, drop_column?, drop_destination?)` | `void` (refuses `drop_column` for adopted columns and for chunked entries; `drop_destination` removes a chunked entry's proven destination) | 1 |
| `uninstall(drop_columns?, drop_destinations?)` | `bigint` cleaned entries | 1 |
| `create_vector_index(relation, column_name)` | `void` | 1 |
| `search(relation, column_name, query, limit_n?, semantic_weight?, rrf_k?, candidates?, filter?)` | `TABLE(pk_value, rrf_score, semantic_rank, fts_rank, chunk_seq, chunk_start, chunk_end, chunk_text)` — the chunk fields are the winning chunk for a chunked entry, NULL in column mode | 1 |
| `search_with_vector(relation, column_name, query_vector, query_text?, limit_n?, semantic_weight?, rrf_k?, candidates?, filter?)` | `TABLE(...)` | 1 |
| `set_format(relation, column_name, format)` | `void` — change/clear the embedding template; atomic full refresh of every row | P7 |
| `retry_dead(relation regclass, column_name, dead_ids?)` | `bigint` dead rows consumed — owner-gated, atomic dead-letter re-drive | P11 |
| `status()` | `TABLE(...)` | 1 |
| `stats()` | `TABLE(...)` — worker counters + queue totals | 2 |
| `worker_kick()` | `void` (latch nudge; called by the triggers) | 2 |
| `migrate(relation, column_name, new_model, strategy?, reindex?, observed_writes_quiesced?)` | `bigint` (migration id) | 2 |
| `migration_status(migration_id?)` | `TABLE(...)` incl. progress + suggested index SQL | 2 |
| `migration_finalize(migration_id)` | `void` (column swap; call again after building the index) | 2 |
| `migration_abort(migration_id)` | `void` (drops the new column; old data untouched) | 2 |

## Model migration (the headline feature)

Switch a column's embedding model and convert the stored vectors **in place**
— no re-embedding of source text (which may be slow, costly, or gone):

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

Strategies: `'convert'` (default — error if no convert path), `'reembed'`
(re-embed source text with the new model; rows whose text is NULL are counted
as `rows_skipped`), `'auto'` (prefer convert, fall back to reembed).

**The target model does not need to be embeddable directly.** A model that
exists only as a converter's target (e.g. a commercial space like
`cohere-embed-v4.0` with no provider API key on any ninference node) is a
valid migration target: the column dimension comes from the converter's
`target_dim`, stored vectors convert as usual, and fresh writes are embedded
through ninference's **embed-bridge** executor (embed with the converter's
source model, convert engine-side — one RPC). Embed resolution is two-tier
everywhere (worker, `search()`, `enable()`, `embed()`): a hosted embed model
wins; the bridge is the fallback, re-resolved from the model cache on every
batch — so if the target later becomes directly embeddable, postvec upgrades
to direct embedding automatically. Configuring an external provider for that
space is exactly how that happens, which is why `postvec provider add` lists
the affected columns and requires an acknowledgement: their source text starts
going to the provider on the next worker cycle. See
[docs/external-providers-usage.md](../docs/external-providers-usage.md). A converter target whose *source* is not
embeddable (and any model with no embed path at all) is still refused at
`enable()`/`migrate()` time.

**Adopted columns migrate too** — that is the P4 flagship journey: `adopt()`
a column full of vectors from a deprecated model, `migrate()` it to a current
one, and no source text is ever re-embedded. Caveats that come with a column
postvec did not create: the swap drops every index on the old column and does
**not** copy old defaults, constraints, comments, privileges/security labels,
or statistics settings — `migrate()` refuses while such column-local metadata
exists, warns about dependent views, and `migration_finalize()` re-checks
under a table lock, refusing (retryably, never with `CASCADE`) while blocking
dependents remain. An **observed** entry (`adopt(sync => false)` — the
no-embed-route rescue path) has no triggers, so online migration cannot see
application writes: `migrate()` demands `observed_writes_quiesced => true`,
and writes must stay stopped until `migration_finalize()` commits. After
finalize the replacement column is postvec-owned, and a second
`adopt(sync => true)` promotes the entry to normal synced operation in place.

Writes arriving mid-migration are embedded with the **new** model into the
**new** column and always win over conversions (`new IS NULL` guards on both
the driver and its write-back); `search()` keeps using the old column until
the swap. ninference outages never fail a migration (transient errors retry
forever, visible in `migration_status().error`), and so do bridge-inventory
errors (`BridgePathNotFound`, `ConverterNotFound` — the chain isn't complete
on any node *right now*: rollout skew or a model load window; postvec tries
the other nodes first). Permanent errors (`TargetRestricted` — the target is
on the deployment's embed-bridge restriction list — `InvalidInput`, malformed
responses) mark it `failed`, after which fresh writes route back to the old
column and `migration_abort()` reverts cleanly.

## Embedded mode (Arch B)

Opt-in deployment mode in which the **background worker hosts the UniVec
`engine` crate in-process** — no external ninference node, no network hop,
and raw text never leaves the database host. It adds no SQL-level features;
everything (queue, search, migrate, embed-bridge routing, error taxonomy)
behaves exactly as in gRPC mode.

**Shape.** The engine lives in exactly one process: the **launcher** (the
static background worker, which also supervises the per-database workers).
Everything else — connection backends (`search()`, `embed()`, `convert()`,
`enable()`'s probe) and every per-database worker — keeps the unchanged
gRPC client, automatically pointed at a **loopback gRPC server the
launcher runs** (`postvec.embedded_listen`, default `127.0.0.1:33433`) —
"Arch C with the server inside the bgworker". One engine serves however
many databases `postvec.database` lists.
The engine runs on its own multi-thread tokio runtime whose threads are pure
compute and never touch Postgres state.

```bash
# build with the engine compiled in (one artifact per PG major, as usual)
cargo pgrx package --no-default-features --features pg18,embedded \
    --pg-config ~/.pgrx/18.*/pgrx-install/bin/pg_config
```

```conf
shared_preload_libraries = 'postvec'
postvec.database        = 'univec,analytics'  # one or many — one shared engine either way
postvec.mode            = 'embedded'
postvec.ninference_path = '/opt/ninference'   # libs/**/libonnxruntime.so + models/
# optional:
postvec.embedded_models = 'baai-bge-m3,convert-bge-to-cohere,embed-bridge'
postvec.embedded_listen      = '127.0.0.1:33433'   # gRPC (embed/convert)
postvec.embedded_http_listen = '127.0.0.1:33434'   # GET /config (discovery)
```

Notes and constraints:

- **One shared engine, any number of databases.** The launcher hosts the
  engine + listeners; each per-database worker is a thin loopback gRPC
  client, exactly like connection backends — one engine's RAM serves N
  databases, never RAM × N (workers are separate OS processes, so in-memory
  sharing is impossible; the loopback is the sharing mechanism). Workers
  gate draining on a listener reachability probe, so jobs don't burn retry
  attempts while the engine is still loading, and they discover models via
  the launcher's loopback `GET /config` (`postvec.embedded_http_listen`,
  default `127.0.0.1:33434`) — same envelope as ninference's `/config`,
  consumed by the unchanged discovery client. Launcher engine-init failures
  appear in the server log only (the launcher serves no database, so there
  is no `stats()` row for it; each worker's transport errors surface in its
  own `stats()`).
- **Model assets on disk.** The DB host carries
  `$root/libs/**/libonnxruntime.*` and `$root/models/<backend>/<name>/…`
  (there is no S3 hub in embedded mode). `postvec.embedded_models` empty
  means "scan-load every enabled model under `models/`".
- **Model discovery is worker-driven.** The engine host refreshes
  `postvec.models` from the engine right after startup; per-database workers
  refresh from the host's loopback `/config` — both on the
  `model_refresh_interval_ms` cadence. The SQL `refresh_models()` also works
  (it polls the loopback `/config`), but errors with a transport error while
  the engine host is down. Right after `embedded engine up`, queries may
  still degrade for about one poll tick until the first cache refresh lands.
- **Preload what you use.** A model that is on disk but not loaded at engine
  start JIT-loads on first use; a cold load longer than the request budget
  costs that batch one retry + backoff window (it converges — the load
  finishes in the background). Scan mode loads everything up front; with an
  explicit `postvec.embedded_models` list, include every model your entries
  and migrations touch.
- **Failure containment.** Engine init failures (missing ORT library, bad
  root path, taken port) never crash the worker: it stays up, heartbeats,
  surfaces the error in `stats().last_error`, and retries every 30 s.
  Prediction-time panics are contained by the engine and surface as ordinary
  retryable/dead-letter job errors. The residual risk is a **native** fault
  (segfault/OOM kill) inside ONNX Runtime — that takes the worker process
  down (postmaster respawns it in 5 s) and is the reason this mode is
  opt-in; keep gRPC mode for deployments that cannot accept it.
- **Resources.** Inference now competes with Postgres for CPU/RAM on the DB
  host. Bound it via each model's `pool_size` and ORT thread settings in its
  `ninference.hub.json`; the engine's own runtime uses 2 threads (heavy work
  runs on bounded blocking pools).
- **Security.** The loopback server speaks plain gRPC with no auth: anything
  that can reach the socket can drive inference. Keep
  `postvec.embedded_listen` on 127.0.0.1 (postvec warns otherwise).
- **Model configs must be self-contained.** A tokenizer config with an empty
  `pretrained_vocab_file` makes the engine fall back to downloading the
  tokenizer from the Hugging Face hub (network egress + writes under the
  postgres OS user's `~/.cache/huggingface`) — surprising on a DB host. Ship
  complete model assets.
- **ORT's own diagnostics don't reach the PG log.** The embedded build
  compiles `ort` without its `tracing` feature, so ONNX Runtime C-side
  messages are dropped; engine-level logs (model load, prediction errors) do
  land in the server log via the stderr bridge.

Design, constraints and the outstanding acceptance criteria:
[`docs/postvec-description.md`](../docs/postvec-description.md)
§12.

## Packaging

```bash
# one artifact per supported PG major (16 / 17 / 18)
cargo pgrx package --no-default-features --features pg18 \
    --pg-config ~/.pgrx/18.*/pgrx-install/bin/pg_config
```

`cargo pgrx package` emits the versioned install SQL + control file + .so
under `target/release/postvec-pg18/`. Upgrade scripts between released
versions are hand-written — see [`sql/README.md`](./sql/README.md). Production
rollout rides the existing Ansible stack (univec-deployment):
`shared_preload_libraries = 'postvec'` (restart required), pgvector ≥ 0.8,
endpoint GUCs pointing at mesh inference nodes.

## Local development

```bash
rustup update stable                              # Rust ≥ 1.96
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

Tests need no live inference service: `#[pg_test]`s use `/config` fixtures
and an in-process mock `InferenceClient`. Worker/live end-to-end runs use a
real inference node — `packaging/postvec/scripts/build-server-image.sh`
builds one — with the endpoint GUCs set. CI is defined in
[`.github/workflows/postvec-ci.yml`](../.github/workflows/postvec-ci.yml) and
mirrored by `ci.sh`.
