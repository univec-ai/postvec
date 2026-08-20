---
title: Overview
description: postvec requirements, install paths and inference modes.
---

# Overview

postvec is a PostgreSQL extension. You mark a text column as semantic. It
adds a shadow `pgvector` column, keeps that column in sync as rows change
and searches with full-text and vectors in one call.

A column that already holds vectors can keep them. `search()` embeds each
query into that stored space (embed-bridge). If you later want a different
model, stored vectors convert in place.

Inference runs on the database host by default. Models live on disk. The
bundled MiniLM model needs no API key.

A column can also use a hosted embedding API. The key stays in the
inference layer. [External providers](/docs/models/providers).

## Starting points

| Situation | Path |
|---|---|
| Text column, no vectors | [`enable()`](/docs/guides/enable) creates and maintains a shadow `vector(N)` column |
| Existing vectors, keep that model | [`adopt()`](/docs/guides/adopt) then [`search()`](/docs/guides/search). For a retired or provider-only space such as ada-002, [search the existing space](/docs/guides/bridge): each query is converted into the stored space. Rows stay as they are. |
| Existing vectors, change the model | After search on the current space is working, [`migrate()`](/docs/guides/migrate) converts stored vectors in place |

Keep-then-search is the usual first step for a populated column. Migration
is optional and comes after that path is working.

## Install

PostgreSQL 16, 17 or 18. pgvector >= 0.8.

| Host | Path |
|---|---|
| Self-hosted cluster that can load a native extension | [Packages](/docs/install/packages) or [Docker](/docs/install/docker), then [configure](/docs/install/setup) |
| RDS, Aurora, Cloud SQL, Azure Flexible Server, Supabase and Neon | [Managed PostgreSQL](/docs/server/managed): a plain SQL schema and a worker in `postvec-server` |

[Quick start](/docs/quickstart) is the same sequence in one container.

Self-hosted first-time setup:

1. [Packages](/docs/install/packages) or [Docker](/docs/install/docker).
2. [Configure the cluster](/docs/install/setup) with `postvec setup`.
   Expected: `postvec.models` lists MiniLM (or whatever the node
   advertises in remote mode).
3. [Enable](/docs/guides/enable) a text column, or [adopt](/docs/guides/adopt)
   one that already has vectors.
   Expected: a registry id. Vectors stay NULL until the worker writes them.
4. Wait until `pending_jobs = 0`. Vectors fill after commit.
5. [Build an ANN index](/docs/guides/indexes). Default `index_mode` is
   `manual`.
6. [Search](/docs/guides/search).

Functions live in the `postvec` schema (`postvec.enable(...)`).
Leave `search_path` as it is.

## Inference modes

| | Embedded (default) | Remote (`grpc`) |
|---|---|---|
| Where inference runs | Inside the PostgreSQL launcher | `postvec-server` nodes you run |
| Text leaves the DB host | Stays on the host, except columns bound to an [external provider](/docs/models/providers) | Goes to those nodes |
| Models | `postvec model ...` on this host | On each node, CLI or [dashboard](/docs/server/dashboard) |

SQL is the same in both modes. A package install plus `setup --embedded`
uses `/opt/postvec` as the engine root. Remote mode isolates inference
from the database process (CPU, GPU, crash domain).

[Embedded vs remote](/docs/concepts/modes).
[Remote inference](/docs/server/).
[Managed PostgreSQL](/docs/server/managed) when the database cannot load
`postvec.so`.

## Related work

Ingest (PDF, HTML, Office) and generation (`rag()`, chat completion) stay
in the application. Storage and ANN indexes are pgvector's. Provider API
keys live in the inference layer.

## License

The extension, CLI and their packages are under the **PostgreSQL License**.
`postvec-server` is **Business Source License 1.1** (source-available).
Personal production use, all non-production environments and one 30-day
production evaluation per organization are free. Production use by an
organization needs a [postvec Pro](https://univec.ai) subscription. Each
version converts to the PostgreSQL License four years after release.

The public model catalogue is free. Commercial use of the private
catalogue (converters) needs postvec Pro. Hosted embed and convert APIs
are billed separately. The bundled MiniLM model works offline.
