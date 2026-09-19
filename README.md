# postvec

Postvec is an extension for fused hybrid search: it combines BM25 and semantic search in one SQL call and keeps vectors in sync with your text automatically. You can also use it to migrate vectors without re-embedding of text with direct conversion between embedding formats.

Inference runs locally inside PostgreSQL or remote on [postvec-server](postvec-server/README.md) or through hosted embedding APIs.

```console
docker run -d --name postvec -p 5432:5432 -e POSTGRES_PASSWORD=postvec \
  ghcr.io/univec-ai/postvec:pg18-local
```

Wait until `docker inspect --format '{{.State.Health.Status}}' postvec` prints `healthy`. Then:

```sql
CREATE TABLE docs (id bigserial PRIMARY KEY, body text);

SELECT postvec.enable(
  'public.docs', 'body',
  model => 'sentence-transformers-all-minilm-l6-v2'
);

INSERT INTO docs (body) VALUES
  ('quarterly revenue guidance was raised'),
  ('embedding model migration without source re-embedding');

-- vectors fill after commit; wait until postvec.status() shows pending_jobs = 0

SELECT d.*, s.rrf_score
  FROM postvec.search('public.docs', 'body', 'switching vector models') AS s
  JOIN docs AS d ON d.id = s.pk_value::bigint
 ORDER BY s.rrf_score DESC;
```

To change the stored model:

```sql
SELECT postvec.migrate('public.docs', 'body', 'baai-bge-m3');
```

Old rows convert through an embedding-space translator. New writes use the new model. The columns swap at `migration_finalize()`.

## Install

| Host | Start here |
|---|---|
| Try it | [Quick start](https://postvec.dev/docs/quickstart) (one container) or [quick start remote](https://postvec.dev/docs/quickstart-remote) |
| Existing PostgreSQL 16, 17 or 18 | [Packages](https://postvec.dev/docs/install/packages), then [`postvec setup`](https://postvec.dev/docs/install/setup) |
| RDS, Aurora, Cloud SQL, Azure, Supabase, Neon | [Managed PostgreSQL](https://postvec.dev/docs/server/managed) |

Downloads: [postvec.dev/download](https://postvec.dev/download). Docs: [postvec.dev/docs](https://postvec.dev/docs/).

## Inference

**Embedded** (`postvec.mode = 'embedded'`, default). The engine runs in the PostgreSQL process. Models live on local disk. The local image includes MiniLM (384-d).

**Remote** (`postvec.mode = 'grpc'`). PostgreSQL sends work to [`postvec-server`](postvec-server/README.md): GPU, process isolation, a fleet, or a host that cannot load a third-party `.so`. SQL is the same.

Hosted embedding APIs (OpenAI, Cohere, Bedrock, Gemini, Mistral, OpenRouter, UniVec) are optional per column. Keys live in `0600` files on the inference host. [Providers](https://postvec.dev/docs/models/providers).

## License

The extension, CLI, engine, providers and packaging use the PostgreSQL License.

`postvec-server` uses Business Source License 1.1 (source-available). Development, testing, personal production and one 30-day production evaluation per organization are free. Organizational production use needs [postvec Pro](https://univec.ai). Each version converts to the PostgreSQL License four years after release. [LICENSING.md](LICENSING.md).

## Source

```console
cargo build --workspace                 # CLI, server, engine
cd postvec && cargo pgrx test pg18      # extension tests
cd postvec && ./ci.sh                   # full local gate
```

Needs Rust, cargo-pgrx 0.18.1, a pgrx-managed PostgreSQL 18 with pgvector, and protoc. [Contributing](CONTRIBUTING.md).
