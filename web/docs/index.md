---
title: Overview
description: What postvec is, what it is not, and where to start.
---

# Overview

postvec keeps a shadow `pgvector` column synchronized with a source text
column, provides hybrid full-text and vector search, and either converts
stored vectors to another model in place or
[bridges each query](/docs/guides/bridge) into the existing space.

Inference runs in-process ([embedded](/docs/concepts/modes)) or on separately
operated ninference nodes. No third-party embedding API or provider key in
PostgreSQL is required.

[Vector lock-in](/docs/concepts/lock-in) is the name for the dependency
between stored vectors and their embedding model.
[Embedding debt](/docs/concepts/lock-in) is the accumulated cost of changing
that dependency. postvec is the operator for both.

```sql
SELECT postvec.enable('public.docs', 'body',
                      model => 'sentence-transformers-all-minilm-l6-v2');
SELECT postvec.migrate('public.docs', 'body', new_model => 'baai-bge-m3');
```

## Who this is for

A third-party operator with a working PostgreSQL 16, 17, or 18 install, on a
host that permits `shared_preload_libraries`. Knowledge of the internal UniVec
stack is not required.

RDS and Aurora are unsupported: the worker requires
`shared_preload_libraries = 'postvec'`.

## Starting points

| Initial state | Operation | Result |
|---|---|---|
| Text, no vectors | [`enable()`](/docs/guides/enable) | postvec creates and maintains a shadow vector column |
| Existing vector column, same model | [`adopt()`](/docs/guides/adopt) | existing vectors remain unchanged; missing rows can backfill |
| Existing vectors in an old or provider-only space | [Bridge search](/docs/guides/bridge) | queries convert into that space; the corpus stays put |
| Existing vectors, ready for a new model | [`migrate()`](/docs/guides/migrate) | stored vectors convert in place, or re-embed by explicit strategy |
| Long source documents | [Recursive chunking](/docs/guides/chunking) | a managed 1:N destination stores passage vectors; search returns documents |

## Basic workflow

1. **Install files** — [Docker](/docs/install/docker),
   [packages](/docs/install/packages), or [source](/docs/install/source).
   File installation does not modify cluster state.
2. **Configure** — `postvec setup --embedded` for local inference, or
   configure remote ninference endpoints. See
   [cluster configuration](/docs/install/setup).
3. **Enable or adopt** a column. New vectors fill in the background;
   adopted vectors remain unchanged.
4. **Search** — `postvec.search(...)`, optional
   [filters](/docs/guides/filters).
5. **`migrate()`** to another model, or
   [bridge](/docs/guides/bridge) while leaving the column unchanged.

The [quick start](/docs/quickstart) is the same workflow in one disposable
container.

## Two inference modes

| | Embedded | Remote (`grpc`) |
|---|---|---|
| Where inference runs | Inside the PostgreSQL launcher | Separately operated ninference nodes |
| Text leaves the DB host | No | Only to the configured ninference service |
| Models | `postvec model …` on this host | Administered on that fleet |
| Catalogue | Public subset; private after `login` | Same channels, served by the fleet |
| Same SQL? | Yes | Yes |

Embedded is the default story on this site. Remote is the shape for
distributed or GPU inference and for organisation ninference deployments.
[Embedded vs remote](/docs/concepts/modes) has the trade-offs.

## Out of scope

- Call OpenAI / Gemini / Cohere from PostgreSQL, or store their keys.
- Parse PDFs or HTML in the database.
- Provide `rag()` / chat-completion SQL. Generation belongs in the application.
- Invent a new index access method. Storage and ANN indexes are pgvector's.

## License

The extension, CLI, and their packages are under the **PostgreSQL License**.
Converter weights are a separate UniVec product. The public registry is a
subset; a verified account sees the private superset. The bundled open-weight
MiniLM model operates offline and does not require registry access.
