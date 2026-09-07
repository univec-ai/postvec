---
title: Install with Docker
description: postvec container images for PostgreSQL 16, 17 and 18. Volume layouts and runtime configuration.
---

# Install with Docker

The image includes PostgreSQL, pgvector, postvec, the CLI, ONNX Runtime
and MiniLM. The data-directory mount follows the official `postgres`
image for the selected major.

An existing self-hosted cluster uses [packages](/docs/install/packages).
RDS, Aurora, Cloud SQL, Azure, Supabase and Neon use
[managed PostgreSQL](/docs/server/managed).

Commands use the planned `0.1.0-1` tags. The [release artifacts
page](/download) reports whether they are published.

## Local (all-in-one)

<PgSnippet
  id="docker-embedded"
  caption="A local demo can pass POSTGRES_PASSWORD=demo instead of a secret file. The example binds the port to loopback."
/>

Wait until health is `healthy`:

<div v-pre>

```bash
docker inspect --format '{{.State.Health.Status}}' postvec
```

</div>

The extension exists in `POSTGRES_DB` **only on first initialization** of
an empty volume. First SQL steps: [quick start local](/docs/quickstart).

:::: info Optional
`docker exec postvec postvec-healthcheck` exits 0 when the worker and
engine are ready. Image attestations:
[verify artifacts](/docs/install/verify).
::::

## Remote image

Use the `-remote` tag and point at your `postvec-server` nodes:

<PgSnippet id="docker-remote" />

The companion server is published too, as
`ghcr.io/univec-ai/postvec-server`, composed of the same packages and
serving the bundled model out of the box.
[Docker](/docs/server/docker) covers it; the release's remote images
are smoke-tested against exactly that image. Both containers together:
[quick start remote](/docs/quickstart-remote).

## Volume path

| PostgreSQL major | Mount |
|---|---|
| 18 | `/var/lib/postgresql` |
| 16 or 17 | `/var/lib/postgresql/data` |

:::: danger PostgreSQL majors require distinct volume layouts
Each major needs its own data directory. Major upgrades require
`pg_upgrade` or dump/restore. An incorrect mount path can create a new
empty data directory and conceal the existing data. Never change major
by retagging an existing volume.
::::

## Environment

Every `POSTVEC_*` variable also accepts the official `_FILE` secret form.

| Variable | Default | Meaning |
|---|---|---|
| `POSTVEC_MODE` | pinned by the image: `grpc` in `*-remote`, `embedded` in `*-local` | Inference mode |
| `POSTVEC_DATABASES` | `POSTGRES_DB` | Comma-separated worker databases |
| `POSTVEC_GRPC_ENDPOINTS` | unset | Remote gRPC |
| `POSTVEC_HTTP_ENDPOINTS` | unset | Remote `/config` |
| `POSTVEC_PATH` | `/opt/postvec` | Embedded engine root |
| `POSTVEC_EMBEDDED_MODELS` | bundled model | Preload allow-list |
| `POSTVEC_SHARED_PRELOAD_LIBRARIES` | unset | Existing preloads; postvec is appended |
| `POSTVEC_CREATE_EXTENSION` | `1` | `0` skips first-run `CREATE EXTENSION` |

Both images pin `POSTVEC_MODE`. The `-remote` tag pins `grpc` because that
image has no engine assets. The `-local` tag pins `embedded` and includes
ONNX Runtime and MiniLM.

An invalid mode, an **empty** mode, an empty database list or a newline in a
value exits **64**. Compose renders an undefined interpolation as the empty
string; treating that as unset would silently switch a `*-local` image
to `grpc`. Embedded mode with no engine assets exits **78**. Embedded
listeners stay loopback-only.

## External providers

To serve a hosted model from a container (OpenAI, Cohere, Bedrock, Gemini,
Mistral, OpenRouter or UniVec), give it a `providers.d` and a key.
The lighter path keeps the key out of the filesystem and names a variable the
postmaster already has:

<PgSnippet id="docker-provider" />

with `providers.d/openai.toml` carrying `api_key_env = "OPENAI_API_KEY"`.
`postvec provider add openai --model text-embedding-3-small --path "$PWD"`
writes that file for you. Without a mount, `docker exec -it postvec postvec
provider add …` writes into the container's own `/etc/postvec/providers.d`
(the `-local` image sets `POSTVEC_PROVIDERS_PATH`); that lives in the
container's writable layer, not in a volume, and is gone with the container.
The exec shell is root, so commands from the guides run without `sudo`.

| Mount | Contents | Permissions |
|---|---|---|
| `/etc/postvec/providers.d` | One `*.toml` connector file per provider | Directory `0700`, files `0600`, owned by the container's `postgres` uid (`999`) |
| Any path you reference | The `api_key_file` a connector points at | `0600`, same owner |

The host refuses a connector file, or a key file it references, that is
readable by other users, so a bind mount has to carry the right mode and
owner. With Compose secrets, mount the secret with an explicit `mode: 0400`
and `uid: "999"` and reference it as `api_key_file`. The image's `_FILE`
convention applies to PostgreSQL's own variables (`POSTGRES_PASSWORD_FILE`
and the rest).

Kubernetes projected secret volumes are symlinks into a `..data` directory
and are mounted world-readable, so they cannot be referenced as
`api_key_file`. Use `api_key_env` there.

Walkthrough: [external providers](/docs/models/providers). Per-provider
setup: [OpenAI](/docs/models/openai), [Cohere](/docs/models/cohere),
[Amazon Bedrock](/docs/models/aws), [Gemini](/docs/models/gemini),
[Mistral](/docs/models/mistral), [OpenRouter](/docs/models/openrouter),
[UniVec](/docs/models/univec). Conversion entries use the same mount and
permissions.

## Adding another database

Init scripts run only on an empty volume.

```sql
CREATE DATABASE analytics;
\c analytics
CREATE EXTENSION postvec CASCADE;
```

Include `analytics` in `POSTVEC_DATABASES` and recreate the container, or
pass `-c postvec.database=...`. Restart is required because the database
list is POSTMASTER.

## Diagnose inside the image {#diagnose}

:::: info Optional
The official PostgreSQL image has no `pg_lsclusters`, so a plain
`postvec doctor` finds no cluster. Pass a socket URL as below.

:::: code-group

```bash [healthcheck]
docker exec postvec postvec-healthcheck
```

```bash [doctor]
docker exec -u postgres postvec \
  postvec doctor \
  --database-url 'postgresql:///app?host=/var/run/postgresql' \
  --database app
```

::::
::::

## Persistent model storage

Pulled models are stored under the image's engine root. A **named volume** on
`/opt/postvec/models` copies the bundled MiniLM in. A **bind mount or
PVC** masks the bundled directory, so MiniLM must be copied into the
mounted directory.

## Tags

<PgSnippet id="docker-tags" />

Every tag includes the PostgreSQL major. Pin the versioned tag in
production, preferably by digest. A preview tag may not resolve until
the release is published.

After you pull a new image, run `ALTER EXTENSION` on the existing
volume. See [upgrade](/docs/install/upgrade).
