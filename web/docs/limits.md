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

## Not supported

- **RDS, Aurora** and any host that cannot set
  `shared_preload_libraries = 'postvec'`
- `TEMPORARY` tables (the worker cannot see them)
- In-SQL chat / RAG completion
- Provider API keys as GUCs
- In-database document parsing
- A new index access method

## Deliberate product refusals

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
| Extension, CLI, packages, images | **PostgreSQL License** |
| Embedded inference on the database host | The same stack, running in-process |
| `postvec-server` (remote-mode node) | To be announced |
| Converter catalogue | UniVec, under a separate license |
| Bundled MiniLM | Upstream license, shipped in the model package |

Terms for `postvec-server` are not settled yet. The omission is
deliberate rather than an oversight; it will be stated before the first
release.

A verified UniVec account sees the private catalogue superset. The
public channel is a subset and does not require a key.
