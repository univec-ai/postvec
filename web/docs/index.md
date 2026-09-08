---
title: Overview
description: postvec requirements, install paths and inference modes.
---

# Overview

postvec is a PostgreSQL extension. You mark a text column as semantic.
It adds a shadow `pgvector` column, keeps that column in sync as rows
change and searches with full-text and vectors in one call.

A column that already holds vectors can keep them. `search()` embeds
each query into that stored space (embed-bridge). If later you want a
different stored model, vectors convert in place.

Inference runs on the database host by default. Models live on disk.
The bundled MiniLM model needs no API key.

A column can also use a hosted embedding API: OpenAI, Cohere, Amazon
Bedrock, Gemini, Mistral, OpenRouter or UniVec (embed and convert).
The key stays in the inference layer.
[External providers](/docs/models/providers).

Qualify every call as `postvec.function(...)`.

## Starting points

| Situation | Path |
|---|---|
| Text column, no vectors | [`enable()`](/docs/guides/enable) creates and maintains a shadow `vector(N)` column |
| Existing vectors, keep that model | [`adopt()`](/docs/guides/adopt) then [`search()`](/docs/guides/search). For a retired or provider-only space such as ada-002, [search the existing space](/docs/guides/bridge): each query is converted into the stored space. |
| Existing vectors, change the model | After search on the current space is working, [`migrate()`](/docs/guides/migrate) converts stored vectors in place |
| pgai or pg_vectorize pipeline | [Coming from pgai](/docs/from-pgai): map the vectorizer to `enable()` / `adopt()`, then `search()` |

Keep-then-search is the usual first step for a populated column.
Migration is optional and comes after that path is working.

## Install

PostgreSQL 16, 17 or 18. pgvector >= 0.8.

| Host | Path |
|---|---|
| Try it in a container | [Quick start local](/docs/quickstart) or [quick start remote](/docs/quickstart-remote) |
| Self-hosted cluster that can load a native extension | [Packages](/docs/install/packages) or [Docker](/docs/install/docker), then [configure](/docs/install/setup) |
| RDS, Aurora, Cloud SQL, Azure Flexible Server, Supabase and Neon | [Managed PostgreSQL](/docs/server/managed): a plain SQL schema and a worker in `postvec-server` |

Self-hosted first-time setup:

1. [Packages](/docs/install/packages) or [Docker](/docs/install/docker).
2. [Configure the cluster](/docs/install/setup) with `postvec setup`.
   Expected: `postvec.models` lists MiniLM (or whatever postvec-server
   advertises in remote mode).
3. [Enable](/docs/guides/enable) a text column, or [adopt](/docs/guides/adopt)
   one that already has vectors.
   Expected: a registry id. Vectors stay NULL until the worker writes them.
4. Wait until `pending_jobs = 0`. Vectors fill after commit.
5. [Build an ANN index](/docs/guides/indexes). Default `index_mode` is
   `manual`.
6. [Search](/docs/guides/search). Hybrid by default; [BM25](/docs/guides/bm25)
   for the keyword mix, GIN and corpus stats.

## Inference modes

| | Embedded (default) | Remote (`grpc`) with [postvec-server](/docs/server/) |
|---|---|---|
| Where inference runs | Inside the PostgreSQL launcher (one thread) | `postvec-server`, multi-threaded, CPU or GPU |
| Text leaves the DB host | Stays on the host, except columns bound to an [external provider](/docs/models/providers) | Goes to those servers |
| Models | `postvec model ...` on this host | On each server, CLI or [dashboard](/docs/server/dashboard) |

SQL is the same in both modes. A package install plus `setup --embedded`
uses `/opt/postvec` as the engine root.

[postvec-server](/docs/server/) is the companion inference process.
Use it for a separate process from PostgreSQL, multi-threaded inference
on the same VM, a CPU or GPU fleet and model management from the
dashboard or HTTP API. Managed cloud databases use it too.

[When to use postvec-server](/docs/server/usage).
[Embedded vs remote](/docs/concepts/modes).
[Install postvec-server](/docs/server/node)
([Docker](/docs/server/docker), [packages](/docs/server/packages),
[from source](/docs/server/source)).
[Managed PostgreSQL](/docs/server/managed) when the database cannot load
`postvec.so`.

## Related work

Ingest (PDF, HTML, Office) and generation (`rag()`, chat completion)
stay in the application. Storage and ANN indexes are pgvector's.
Provider API keys live in the inference layer.

## License

The extension, CLI and their packages are under the **PostgreSQL License**.
`postvec-server` is **Business Source License 1.1** (source-available).
Personal production use, all non-production environments and one 30-day
production evaluation per organization are free. Production use by an
organization needs a [postvec Pro](https://univec.ai) subscription.

Full table: [License](/docs/license).
