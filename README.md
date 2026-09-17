# postvec

A PostgreSQL extension (Rust, [pgrx](https://github.com/pgcentralfoundation/pgrx))
that keeps a [pgvector](https://github.com/pgvector/pgvector) column in sync
with a text column, serves hybrid full-text and vector search in one call, and
converts stored vectors between embedding models in place.

```console
docker run -d -p 5432:5432 -e POSTGRES_PASSWORD=postvec \
  ghcr.io/univec-ai/postvec:pg18-local
```

The local image runs the inference engine and a bundled embedding model inside
the database process.

```sql
CREATE TABLE docs (id bigserial PRIMARY KEY, body text);

SELECT postvec.enable('public.docs', 'body',
                      model => 'sentence-transformers-all-minilm-l6-v2');

INSERT INTO docs (body) VALUES
  ('quarterly revenue guidance was raised'),
  ('embedding model migration without source re-embedding');

SELECT d.*, s.rrf_score
  FROM postvec.search('public.docs', 'body', 'switching vector models') AS s
  JOIN docs AS d ON d.id = s.pk_value::bigint
 ORDER BY s.rrf_score DESC;
```

To change the stored model, convert the existing vectors:

```sql
SELECT postvec.migrate('public.docs', 'body', 'baai-bge-m3');
```

Old rows convert through an embedding-space translator. New writes use the new
model. The columns swap at finalization.

## Contents

| Directory | Role |
|---|---|
| `postvec/` | The extension (pgrx, PostgreSQL 16-18, pgvector >= 0.8) |
| `postvec-cli/` | The `postvec` command: `setup`, `doctor`, `model pull/...`, `provider add/...`, `uninstall` |
| `postvec-server/` | Standalone inference node for remote mode ([README](postvec-server/README.md)). **Business Source License 1.1** (source-available). The rest of the tree uses the PostgreSQL License. |
| `engine/`, `shared/` | In-process inference engine used by embedded mode. Trimmed fork of UniVec's engine ([engine/FORK.md](engine/FORK.md)). |
| `providers/` | Connectors for hosted embedding APIs (OpenAI, Gemini, Cohere, Mistral, Bedrock, OpenRouter). Both inference hosts mount them ([docs](https://postvec.dev/docs/models/providers)). |
| `proto/` | Canonical gRPC contract between postvec and inference nodes |
| `packaging/postvec/` | `.deb` / `.rpm` / container release pipeline |
| `web/` | Public documentation ([postvec.dev/docs](https://postvec.dev/docs/)) |

## Modes

SQL, queue and wire contract are the same in both modes. `postvec.mode`
selects where the model runs and applies at the next worker start.

**Embedded** (`postvec.mode = 'embedded'`, the default). The launcher process
hosts one engine on the database host. Models live on local disk. CPU only.
Source text stays on the host unless a column is bound to an external
provider. After the packages, preload and a restart, `search()` runs with the
bundled model.

**Remote** (`postvec.mode = 'grpc'`). PostgreSQL is a thin client.
[`postvec-server`](postvec-server/README.md) runs the same engine in its own
process, on the same host or on a CPU/GPU fleet. Use this when inference must
have its own crash domain, CPU budget or GPU, or when the database is a
managed service that loads no third-party `.so`. That directory is the BSL
exception; see [LICENSING.md](LICENSING.md).

The extension loads through `shared_preload_libraries`. Self-hosted clusters
and hosts that allow that GUC run the extension in-process. Hosts that refuse
third-party libraries (Amazon RDS, Aurora, Cloud SQL, Azure Flexible Server,
Supabase, Neon and similar) run [postvec-server in managed
mode](https://postvec.dev/docs/server/managed).

## License and models

The extension, CLI, engine, providers and packaging use the PostgreSQL
License in either mode. External embedding providers are opt-in per column.
Credentials live in `0600` files on the inference host.

`postvec-server` uses Business Source License 1.1. Development, testing,
personal use and one 30-day production evaluation per organization are free.
Production use by an organization needs a [postvec Pro](https://univec.ai)
subscription. Each version converts to the PostgreSQL License four years
after release.

`postvec model pull` fetches models from UniVec's registry: an anonymous
public channel for open models, and an authenticated channel (an account at
https://univec.ai) for the commercial conversion models that `migrate()` uses
between proprietary embedding spaces. Those two compiled-in URLs are the
origins the CLI contacts.

Embedded mode is CPU-only. GPU inference runs in remote mode.

## Building

```console
cargo build --workspace                    # CLI, server, engine, shared, registry schema
cd postvec && cargo pgrx test pg18         # the extension test suite
cd postvec && ./ci.sh                      # the full local gate
```

Prerequisites: Rust, cargo-pgrx 0.18.1, a pgrx-managed PostgreSQL 18 with
pgvector built into it, protoc, libssl-dev (the server's TLS listener).
Packaging and images: [packaging/postvec/README.md](packaging/postvec/README.md).

## Documentation

- Docs: [postvec.dev/docs](https://postvec.dev/docs/)
- Install: [packages](https://postvec.dev/docs/install/packages), [Docker](https://postvec.dev/docs/install/docker), [from source](https://postvec.dev/docs/install/source)
- postvec-server: [postvec.dev/docs/server](https://postvec.dev/docs/server/)
- Downloads: [postvec.dev/download](https://postvec.dev/download)

## Provenance

postvec was developed at UniVec. This repository is the history since it was
split from the private monorepo; postvec-server is Business Source License 1.1
on every revision.
