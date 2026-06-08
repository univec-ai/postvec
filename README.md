# postvec

Make a text column semantic — and keep it that way when your embedding model
changes.

postvec is a PostgreSQL extension (Rust, [pgrx](https://github.com/pgcentralfoundation/pgrx))
that creates or adopts a [pgvector](https://github.com/pgvector/pgvector)
column, keeps it synchronized as rows change, serves hybrid full-text/vector
search in one call, and — the part nothing else does — **migrates stored
vectors between embedding models in place**, converting the vectors themselves
instead of re-embedding the source text.

```console
docker run -d -p 5432:5432 -e POSTGRES_PASSWORD=postvec \
  ghcr.io/univec-ai/postvec:pg18-local
```

No API key, no external service - the local image carries the inference
engine and a bundled embedding model in-process:

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

And when the model that produced your vectors is deprecated, expensive, or
simply worse than what you want next:

```sql
SELECT postvec.migrate('public.docs', 'body', 'baai-bge-m3');
```

Old rows are converted in place through an embedding-space translator; fresh
writes route to the new model; the columns swap at finalization. No corpus
re-embedding, no shadow index rebuild.

## What's here

| Directory | |
|---|---|
| `postvec/` | The extension (pgrx, PostgreSQL 16–18, pgvector ≥ 0.8) |
| `postvec-cli/` | The `postvec` command: `setup`, `doctor`, `model pull/…`, `provider add/…`, `uninstall` |
| `postvec-server/` | The standalone inference node remote mode dials ([README](postvec-server/README.md)) — **Business Source License 1.1** (source-available), not the PostgreSQL License the rest uses |
| `engine/`, `shared/` | The in-process inference engine embedded mode uses — a trimmed fork of UniVec's engine ([engine/FORK.md](engine/FORK.md)) |
| `providers/` | Connectors for hosted embedding APIs (OpenAI, Gemini, Cohere, Mistral, Bedrock, OpenRouter), mounted by both inference hosts ([docs](docs/external-providers-usage.md)) |
| `proto/` | The canonical gRPC contract between postvec and inference nodes |
| `packaging/postvec/` | The `.deb`/`.rpm`/container release pipeline |
| `docs/` | Operator documentation — start at [docs/postvec-description.md](docs/postvec-description.md), or [docs/postvec-server.md](docs/postvec-server.md) to run the inference nodes |

## Two deployment modes

- **Embedded** (`postvec.mode = 'embedded'`, the default): one engine hosted
  inside the PostgreSQL launcher process, CPU-only, models on local disk, no
  text leaves the host (unless you bind a column to an external provider).
  Install the packages, preload the library, restart —
  `search()` works with nothing else configured. Traded against a shared
  CPU/memory/failure domain with PostgreSQL.
- **Remote** (`postvec.mode = 'grpc'`): inference runs on separate nodes and
  PostgreSQL stays a thin client — the shape to move to when inference should
  not share a crash domain, a CPU budget or a GPU with the database. Those
  nodes are [`postvec-server`](postvec-server/README.md): same engine, same
  wire contract, its own process, no model hub. It is the one directory in
  this repository that is **not** PostgreSQL-licensed; see [LICENSE](LICENSE).

Same SQL, same queue, same wire contract in both modes.

## Honest boundaries

- Requires `shared_preload_libraries`, so no RDS/Aurora or other managed
  services that do not allow it.
- The extension is PostgreSQL-licensed — unconditionally, in either mode — and
  phones nothing home: no telemetry, no licence check, and no provider API key
  in the database — external embedding providers are opt-in per column, and
  their credentials live only in the inference layer, in `0600` files.
  `postvec-server` is under the Business Source License 1.1 (source-available)
  and does the same.
- `postvec model pull` fetches models from UniVec's model registry: an
  anonymous public channel for open models, and an authenticated channel
  (an account at https://univec.ai) for the commercial conversion models that
  power `migrate()` between proprietary embedding spaces. Those two compiled-in
  URLs are the only origins the CLI contacts.
- Embedded mode is CPU-only; use remote mode for GPU inference.

## Building

```console
cargo build --workspace                    # CLI, server, engine, shared, registry schema
cd postvec && cargo pgrx test pg18         # the extension test suite
cd postvec && ./ci.sh                      # the full local gate
```

Prerequisites: Rust, cargo-pgrx 0.18.1, a pgrx-managed PostgreSQL 18 with
pgvector built into it, protoc, libssl-dev (the server's TLS listener). Packaging and images:
[packaging/postvec/README.md](packaging/postvec/README.md).

## Provenance

postvec was developed inside UniVec's private monorepo and published as a
squashed initial commit. Development history before v0.1.0 is not public.
