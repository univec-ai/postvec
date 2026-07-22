---
title: Overview
description: postvec requirements, install sequence and inference modes.
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

## Requirements

PostgreSQL 16, 17 or 18 on a host that can set
`shared_preload_libraries = 'postvec'`. First-time setup restarts the
cluster.

- [Quick start](/docs/quickstart) in a container
- [Packages](/docs/install/packages) on an existing PostgreSQL, then [configure](/docs/install/setup)
- [SQL functions](/docs/guides/) for enable, adopt, bridge, migrate or chunking

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

The [quick start](/docs/quickstart) is the same sequence in one container.

Functions live in the `postvec` schema (`postvec.enable(...)`).
Leave `search_path` as it is.

## Inference modes

| | Embedded (default) | Remote (`grpc`) |
|---|---|---|
| Where inference runs | Inside the PostgreSQL launcher | `postvec-server` nodes you run |
| Text leaves the DB host | No | Only to those nodes |
| Models | `postvec model ...` on this host | On each node, CLI or [dashboard](/docs/server/dashboard) |

SQL is the same in both modes. A package install plus `setup --embedded`
uses `/opt/postvec` as the engine root. Remote mode isolates inference
from the database process (CPU, GPU, crash domain).

[Embedded vs remote](/docs/concepts/modes).
[Remote inference](/docs/server/).

## Out of scope

- Provider API keys in PostgreSQL. Hosted providers are opt-in per
  column; credentials live in the inference layer.
  [External providers](/docs/models/providers).
- PDF or HTML parsing in the database. Ingest stays in the application.
- `rag()` / chat-completion SQL. Generation belongs in the application.
- A new index access method. Storage and ANN indexes are pgvector's.

## License

The extension, CLI and their packages are under the **PostgreSQL License**.
`postvec-server` is **Business Source License 1.1** (source-available;
production use by an organization needs a commercial license). Converter weights are a separate UniVec
product. The public model catalogue is a subset; a verified account sees
the private superset. The bundled MiniLM model works offline without
registry access.
