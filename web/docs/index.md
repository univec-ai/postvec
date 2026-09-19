---
title: Overview
description: What postvec is, where to start, and the two inference hosts.
---

# Overview

Postvec is an extension for fused hybrid search: it combines BM25 and semantic search in one SQL call and keeps vectors in sync with your text automatically. You can also use it to migrate vectors without re-embedding of text with direct conversion between embedding formats.

Inference runs locally inside PostgreSQL or remote on [postvec-server](/docs/server/) or through hosted embedding APIs.

Qualify every call: `postvec.enable(...)`, `postvec.search(...)`.

| Starting point | First call |
|---|---|
| A text column with no vectors | [`enable()`](/docs/guides/enable) creates the shadow column and queues existing rows |
| A column that already holds vectors | [`adopt()`](/docs/guides/adopt) registers it and keeps the stored bytes |
| A stored model to replace | [`migrate()`](/docs/guides/migrate) converts vectors in place, or re-embeds them |

The bundled MiniLM model runs locally. A column can also use a hosted embedding API (OpenAI, Cohere, Amazon Bedrock, Gemini, Mistral, OpenRouter or UniVec). The key stays in the inference layer. [External providers](/docs/models/providers).

## Requirements

PostgreSQL 16, 17 or 18. pgvector 0.8 or newer, on the same major.

## Install

| Host | Path |
|---|---|
| Container | [Quick start local](/docs/quickstart) or [both containers](/docs/quickstart-remote) |
| Self-hosted cluster | [Packages](/docs/install/packages) or [Docker](/docs/install/docker), then [postvec setup](/docs/install/setup) |
| RDS, Aurora, Cloud SQL, Azure Flexible Server, Supabase, Neon | [Managed PostgreSQL](/docs/server/managed) |

On a self-hosted cluster:

1. Install [packages](/docs/install/packages) or start the [Docker image](/docs/install/docker).
2. Run [`postvec setup`](/docs/install/setup). Expected: `postvec.models` lists MiniLM.
3. [`enable()`](/docs/guides/enable) a text column, or [`adopt()`](/docs/guides/adopt) a populated one. Expected: a registry id. The shadow column stays NULL until the worker writes it.
4. Wait until `pending_jobs = 0` in [`status()`](/docs/guides/status).
5. [Build an ANN index](/docs/guides/indexes). `index_mode` defaults to `manual`.
6. [Search](/docs/guides/search). Hybrid by default.

## Inference hosts

| | Embedded (default) | [postvec-server](/docs/server/) |
|---|---|---|
| Runs in | The PostgreSQL launcher, one thread | A separate process, multi-threaded |
| Source text | Stays on the database host, except columns bound to an [external provider](/docs/models/providers) | Reaches the server that holds the model |
| Scale | One host | A CPU or GPU fleet |
| Models | `postvec model ...` on the database host | [Dashboard](/docs/server/dashboard) or HTTP API |
| Managed PostgreSQL | Use postvec-server | [Managed PostgreSQL](/docs/server/managed) |

SQL, the job queue and the sync behaviour are identical in both modes. Use postvec-server for process isolation, more threads on the same VM, a fleet, or a database that cannot load the extension. [When to use postvec-server](/docs/server/usage).

## Other features

| Feature | Guide |
|---|---|
| Recursive chunking for long documents | [Chunking](/docs/guides/chunking) |
| Typed metadata filters | [Filters](/docs/guides/filters) |
| Embedding templates over several columns | [Templates](/docs/guides/templates) |
| Queue status and dead-letter re-drive | [Status](/docs/guides/status), [`retry_dead()`](/docs/guides/retry) |
| Model registry and air-gapped installs | [Models](/docs/models/) |
| An existing pgai or pg_vectorize pipeline | [Coming from pgai](/docs/from-pgai) |

Storage and ANN indexes are pgvector's.

## License

The extension, the CLI and their packages use the PostgreSQL License. `postvec-server` is source-available under the Business Source License 1.1: development, testing, personal production and one 30-day production evaluation per organization are free; organizational production use needs [postvec pro](/server#plans) at €30/month. [License](/docs/license).
