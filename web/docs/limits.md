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

postvec needs `shared_preload_libraries = 'postvec'` and a host that can
restart PostgreSQL. Tables must be ordinary or partitioned and have a
primary key. The worker runs in its own session, so `TEMPORARY` tables
are invisible to it.

## Out of scope

- Chat / RAG completion as SQL. Generation stays in the application.
- Provider API keys in PostgreSQL. Credentials live in the inference
  layer; see [external providers](/docs/models/providers).
- Document parsing (PDF, HTML, Office) inside the database.
- A new index access method. Storage and ANN indexes are pgvector's.
- AWS session tokens, instance profiles and the default credential
  chain. The Bedrock signer takes static credentials.
- A spend or token budget. `max_concurrent` bounds calls in flight.
- A private or corporate CA for provider TLS. Connectors use rustls
  with the bundled Mozilla root set.
- Reranking providers and `Retry-After`-aware provider backoff.
- Provider-backed converters in `embed-bridge` routes. Hosted converters
  are direct `migrate()` / `convert()` routes.

## SQL refusals

| Situation | What happens |
|---|---|
| Chunked `adopt()` | Refused |
| Chunked `index_mode => 'immediate'` | Refused |
| Composite PK + chunking | Refused |
| Live splitter reconfiguration | Disable / drop / re-enable |
| `halfvec` / undimensioned `vector` on adopt | Refused, with a rewrite recipe |
| `NOT NULL` vector the worker would write | Refused |
| `DROP EXTENSION ... CASCADE` | Unsupported. `uninstall` is the supported path. |
| Signed apt/yum repository | Not shipped yet |

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
| `postvec-server` (remote-mode node) | **Business Source License 1.1** |
| Converter catalogue | UniVec, under a separate license |
| Bundled MiniLM | Upstream license, shipped in the model package |

A verified UniVec account sees the private catalogue superset. The
public channel is a subset and needs no key.
