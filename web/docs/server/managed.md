---
title: Managed PostgreSQL
description: Run automatic embeddings and model migrations on PostgreSQL without a native extension.
---

# Managed PostgreSQL

postvec-server runs the sync worker outside PostgreSQL, using a plain SQL schema
and pgvector on PostgreSQL 16–18. It supports backfill, automatic updates,
recursive chunking, migration by conversion or re-embedding, fleet failover and
a pgwire proxy for single-call `search(text)`.
No postvec library, preload setting or database restart is required.

## Install and run

Have the database administrator enable pgvector and create a login role with
`CREATE` on the application database. Install as that role; it owns the managed
schema. The worker needs ownership of source tables, or membership in their
owning roles. The installer prints the applicable `GRANT owner TO worker`
commands; a superuser (or a role with `ADMIN OPTION` on the table owner)
must run them — a table owner cannot grant their own role.

```sh
chmod 600 /etc/postvec-server/database.pw
postvec-server managed install \
  --dsn 'postgresql://postvec_worker@db.example/app?sslmode=verify-full' \
  --password-file /etc/postvec-server/database.pw
```

Add this to `/etc/postvec-server/config.json`:

```json
{
  "managed": [{
    "name": "prod",
    "dsn": "postgresql://postvec_worker@db.example/app?sslmode=verify-full",
    "password_file": "/etc/postvec-server/database.pw",
    "sync": true,
    "proxy_port": 5433,
    "poll_interval_ms": 2000,
    "poll_only": false,
    "batch_size": 64,
    "index_concurrently": true
  }]
}
```

Restart postvec-server. Its existing models, provider credentials and gossip
fleet serve the worker. For one database, `postvec-server serve --sync <DSN>`
is a shortcut; `--poll-only` disables LISTEN. Multiple databases use the file.
Passwords never enter the schema or dashboard. Password files must be regular
files, mode `0600`, readable by the service account; symlinks are refused.
Relative password paths resolve against the engine root.

Use a direct database endpoint, not a transaction-pooling endpoint: election
requires a session advisory lock. All nodes configured for a database share
one leader, even when their DSNs use different credentials or host aliases.
Standbys take over after the leader's database session closes. PostgreSQL's
TCP keepalive settings determine detection time for a severed network.

`poll_only` removes the LISTEN connection, but election and heartbeat still
keep database sessions open. It does **not** guarantee Neon or Aurora scale to
zero. Stop the managed workers when scale-to-zero is required.

## Use

Once the worker has populated `postvec.models`:

```sql
SELECT postvec.enable('public.docs', 'body', 'your-model',
                      backfill_mode => 'cursor', index_mode => 'auto');
SELECT * FROM postvec.status();
SELECT postvec.migrate('public.docs', 'body', 'new-model', strategy => 'convert');
SELECT * FROM postvec.migration_status();
SELECT postvec.migration_finalize(1);
```

`adopt()` registers an existing vector column. `enable()` creates one, or a
chunk destination with `chunking => 'recursive'`. `set_format()` refreshes the
entry using a document template. `trigger_mode => 'statement'` (the default)
enqueues from transition tables, which is the right choice for bulk loads;
`'row'` fires per changed row. Use `backfill_mode => 'cursor'` to bound the
initial queue on large tables.

Claims and source reads commit before inference. Write-back checks the source
row version and migration target again. Transient/configuration failures retry
with exponential backoff; queue jobs dead-letter after five attempts. Oversized
embedding inputs (1 MiB per item, 8 MiB per batch) dead-letter without loading
the full value into the worker. Recursive splitting accepts documents up to
32 MiB. Input-length failures split batches to isolate the offending row.

Source-table RLS must not restrict the worker: it uses `row_security=off` to
fail rather than silently skip rows. Ordinary table owners bypass non-forced
RLS; a forced-RLS source needs an appropriately privileged worker role.
Chunk destinations are private until the owner grants access; their SELECT
policy checks source visibility for non-owner readers.

`index_mode => 'auto'` waits for backfill and jobs to drain and builds an HNSW
index concurrently by default. A saved build intent supports invalid-index
recovery. Other index errors appear in `index_error`; resolve the cause and
clear that field to retry. `create_vector_index()` explicitly builds a blocking
index. PostgreSQL does not support concurrent index creation on partitioned
parents; use a blocking build or manage partition indexes explicitly.

Migration cutover is explicit and refuses old-column constraints or metadata
that dropping the column would discard. Dependent views block cutover. Aborting
rebuilds the old model's vectors so writes received during migration converge.
`disable()` retains user vectors and chunk data by default; managed destinations
are dropped explicitly after reviewing their dependencies.

## Search through the proxy

`postvec.search(relation, column, text)` and `postvec.embed(text, model)` need
an embedding the database cannot compute. Set `proxy_port` on a managed entry
(or pass `--proxy <PORT>` with `--sync`, or `--proxy-upstream <DSN>` on a node
that only proxies) and point the connections that call them at that port. The
proxy rewrites each call into `search_with_vector()` or a vector literal,
embedding the text with this node's models or the fleet, and forwards every
other byte unchanged: authentication, transactions, prepared statements,
`COPY` and cancellation all pass through. Clients authenticate against the
database with their own credentials; the proxy has none of them.

```sql
SELECT * FROM postvec.search('public.docs', 'body', 'reset password', limit_n => 5);
SELECT * FROM postvec.search('public.docs', 'body', $1, filter => '{"tenant": 7}');
SELECT postvec.embed('reset password', 'your-model');
```

The relation, column and model must be literals; the text may be a literal or
a bind parameter. Calls in any other form, and calls made without the proxy,
raise an error naming the proxy. Unqualified relation names must be unique
across schemas. Every execution embeds the text again, so prepared statements
cost one embedding per execution.

The proxy uses the node's TLS certificate, or plain TCP with `--insecure`, and
connects to the database with the entry's `sslmode`. SCRAM channel binding
cannot pass through a proxy, so the proxy never offers `SCRAM-SHA-256-PLUS`.
Default libpq (`channel_binding=prefer`) falls back to `SCRAM-SHA-256`.
Clients that set `channel_binding=require` will fail to authenticate.
Metrics: `postvec_proxy_connections{db}` and
`postvec_proxy_rewrites_total{db,kind}`.

## Observe and operate

The dashboard's **Databases** page shows platform, schema version, leader,
heartbeat age, queues, migrations and source-owner grants. Database credentials
and source text are omitted. Job inspection, retry and model refresh use the
loopback admin listener. Open the dashboard through an SSH tunnel to that port:

```sh
ssh -L 22223:127.0.0.1:22223 server-host
# Open http://127.0.0.1:22223
```

Admin routes are `GET /admin/managed`, `GET /admin/managed/{name}/jobs`,
`POST /admin/managed/{name}/refresh-models`, and
`POST /admin/managed/{name}/retry-dead` with
`{"registry_id":1,"dead_ids":[123]}`. Omitting `dead_ids` retries the entry's
eligible dead letters. Database actions remain off the public listener.

Metrics include `postvec_managed_queue_depth{db}`,
`postvec_managed_jobs_total{db,outcome}`, `postvec_managed_leader{db}`, and
`postvec_managed_heartbeat_age_seconds{db}`. The job counters are database-wide
and survive leadership changes; avoid summing copies scraped from multiple nodes.

```sh
postvec-server managed status --dsn <DSN> --password-file <PATH>
postvec-server managed uninstall --dsn <DSN> --password-file <PATH>
```

Installation is transactional and repeatable. Rerun `managed install` to add the
lifecycle functions to an earlier version-1 managed installation. The installer
refuses an extension database, unrelated schema or unsupported schema version.
Workers park on schema-version mismatch. Uninstall removes managed triggers and
schema objects, retains user vectors/chunks, and refuses external dependencies.
Stop configured workers before uninstalling.

Use your provider's CA configuration for verified TLS. RDS/Aurora, Cloud SQL,
Azure, Supabase and Neon also require a network route and suitable database-role
permissions. Tests exercise local PostgreSQL; verify your provider's actual
role, TLS and network configuration before deployment.

Organization production use requires postvec Pro. Personal noncommercial use,
non-production environments and one 30-day production evaluation per organization
are free. See the [license terms](https://github.com/univec-ai/postvec/blob/main/LICENSING.md).
There is no runtime license check or telemetry.
