---
title: Overview
description: What postvec is, what you need and the path from install to search.
---

# Overview

postvec is a PostgreSQL extension. You mark a text column as semantic. It
adds a shadow `pgvector` column, keeps that column in sync as rows change
and searches with full-text and vectors in one call.

If the embedding model later has to change, stored vectors can be
converted in place. You can also leave the column as-is and convert only
the query.

Inference runs on the database host by default. Models live on disk. The
bundled MiniLM model needs no API key.

A column can also be bound to a hosted embedding API instead. The key stays
in the inference layer, never in the database.
[External providers](/docs/models/providers).

## What you need

PostgreSQL 16, 17 or 18 on a host that can set
`shared_preload_libraries = 'postvec'`. A restart is part of first-time
setup.

RDS and Aurora cannot load the worker.

## The path

1. [Install the files](/docs/install/) - Docker, packages or a source build.
2. [Configure the cluster](/docs/install/setup) with `postvec setup`.
3. [Enable](/docs/guides/enable) a text column, or [adopt](/docs/guides/adopt)
   one that already has vectors.
4. Wait until `pending_jobs = 0`. Vectors fill after commit, not inside
   the inserting transaction.
5. [Build an ANN index](/docs/guides/indexes). Nothing builds one unless
   you ask.
6. [Search](/docs/guides/search).

The [quick start](/docs/quickstart) is the same path in one disposable
container. It does not touch the host cluster.

## Which SQL call

| You have | Call | What happens |
|---|---|---|
| Text, no vectors | [`enable()`](/docs/guides/enable) | Creates and fills a shadow `vector(N)` column |
| A populated `vector(N)` column | [`adopt()`](/docs/guides/adopt) | Registers it. Stored bytes stay. |
| Vectors in a retired or provider-only space | [Bridge search](/docs/guides/bridge) | Queries convert into that space. The corpus stays. |
| Ready to change the stored model | [`migrate()`](/docs/guides/migrate) | Stored vectors convert, or re-embed if you choose that. |
| Long source documents | [Chunking](/docs/guides/chunking) | A managed 1:N table holds passage vectors. Search still returns documents. |

[Which SQL call](/docs/guides/starting) walks each row with the SQL and
the expected result.

Every function lives in the `postvec` schema. Qualify the call:
`postvec.enable(...)`. The extension does not change `search_path`.

## Two inference modes

| | Embedded (default) | Remote (`grpc`) |
|---|---|---|
| Where inference runs | Inside the PostgreSQL launcher | `postvec-server` nodes you run |
| Text leaves the DB host | No | Only to those nodes |
| Models | `postvec model ...` on this host | On each node |

SQL is the same in both modes. Embedded is what a package install does
with `setup --embedded`. Use remote when inference should not share a
crash domain, a CPU budget or a GPU with PostgreSQL.

[Embedded vs remote](/docs/concepts/modes) compares them.
[Remote inference](/docs/server/) covers running the nodes.

## Out of scope

- Store provider API keys in PostgreSQL. Hosted providers are opt-in per
  column and their credentials live only in the inference layer.
  [External providers](/docs/models/providers).
- Parse PDFs or HTML in the database.
- Provide `rag()` or chat-completion SQL. Generation belongs in the application.
- Invent a new index access method. Storage and ANN indexes are pgvector's.

## License

The extension, CLI and their packages are under the **PostgreSQL License**.
Converter weights are a separate UniVec product. The public model
catalogue is a subset; a verified account sees the private superset. The
bundled MiniLM model works offline and does not need registry access.

Terms for `postvec-server`, the remote-mode inference node, are not
settled yet and are not stated here.
