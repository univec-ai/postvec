---
title: Managed PostgreSQL
description: Run postvec on RDS, Aurora, Cloud SQL, Azure, Supabase or Neon with a plain SQL schema and postvec-server.
---

# Managed PostgreSQL

Use this on Amazon RDS, Aurora, Cloud SQL, Azure Flexible Server,
Supabase, Neon and similar hosts that load no third-party `.so`.
`postvec-server` installs a plain SQL schema, runs the sync worker and
optionally proxies single-call `search(text)`.

SQL is the same surface as the extension: `enable()`, `adopt()`,
`search()`, chunking, `migrate()`. Models, provider keys and the
inference engine live on the server host.

Installation stages:

1. Prepare the database - pgvector and a worker role
2. Install the schema - `postvec-server managed install`
3. Configure and start - a `managed` block in the server config
4. Enable or adopt a column, then search

A [self-hosted cluster](/docs/install/packages) that can load `postvec.so`
uses packages and `postvec setup` instead.

Organization production use needs [postvec pro](/server#plans).
Personal use, non-production environments and one 30-day production
evaluation per organization are free. See [License](/docs/license).

## 1. Prepare the database

Have the database administrator run:

```sql
CREATE EXTENSION IF NOT EXISTS vector;          -- pgvector 0.8 or newer
CREATE ROLE postvec_worker LOGIN PASSWORD '…';
GRANT CREATE ON DATABASE app TO postvec_worker;
```

PostgreSQL 16, 17 and 18 are supported. If pgvector lives in a schema the
role cannot use (Supabase's `extensions`), also
`GRANT USAGE ON SCHEMA extensions TO postvec_worker`.

Install as that role; it owns the managed schema. The worker needs
ownership of source tables, or membership in their owning roles. For
tables owned by other roles, in schemas the worker can use, the installer
prints two options per owner:

```sql
-- as a superuser:
GRANT app_owner TO postvec_worker;
-- or hand the tables over, as admin:
GRANT postvec_worker TO app_owner;
-- as the schema owner:
GRANT CREATE ON SCHEMA sales TO postvec_worker;
-- then as app_owner:
ALTER TABLE sales.orders OWNER TO postvec_worker;
```

Managed platforms on PostgreSQL 16+ have no superuser, and a role never
holds ADMIN OPTION on itself, so they use the hand-over. Its `GRANT` needs
ADMIN OPTION on the worker, which the role that created the worker holds;
the installer names it. The schema line appears only where the worker
lacks `CREATE`. Afterwards the owner is a member of the worker role and
keeps full control of its tables. Membership is a shared trust boundary:
every member can reach every table the worker owns, with or without
`INHERIT`. Where owners must not reach each other's tables, revoke the
membership once the tables are handed over and have the worker grant each
owner the privileges it needs on its own tables; DDL on them then runs as
the worker.

Use a **direct** or session-mode database endpoint. Leader election takes
a session advisory lock, which a transaction-pooling endpoint does not
hold. The server refuses the known ones (Supabase port 6543, Neon
`-pooler` hosts). Behind another transaction pooler the worker usually
stops with `leader lock lost`, since every worker transaction checks that
its backend still holds the lock, but only direct and session-mode
endpoints are supported.

## 2. Install the schema

Install [postvec-server](/docs/server/packages) on a host that can reach
the database. Then:

```sh
chmod 600 /etc/postvec-server/database.pw
postvec-server managed install \
  --dsn 'postgresql://postvec_worker@db.example/app?sslmode=verify-full' \
  --password-file /etc/postvec-server/database.pw
```

Expected: the `postvec` schema exists, `postvec.settings` is written
and the command prints any remaining `GRANT` statements.

Installation is transactional and repeatable. Rerun `managed install` after
upgrading postvec-server: it upgrades an older schema in place and replaces the
SQL function bodies, keeping views that use them and narrowed `EXECUTE`
grants. Schema version 3 adds the vector argument to `search()` and
`embed()`, and the upgrade to it refuses while views use their old
signatures: drop those views, upgrade, then recreate them and any
`EXECUTE` grants on the two functions. Index changes build `CONCURRENTLY` after that commit, so
application writes continue; the build waits for transactions already open,
and an interrupted build is retried by the next install. The installer refuses an extension database, an unrelated
schema or a schema newer than the server.

Passwords stay in the password file (or `PGPASSWORD` / `~/.pgpass`, which
the server also reads). The file must be a regular file
owned by the service account, mode `0600` or `0400`. Symlinks are
refused. A relative `--password-file` resolves against the working
directory of the `managed install` process. DSNs use the
`postgresql://` URI format.

## 3. Configure and start

Add this to `/etc/postvec-server/config.json`:

```json
{
  "managed": [{
    "name": "prod",
    "dsn": "postgresql://postvec_worker@db.example/app?sslmode=verify-full",
    "password_file": "/etc/postvec-server/database.pw",
    "sync": true,
    "proxy_port": 5433,
    "proxy_max_connections": 256,
    "poll_interval_ms": 2000,
    "poll_only": false,
    "batch_size": 64,
    "index_concurrently": true
  }]
}
```

Restart postvec-server. In the `managed[]` block a relative `password_file`
resolves against the engine root. Models, provider credentials and a gossip
fleet already configured on that host serve the worker.

For one database, `postvec-server serve --sync <DSN>` is a shortcut.
`--proxy 5433` opens the search proxy. `--poll-only` skips LISTEN
(useful on serverless databases). Several databases use the file.

All nodes configured for a database share one leader, even when their
DSNs use different credentials or host aliases. Standbys take over after
the leader's database session closes.

Each node holds one session per database, the leader three (worker,
heartbeat, LISTEN), plus up to four for a proxy. They show in
`pg_stat_activity` as `application_name = 'postvec-server'` unless the DSN
sets one.

`poll_only` removes the LISTEN connection. Election and heartbeat still
keep database sessions open, so Neon or Aurora Serverless may stay
awake. Stop the managed workers when scale-to-zero is required.

## 4. Enable or adopt a column

Once the worker has populated `postvec.models`:

```sql
SELECT postvec.enable(
  'public.docs', 'body', 'your-model',
  backfill_mode => 'cursor',
  index_mode => 'auto',
  create_fts_index => true
);

SELECT * FROM postvec.status();
```

`enable()` creates a vector column, or a chunk destination with
`chunking => 'recursive'`. `adopt()` registers an existing vector
column and leaves stored bytes as they are.

For a retired or provider-only space, adopt then
[search the existing space](/docs/guides/bridge). Stored rows stay.
[`migrate()`](/docs/guides/migrate) is a later step, once that search is
working.

Wait until `pending_jobs = 0` before judging ranks. Then search through
the proxy (next section).

`set_format()` refreshes the entry using a document template.
`trigger_mode => 'statement'` (the default) enqueues from transition
tables and is the right choice for bulk loads. `'row'` fires per
changed row. Either mode re-embeds a row only when the source column, a
column referenced by its format template or its primary key changes.
Inserts with a NULL source enqueue nothing.

`backfill_mode => 'cursor'` bounds the initial queue on large tables.
Cursor adoption honors both `backfill => 'missing'` and `'all'`. On
partitioned tables, use row triggers if applications write directly to
partitions; statement triggers cover writes through the parent only.

Source-table RLS must allow the worker: it uses `row_security=off` so
it fails rather than silently skip rows. Ordinary table owners bypass
non-forced RLS. A forced-RLS source needs an appropriately privileged
worker role. Chunk destinations are private until the owner grants
access.

`index_mode => 'auto'` waits for backfill and jobs to drain, then
builds an HNSW index concurrently by default, on its own connection.
`create_vector_index()` builds a blocking index. PostgreSQL has no
concurrent index creation on partitioned parents; use a blocking build
or manage partition indexes explicitly.

Claims and source reads commit before inference. Write-back checks the
source row version and migration target again. Worker statements time
out after ten minutes. A lock wait gives up after five seconds and that
entry retries at the next poll, so one locked table (an `ALTER`, a
`VACUUM FULL`) does not stall the others. Index builds get an hour, and
one that runs out of time is recorded in `status().index_error` rather
than retried. A row the table rejects on write-back (a trigger, a
`CHECK`) retries on its own and dead-letters with the database's error;
the rest of its batch is written.

Primary keys travel through the queue as text, so the triggers and the
worker both render them with `DateStyle = 'ISO, MDY'`, `TimeZone = 'UTC'`
and `IntervalStyle = 'postgres'`, whatever the writing session uses. The
worker's session keeps those settings, so a format template that includes a
timestamp renders it in UTC.
Transient and configuration failures retry with exponential backoff.
Queue jobs dead-letter after five attempts.

`disable()` retains user vectors and chunk data by default.
`convert()` is a stub on managed databases; it names the server's
`/api/convert` endpoint.

## 5. Search

`postvec.search(relation, column, text)` and `postvec.embed(text, model)`
need an embedding the database cannot compute. Set `proxy_port` on a
managed entry (or pass `--proxy <PORT>` with `--sync`) and point the
connections that call them at that port.

```sql
SELECT * FROM postvec.search(
  'public.docs', 'body', 'reset password',
  limit_n => 5
);
SELECT * FROM postvec.search(
  'public.docs', 'body', $1,
  filter => '{"tenant": 7}'
);
SELECT postvec.embed('reset password', 'your-model');
```

The proxy embeds the text with this node's models or the fleet and passes
the vector to the same call as a last argument (`query_vector =>` for
`search()`, `vector =>` for `embed()`). The database still runs the
function the client named, under the client's role: `EXECUTE` privileges,
result column names and prepared statements behave as without the proxy.
Every other byte passes unchanged: authentication, transactions, `COPY` and
cancellation. Clients authenticate against the database with their own
credentials. Each proxy port accepts only the database configured in its
managed entry.

Inference costs money, so before embedding the proxy also refuses a
session whose login role cannot reach `EXECUTE` at all, directly or through
a role it may `SET ROLE` to (checked once per session; the database still
checks every call). `REVOKE EXECUTE ON FUNCTION postvec.embed(text, text, real[]) FROM PUBLIC`
(or `postvec.search(...)`) limits who can use it.

The relation, column and model must be literals. The text may be a
literal or a bind parameter, cast to `text` or `varchar` at most.
Inference accepts UTF-8 text (`client_encoding=UTF8`, or UTF-8 bytes in a
`SQL_ASCII` session). SQL string escapes follow the session's
`standard_conforming_strings` setting. Calls in any other form reach the
database's own `search()`/`embed()`, which raise an error naming the
supported forms. Unqualified relation names must be unique across schemas.
Every execution embeds the text again, so prepared statements cost one
embedding per execution.

Everything else (`status()`, `migrate()`, `adopt()`, DDL) connects to
the database directly. `search_with_vector()` also runs direct: pass a
vector the application already has.

Keyword vs vector mix, GIN and corpus stats: [BM25](/docs/guides/bm25).

### Proxy TLS and authentication

The proxy requires client TLS when the node has a TLS certificate.
`--insecure` permits plain TCP. Startup and TLS negotiation have a
10-second deadline. `proxy_max_connections` defaults to 256 per managed
entry. Upstream TLS uses the entry's `sslmode`, root CA and optional
client certificate/key settings.

SCRAM channel binding cannot pass through a proxy, so the proxy never
offers `SCRAM-SHA-256-PLUS`. A client connected over TLS must turn channel
binding off when the database also uses TLS (every managed provider does):
`channel_binding=disable` for libpq and psql, `channelBinding=disable` for
pgjdbc. Otherwise the client asks for binding, and the proxy refuses the
login with a message saying exactly that. Plain-TCP clients and drivers
without channel binding support (sqlx, node-postgres) need nothing.

Metrics: `postvec_proxy_connections{db}` and
`postvec_proxy_rewrites_total{db,kind}`.

`--proxy-upstream <DSN>` opens a proxy on a node that only proxies
while another node syncs.

## Platforms

| Platform | Notes |
|---|---|
| Amazon RDS / Aurora | Direct instance endpoint. Enable pgvector. `sslmode=verify-full` with the AWS CA. |
| Aurora Serverless, Neon | Set `poll_only`. Worker and election sessions still keep the instance awake. |
| Cloud SQL / AlloyDB | Worker through the auth proxy sidecar or a private IP. |
| Azure Flexible Server | `sslmode=require`. Port 5432, not the built-in PgBouncer on 6432. |
| Supabase | Direct connection, or the session pooler on port 5432 with user `postvec_worker.<project-ref>`. `search(text)` goes through the proxy. |
| Neon | Endpoint host without `-pooler`. `poll_only` as above. |

Use your provider's CA configuration for verified TLS. The test suite runs
against local PostgreSQL; provider role, TLS and network configuration differ
per platform.

## Observe and operate

The dashboard's **Databases** page shows platform, schema version,
leader, heartbeat age, queues, migrations and source-owner grants.
Database credentials and source text are omitted. Job inspection, retry
and model refresh use the loopback admin listener. Open the dashboard
through an SSH tunnel to that port:

```sh
ssh -L 22223:127.0.0.1:22223 server-host
# Open http://127.0.0.1:22223
```

Admin routes are `GET /admin/managed`,
`GET /admin/managed/{name}/jobs`,
`POST /admin/managed/{name}/refresh-models` and
`POST /admin/managed/{name}/retry-dead` with
`{"registry_id":1,"dead_ids":[123]}`. Omitting `dead_ids` retries the
entry's eligible dead letters.

Metrics include `postvec_managed_queue_depth{db}`,
`postvec_managed_jobs_total{db,outcome}`,
`postvec_managed_leader{db}` and
`postvec_managed_heartbeat_age_seconds{db}`. The job counters are
database-wide and survive leadership changes; scrape them from one
node.

```sh
postvec-server managed status --dsn <DSN> --password-file <PATH>
postvec-server managed uninstall --dsn <DSN> --password-file <PATH>
```

`managed status` prints pretty JSON with `schema_version`, `platform`,
`worker_alive`, `heartbeat`, `leader`, `queue_depth` and `dead_letters`.
`worker_alive` is `true` while the last beat is inside 30 seconds. The same
boolean is the first column of every `postvec.status()` row, so a SQL session
and the host command report liveness the same way.

Workers park on schema-version mismatch until `managed install` upgrades the
schema. Uninstall removes managed
triggers and schema objects, retains user vectors and chunks, and
refuses external dependencies. Stop configured workers before
uninstalling.

Migration cutover is explicit. After the swap, an entry that had an ANN
index waits in `awaiting_index`; with `index_mode => 'auto'` the server
builds it concurrently and completes the migration, otherwise build it and
call `migration_finalize()` again.

- [Install](/docs/server/node)
- [SQL functions](/docs/guides/)
- [Search a retired space](/docs/guides/bridge)
- [BM25](/docs/guides/bm25)
- [License](/docs/license)
