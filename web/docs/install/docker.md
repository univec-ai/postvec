---
title: Install with Docker
description: Run the official postvec images and the rules that keep data safe.
---

# Install with Docker

The embedded image is the shortest self-contained deployment: PostgreSQL,
pgvector, postvec, the CLI, ONNX Runtime, and MiniLM.

## Embedded (all-in-one)

```bash
docker run -d --name postvec \
  -e POSTGRES_PASSWORD_FILE=/run/secrets/postgres-password \
  -e POSTGRES_DB=app \
  -v postvec-data:/var/lib/postgresql \
  -p 127.0.0.1:5432:5432 \
  ghcr.io/univec-ai/postvec:0.1.0-1-pg18-embedded
```

For a local demo, `-e POSTGRES_PASSWORD=demo` is fine. Bind the port to
loopback.

<div v-pre>

```bash
docker inspect --format '{{.State.Health.Status}}' postvec
docker exec postvec postvec-healthcheck
```

</div>

::: tip Expected
State becomes `healthy`. The extension exists in `POSTGRES_DB` **only on
first initialization** of an empty volume.
:::

The first-hour SQL is in the [quick start](/docs/quickstart).

## Remote image

Omit `-embedded` and point at ninference:

```bash
docker run -d --name postvec \
  -e POSTGRES_PASSWORD=demo \
  -e POSTGRES_DB=app \
  -e POSTVEC_GRPC_ENDPOINTS=10.0.0.20:33333 \
  -e POSTVEC_HTTP_ENDPOINTS=https://10.0.0.20:22222 \
  -v postvec-data:/var/lib/postgresql \
  -p 127.0.0.1:5432:5432 \
  ghcr.io/univec-ai/postvec:0.1.0-1-pg18
```

## Volume path (easy to get wrong)

| PostgreSQL major | Mount |
|---|---|
| 18 | `/var/lib/postgresql` |
| 16 or 17 | `/var/lib/postgresql/data` |

::: danger Don't change major by changing the image tag
A 16/17 data directory is not a 18 data directory. Use `pg_upgrade` or
dump/restore. A wrong mount path **silently loses data** on restart.
:::

## Environment

Every `POSTVEC_*` variable also accepts the official `_FILE` secret form.

| Variable | Default | Meaning |
|---|---|---|
| `POSTVEC_MODE` | `grpc`; `embedded` in `*-embedded` images | Inference mode |
| `POSTVEC_DATABASES` | `POSTGRES_DB` | Comma-separated worker databases |
| `POSTVEC_GRPC_ENDPOINTS` | unset | Remote gRPC |
| `POSTVEC_HTTP_ENDPOINTS` | unset | Remote `/config` |
| `POSTVEC_NINFERENCE_PATH` | `/opt/postvec/ninference` | Embedded engine root |
| `POSTVEC_EMBEDDED_MODELS` | bundled model | Preload allow-list |
| `POSTVEC_SHARED_PRELOAD_LIBRARIES` | unset | Existing preloads; postvec is appended |
| `POSTVEC_CREATE_EXTENSION` | `1` | `0` skips first-run `CREATE EXTENSION` |

Invalid mode, an empty database list, or a newline in a value exits **64**.
Embedded mode with no engine assets exits **78**. Embedded listeners stay
loopback-only.

## Adding another database

Init scripts do **not** rerun on an existing volume.

```sql
CREATE DATABASE analytics;
\c analytics
CREATE EXTENSION postvec CASCADE;
```

Include `analytics` in `POSTVEC_DATABASES` and recreate the container (or
pass `-c postvec.database=...` yourself). Restart is required: the database
list is POSTMASTER.

## Diagnose inside the image

The official PostgreSQL image is not `postgresql-common`, so plain
`postvec doctor` finds no cluster.

```bash
docker exec postvec postvec-healthcheck

docker exec -u postgres postvec \
  postvec doctor \
  --database-url 'postgresql:///app?host=/var/run/postgresql' \
  --database app
```

## Models that survive recreation

Pulled models land under the image's engine root. A **named volume** on
`/opt/postvec/ninference/models` copies the bundled MiniLM in. A **bind mount or PVC masks**
the bundled directory — you must copy MiniLM up yourself.

## Tags

- Pinned: `ghcr.io/univec-ai/postvec:0.1.0-1-pg18-embedded`
- Moving: `ghcr.io/univec-ai/postvec:pg18-embedded`
- No `latest`. Pin by digest in production.

Pulling a new image never runs `ALTER EXTENSION` for you. See
[upgrade](/docs/install/upgrade).
