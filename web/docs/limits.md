---
title: Limits and compatibility
description: Supported environments and explicit limitations.
---

# Limits and compatibility

## Supported

- PostgreSQL **16, 17, 18**
- pgvector **>= 0.8**
- Debian 12, Ubuntu 22.04 / 24.04, EL9 (Alma, Rocky; RHEL/CentOS Stream
  with `--force-untested` on the bootstrap)
- `amd64` and `arm64`
- Ordinary and partitioned tables with a primary key

## Environment

On a self-hosted cluster, postvec loads as
`shared_preload_libraries = 'postvec'` and the host restarts PostgreSQL.
Tables must be ordinary or partitioned and have a primary key. The
worker runs in its own session, so `TEMPORARY` tables are invisible to
it.

On RDS, Aurora, Cloud SQL, Azure Flexible Server, Supabase and Neon,
[managed PostgreSQL](/docs/server/managed) installs a plain SQL schema
and runs the worker in [postvec-server](/docs/server/). The same binary
is the remote inference process for self-hosted clusters
([install](/docs/server/node)).

## Scope

postvec covers in-database embedding, hybrid search, chunking, adopt
and in-place migration. Generation (`rag()`, chat completion) and
document parsing (PDF, HTML, Office) stay in the application.
Storage and ANN indexes are pgvector's. Provider API keys live in the
inference layer; see [external providers](/docs/models/providers)
([OpenAI](/docs/models/openai), [Cohere](/docs/models/cohere),
[Amazon Bedrock](/docs/models/aws), [Gemini](/docs/models/gemini),
[Mistral](/docs/models/mistral), [OpenRouter](/docs/models/openrouter),
[UniVec](/docs/models/univec)).

The Bedrock signer takes static credentials. `max_concurrent` bounds
provider calls in flight. Connectors use rustls with the bundled
Mozilla root set. Hosted converters serve `migrate()` and `convert()`.
Embed-bridge uses local models.

## SQL constraints

| Situation | What happens |
|---|---|
| Chunked `adopt()` | Use `enable(..., chunking => 'recursive')` for new chunked entries |
| Chunked `index_mode => 'immediate'` | Index the destination after backfill |
| Composite PK + chunking | Column mode accepts composite PKs; chunking needs a single-column PK |
| Live splitter reconfiguration | `disable`, drop, `enable` again |
| `halfvec` / undimensioned `vector` on adopt | Rewrite to `vector(N)` first; the error includes the `ALTER TABLE` |
| `NOT NULL` vector the worker would write | Observed adopt (`sync => false`, `backfill => 'none'`) or drop the constraint |
| `DROP EXTENSION ... CASCADE` | Use [`uninstall`](/docs/install/uninstall) for bounded teardown |

## Resource notes

- One worker per configured database. An `auto` index build occupies that worker.
- Chunked writers pay invalidation inside their own transaction.
- `set_format()` blocks writers while it enqueues a full refresh.
- `query_timeout_ms` (default 2 s) bounds the synchronous query embed.
- Chunk splitter: at most 10,000 non-blank chunks, 32 MiB UTF-8 and 4x
  amplification per document.

## License split

| Layer | License |
|---|---|
| Extension, CLI, packages, PostgreSQL images | **PostgreSQL License** |
| Embedded inference on the database host | The same stack, running in-process |
| `postvec-server` (remote-mode node and managed PostgreSQL) | **Business Source License 1.1** |
| Converter catalogue | UniVec, under a separate license |
| Bundled MiniLM | Upstream license, shipped in the model package |

A verified UniVec account sees the private catalogue superset. The
public channel is a subset and needs no key. Terms and Pro:
[License](/docs/license).
