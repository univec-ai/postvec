---
title: Overview
description: What postvec is, where to start, and the two inference hosts.
---

# Overview

postvec is a PostgreSQL extension for semantic search. You mark a text column as
semantic and postvec adds a shadow `pgvector` column, keeps that column in sync as
rows change and answers hybrid (full-text and vector) queries in one call.

Three common starting points:

- **A text column with no vectors.** [`enable()`](/docs/guides/enable) creates the
  shadow column and queues the existing rows for embedding.
- **A column that already holds vectors.** [`adopt()`](/docs/guides/adopt) registers
  it and keeps the stored bytes. If those vectors came from a model you no longer
  send requests to, [search that space](/docs/guides/bridge) first: each query is
  converted into the stored space.
- **A stored model you want to replace.** [`migrate()`](/docs/guides/migrate)
  converts the stored vectors in place, or re-embeds them if you choose that.

Inference runs on the database host by default, from model files on disk. The
bundled MiniLM model needs no API key. A column can also use a hosted embedding
API: OpenAI, Cohere, Amazon Bedrock, Gemini, Mistral, OpenRouter or UniVec (embed
and convert). The key stays in the inference layer.
[External providers](/docs/models/providers).

Qualify every call: `postvec.enable(...)`, `postvec.search(...)`.

## Requirements

PostgreSQL 16, 17 or 18. pgvector 0.8 or newer, on the same major.

## Install

| Host | Path |
|---|---|
| Container, no configuration | [Quick start local](/docs/quickstart), or [both containers](/docs/quickstart-remote) |
| Self-hosted cluster that can load a native extension | [Packages](/docs/install/packages) or [Docker](/docs/install/docker), then [postvec setup](/docs/install/setup) |
| RDS, Aurora, Cloud SQL, Azure Flexible Server, Supabase and Neon | [Managed PostgreSQL](/docs/server/managed): a plain SQL schema and a worker in `postvec-server` |

First install on a self-hosted cluster:

1. Install [packages](/docs/install/packages) or start the
   [Docker image](/docs/install/docker).
2. Run [`postvec setup`](/docs/install/setup). Expected: `postvec.models` lists
   MiniLM, plus whatever a remote postvec-server advertises.
3. [`enable()`](/docs/guides/enable) a text column, or
   [`adopt()`](/docs/guides/adopt) a populated one. Expected: a registry id. The
   shadow column stays NULL until the worker writes it.
4. Wait until `pending_jobs = 0` in [`status()`](/docs/guides/status).
5. [Build an ANN index](/docs/guides/indexes). `index_mode` defaults to `manual`.
6. [Search](/docs/guides/search). Hybrid by default; see [BM25](/docs/guides/bm25)
   for the keyword settings.

## Inference hosts

| | Embedded (default) | [postvec-server](/docs/server/) |
|---|---|---|
| Runs in | The PostgreSQL launcher, one thread | A separate process, multi-threaded |
| Source text | Stays on the database host, except columns bound to an [external provider](/docs/models/providers) | Reaches the server that holds the model |
| Scale | One host | A CPU or GPU fleet on the network |
| Models | `postvec model ...` on the database host | [Dashboard](/docs/server/dashboard) or HTTP API |
| Managed PostgreSQL | - | [Managed PostgreSQL](/docs/server/managed) |

SQL, the job queue and the sync behaviour are identical in both modes. Use
postvec-server for process isolation from PostgreSQL, more threads on the same VM,
a fleet, or a database that cannot load the extension.
[When to use postvec-server](/docs/server/usage) and
[embedded vs remote](/docs/concepts/modes).

## Other features

| Feature | Documented in |
|---|---|
| Recursive chunking for long documents | [Chunking](/docs/guides/chunking) |
| Typed metadata filters inside the search | [Filters](/docs/guides/filters) |
| Embedding templates over several columns | [Templates](/docs/guides/templates) |
| Queue status, stats and dead-letter re-drive | [Status](/docs/guides/status), [`retry_dead()`](/docs/guides/retry) |
| Model registry and air-gapped installs | [Models](/docs/models/) |
| An existing pgai or pg_vectorize pipeline | [Coming from pgai](/docs/from-pgai) |

## Scope

Storage and ANN indexes are pgvector's. Ingest (PDF, HTML, Office) and generation
(RAG, chat completion) belong to the application. Provider keys live in the
inference layer.

## License

The extension, the CLI and their packages use the PostgreSQL License, in either
inference mode. `postvec-server` is source-available under the Business Source
License 1.1: development, testing, personal noncommercial production and one
30-day production evaluation per organization and its affiliates under common
control are free; production use by an organization needs
[postvec Pro](https://univec.ai) at €30/month, and hosting or embedding the
server for third parties needs a platform/OEM agreement. Full table:
[License](/docs/license).
