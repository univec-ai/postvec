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

A column can also use a hosted embedding API. The key stays in the
inference layer. [External providers](/docs/models/providers).

## What you need

PostgreSQL 16, 17 or 18 on a host that can set
`shared_preload_libraries = 'postvec'`. First-time setup restarts the
cluster.

## Start here

| Situation | Page |
|---|---|
| Try it in a container | [Quick start](/docs/quickstart) |
| Install on an existing PostgreSQL | [Install](/docs/install/), then [configure](/docs/install/setup) |
| Pick enable, adopt or migrate | [SQL functions](/docs/guides/starting) |

## The path

1. [Install](/docs/install/) - Docker, packages or a source build.
2. [Configure the cluster](/docs/install/setup) with `postvec setup`.
   Expected: `postvec doctor --deep` exits 0 and `postvec.models` lists
   MiniLM (or whatever the node advertises in remote mode).
3. [Enable](/docs/guides/enable) a text column, or [adopt](/docs/guides/adopt)
   one that already has vectors.
   Expected: a registry id. Vectors stay NULL until the worker writes them.
4. Wait until `pending_jobs = 0`. Vectors fill after commit.
5. [Build an ANN index](/docs/guides/indexes). Default `index_mode` is
   `manual`.
6. [Search](/docs/guides/search).

The [quick start](/docs/quickstart) is the same path in one container.

## SQL functions

| You have | Call | What happens |
|---|---|---|
| Text, no vectors | [`enable()`](/docs/guides/enable) | Creates and fills a shadow `vector(N)` column |
| A populated `vector(N)` column | [`adopt()`](/docs/guides/adopt) | Registers it. Stored bytes stay. |
| Vectors in a retired or provider-only space | [Bridge search](/docs/guides/bridge) | Queries convert into that space. The corpus stays. |
| Ready to change the stored model | [`migrate()`](/docs/guides/migrate) | Stored vectors convert, or re-embed if you choose that. |
| Long source documents | [Chunking](/docs/guides/chunking) | A managed 1:N table holds passage vectors. Search still returns documents. |

[SQL functions](/docs/guides/starting) has the SQL and expected result
for each case.

Functions live in the `postvec` schema (`postvec.enable(...)`).
Leave `search_path` as it is.

## Two inference modes

| | Embedded (default) | Remote (`grpc`) |
|---|---|---|
| Where inference runs | Inside the PostgreSQL launcher | `postvec-server` nodes you run |
| Text leaves the DB host | No | Only to those nodes |
| Models | `postvec model ...` on this host | On each node |

SQL is the same in both modes. A package install plus `setup --embedded`
uses `/opt/postvec` as the engine root. Remote mode isolates inference
from the database process (CPU, GPU, crash domain).

[Embedded vs remote](/docs/concepts/modes) compares them.
[Remote inference](/docs/server/) covers running the nodes.

## Out of scope

- Provider API keys in PostgreSQL. Hosted providers are opt-in per
  column; credentials live in the inference layer.
  [External providers](/docs/models/providers).
- PDF or HTML parsing in the database
- `rag()` / chat-completion SQL. Generation belongs in the application.
- A new index access method. Storage and ANN indexes are pgvector's.

## License

The extension, CLI and their packages are under the **PostgreSQL License**.
Converter weights are a separate UniVec product. The public model
catalogue is a subset; a verified account sees the private superset. The
bundled MiniLM model works offline without registry access.

Terms for `postvec-server` will be stated before the first release.
