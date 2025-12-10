---
title: Limits and compatibility
description: Supported environments and explicit limitations.
---

# Limits and compatibility

## Supported

- PostgreSQL **16, 17, 18**
- pgvector **≥ 0.8**
- Debian 12, Ubuntu 22.04 / 24.04, EL9 (Alma, Rocky; RHEL/CentOS Stream
  with `--force-untested` on the bootstrap)
- `amd64` and `arm64`
- Ordinary and partitioned tables with a primary key

## Not supported

- **RDS, Aurora**, and any host that cannot set
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
| `DROP EXTENSION … CASCADE` | Unsupported; use `uninstall` |
| Signed apt/yum repository | Not shipped yet |

## Resource notes

- One worker per configured database. `auto` index builds occupy it.
- Chunked writers pay invalidation inside their own transaction.
- `set_format()` blocks writers while it enqueues a full refresh.
- `query_timeout_ms` (default 2 s) bounds the synchronous query embed.
- Chunk splitter: ≤ 10,000 non-blank chunks, ≤ 32 MiB UTF-8, ≤ 4×
  amplification per document.

## License split

Extension + CLI + packaging: **PostgreSQL License**.
Converter catalogue: UniVec, not this license. The bundled MiniLM model
keeps its own upstream license, shipped in the package.
