---
title: Managed PostgreSQL schema
description: Install the postvec SQL schema on PostgreSQL without the native extension.
---

# Managed PostgreSQL schema

`postvec-server managed` installs the plain SQL foundation on PostgreSQL 16–18
with pgvector. It needs no postvec library, preload setting, or database restart.
The SQL is embedded in the existing server binary and ships in its packages
and image.

**This release implements the schema and installer only.** It does not start a
sync worker or proxy, populate the model catalogue, or provide managed
`enable()`, `adopt()`, or migration lifecycle commands. Ordinary extension
installations retain their complete feature set. Automatic embeddings on
managed databases arrive with the worker phase.

## Install

Have the database administrator enable pgvector and create a dedicated login
role with `CREATE` on the application database. Install as that role; it owns
the managed schema and its functions. Source-table ownership or membership in
the owning role will be needed when the worker is added.

```sh
chmod 600 /etc/postvec-server/database.pw
postvec-server managed install \
  --dsn 'postgresql://postvec_worker@db.example/app?sslmode=verify-full' \
  --password-file /etc/postvec-server/database.pw
postvec-server managed status \
  --dsn 'postgresql://postvec_worker@db.example/app?sslmode=verify-full' \
  --password-file /etc/postvec-server/database.pw
```

Use your provider's CA configuration for TLS verification. The server host
needs a route to PostgreSQL; installation opens no inbound listener. Passwords
stay outside the schema. Password files must be regular files with mode `0600`;
symlinks are refused. A DSN password is accepted with a warning.

Installation is transactional and serialized. Repeating it preserves data.
The installer refuses an existing postvec extension, an unrelated schema, or
an unsupported schema version. Version 1 is the first managed schema; no older
managed upgrade is needed yet. Future versions must supply explicit migrations.

The installer prints grants for existing source-table owner roles the worker
does not inherit. Apply only the grants relevant to the tables you intend to
manage: role membership grants the privileges of that role. It never applies
those grants itself or creates a database role.

## SQL available in this phase

The durable registry, queue, dead-letter, migration, model, and heartbeat
tables share their definitions with the extension. `schema_version` records
the schema contract and installation mode. `settings` holds non-secret
`key text PRIMARY KEY, value jsonb NOT NULL` rows; currently `platform` is
populated by best-effort detection. An unrecognised platform is `postgresql`.

Available helpers are `search_with_vector()`, `status()`, `stats()`,
`retry_dead()`, `migration_status()`, and `refresh_models()`. Search uses the
caller's table privileges and row security. `status()` adds `worker_alive`
before the extension's usual columns; it is false without a heartbeat newer
than 30 seconds. CLI status also reports an empty leader field until the
worker and fleet phases are implemented.

`retry_dead()` and `refresh_models()` are restricted to the installing role
by default. Retry additionally requires ownership of the source table and
preserves queue deduplication. `refresh_models()` sends a notification;
there is no listener until the worker phase.

For a provisioned registry entry and existing vectors, the two-call search
path is the server's [HTTP embeddings endpoint](/docs/server/http-api), then
`postvec.search_with_vector(relation, column, query_vector::real[], query_text)`.
The installer does not provision those entries. Single-call `search(text)`
through a proxy is a later phase, and no proxy port is opened here.

## Remove

```sh
postvec-server managed uninstall \
  --dsn 'postgresql://postvec_worker@db.example/app?sslmode=verify-full' \
  --password-file /etc/postvec-server/database.pw
```

This removes managed triggers, functions, and the schema in one transaction.
User tables, vector columns, and chunk data remain. External dependencies,
such as a user view of a registry table, cause a rollback instead of a cascading
removal. Remove those dependencies explicitly before retrying. The `postvec`
schema is reserved for managed objects.

## Provider notes

Use the direct database endpoint for installation. RDS/Aurora, Cloud SQL, and
Azure may require an administrator to enable pgvector or grant database
`CREATE`. Supabase's direct endpoint avoids pooler restrictions. Neon and
Aurora Serverless may wake for installation and status checks; no persistent
LISTEN connection is opened in this phase. Test with your provider's actual
role and TLS settings before production; the automated suite uses local PostgreSQL.

Organization production use of postvec-server requires postvec Pro. Personal
noncommercial use, non-production environments, and one 30-day production
evaluation per organization are free. See the
[license terms](https://github.com/univec-ai/postvec/blob/main/LICENSING.md).
No license key or telemetry is added.
