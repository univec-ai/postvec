---
title: Install with Docker
description: postvec container images for PostgreSQL 16, 17 and 18. Volume layouts and runtime configuration.
---

# Install with Docker

This image includes PostgreSQL, pgvector, postvec and the CLI.
The `-local` tag also includes [ONNX Runtime](https://github.com/microsoft/onnxruntime/releases)
and [MiniLM](https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2).
The data-directory mount follows the official `postgres` image for the
selected major.

Inference in the `-local` tag runs in a thread inside the PostgreSQL
process. The `-remote` tag sends inference to
[postvec-server](/docs/server/), a separate process that serves a CPU or
GPU fleet and keeps a model fault out of the database.

- For an existing self-hosted cluster use [packages](/docs/install/packages).
- RDS, Aurora, Cloud SQL, Azure, Supabase and Neon use
  [managed PostgreSQL](/docs/server/managed).

Commands use the current release tag (see the [release artifacts page](/download)
for every published release).

## Local (all-in-one)

<PgSnippet
  id="docker-embedded"
  caption="A local demo can pass POSTGRES_PASSWORD=demo instead of a secret file. The example binds the port to loopback."
/>

<div v-pre>

```bash [wait for healthy]
docker inspect --format '{{.State.Health.Status}}' postvec
```

</div>

The extension exists in `POSTGRES_DB` **only on first initialization** of
an empty volume. First SQL steps: [quick start local](/docs/quickstart).

:::: info Optional
`docker exec postvec postvec-healthcheck` exits 0 when six properties
hold: the extension is installed once, the loaded library version equals
the installed SQL version, the running `postvec.mode` equals
`POSTVEC_MODE`, the build supports embedded mode, the last worker
heartbeat is inside the budget and, in embedded mode, the first name in
`POSTVEC_EMBEDDED_MODELS` is installed. It exits 1 and names the failing
property otherwise. Image attestations:
[verify artifacts](/docs/install/verify).
::::

## Remote image

Prerequisites: [postvec-server](/docs/server/docker) running (or
[quick start remote](/docs/quickstart-remote)).

Use the `-remote` tag and point at your `postvec-server` nodes:

<PgSnippet id="docker-remote" />

## Volume path

| PostgreSQL major | Mount |
|---|---|
| 18 | `/var/lib/postgresql` |
| 16 or 17 | `/var/lib/postgresql/data` |

:::: danger PostgreSQL majors require distinct volumes
Each major needs its own data directory. A major upgrade uses
`pg_upgrade` or dump/restore. An incorrect mount path can create a new
empty data directory and conceal existing data. Keep the major and the
volume together; retagging an existing volume to another major is
unsupported.
::::

## Environment

`POSTGRES_USER`, `POSTGRES_DB`, `POSTVEC_DATABASES`,
`POSTVEC_SHARED_PRELOAD_LIBRARIES`, `POSTVEC_GRPC_ENDPOINTS` and
`POSTVEC_HTTP_ENDPOINTS` accept the `_FILE` secret form. The file is read when
the plain variable is unset; setting both forms is an error. Any other
`POSTVEC_*_FILE` is refused at start: those variables are pinned by the image
or read by `docker exec` commands, which never see the entrypoint.

| Variable | Default | Meaning |
|---|---|---|
| `POSTVEC_MODE` | pinned by the image: `grpc` in `*-remote`, `embedded` in `*-local` | Inference mode |
| `POSTVEC_DATABASES` | `POSTGRES_DB` | Comma-separated worker databases |
| `POSTVEC_GRPC_ENDPOINTS` | unset | Remote gRPC |
| `POSTVEC_HTTP_ENDPOINTS` | unset | Remote `/config` |
| `POSTVEC_PATH` | `/opt/postvec` | Embedded engine root |
| `POSTVEC_PROVIDERS_PATH` | `/etc/postvec/providers.d` in `*-local`, unset in `*-remote` | Directory `postvec provider` reads when `--path` is absent |
| `POSTVEC_EMBEDDED_MODELS` | bundled model | Preload allow-list |
| `POSTVEC_SHARED_PRELOAD_LIBRARIES` | unset | Existing preloads; postvec is appended |
| `POSTVEC_CREATE_EXTENSION` | `1` | `0` skips first-run `CREATE EXTENSION` |
| `POSTVEC_HEALTHCHECK_DATABASE` | `POSTGRES_DB` | Database `postvec-healthcheck` connects to |
| `POSTVEC_HEALTHCHECK_BEAT_AGE` | unset; heartbeat interval + three poll ticks + 2 s | Worker heartbeat budget in seconds |

Each image sets `POSTVEC_MODE`:

- `-remote` sets `mode=grpc` and contains no engine assets
- `-local` sets `mode=embedded` and includes ONNX Runtime and MiniLM

## External providers

To use a hosted embedding API (OpenAI, Cohere, Bedrock, Gemini, Mistral,
OpenRouter or UniVec), mount a `providers.d` directory that holds the
API key. Setup: [external providers](/docs/models/providers).

<PgSnippet id="docker-provider" />

| Mount | Contents | Permissions |
|---|---|---|
| `/etc/postvec/providers.d` | One `*.toml` connector file per provider | Directory `0700`, files `0600`, owned by the container's `postgres` uid (`999`) |
| Any path you reference | The `api_key_file` a connector points at | `0600`, same owner |

:::: danger Connector files permissions and groups
The host refuses a connector file (or a key file it references) that is
readable by other users, so a bind mount has to carry the right mode and
owner. With Compose secrets, mount the secret with an explicit
`mode: 0400` and `uid: "999"` and reference it as `api_key_file`.
`POSTGRES_PASSWORD_FILE` and the other PostgreSQL variables use the
official entrypoint's `_FILE` handling; postvec's own are described
above. Either way the file has to be readable by the container's
`postgres` uid and by nobody else.

Kubernetes projected secret volumes are symlinks into a `..data`
directory and are mounted world-readable, so they cannot be referenced
as `api_key_file`. Use `api_key_env` there.
::::

Per-provider setup: [OpenAI](/docs/models/openai), [Cohere](/docs/models/cohere),
[Amazon Bedrock](/docs/models/aws), [Gemini](/docs/models/gemini),
[Mistral](/docs/models/mistral), [OpenRouter](/docs/models/openrouter),
[UniVec](/docs/models/univec). Conversion entries use the same mount and
permissions.

On postvec-server the same files live under the engine root:
[models on postvec-server](/docs/server/models).

## Adding another database

Init scripts run only on an empty volume.

```sql
CREATE DATABASE analytics;
\c analytics
CREATE EXTENSION postvec CASCADE;
```

Include `analytics` in `POSTVEC_DATABASES` and recreate the container, or
pass `-c postvec.database=...`. The entrypoint passes this list on the
`postgres` command line, so a new value applies at the next container
start.

## Diagnose inside the image {#diagnose}

The official PostgreSQL image omits `pg_lsclusters`, so a plain
`postvec doctor` finds no cluster. Pass a socket URL:

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

## Persistent model storage

Pulled models are stored under the image's engine root. A **named volume**
on `/opt/postvec/models` copies the bundled MiniLM in. A **bind mount or
PVC** masks the bundled directory, so MiniLM must be copied into the
mounted directory.

## Tags

<PgSnippet id="docker-tags" />

Every database-image tag carries the PostgreSQL major; the moving tags
are `pgNN-local` and `pgNN-remote`. Pin the versioned tag in production,
preferably by digest. The `postvec-server` image has no PostgreSQL major,
so its versioned tag is the release identity and its moving tag is
`latest`. [postvec-server](/docs/server/) explains what runs in that
image. A preview tag may not resolve until the release is published.

After you pull a new image, run `ALTER EXTENSION` on the existing
volume. See [upgrade](/docs/install/upgrade).
