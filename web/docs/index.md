---
title: Overview
description: What postvec is and where to start.
---

# Overview

postvec keeps a shadow `pgvector` column synchronized with a source text
column. It also provides hybrid full-text and vector search. Stored vectors
can be converted to another model in place, or each query can be
[bridged](/docs/guides/bridge) into the space that already exists.

Inference runs in-process by default ([embedded](/docs/concepts/modes)), or on
`postvec-server` nodes you operate. Models are local or on-prem.

[Vector lock-in](/docs/concepts/lock-in) is the dependency between stored
vectors and the model that produced them. [Embedding debt](/docs/concepts/lock-in)
is the cost of changing that dependency later. postvec operates on both.

Use search in the header to jump to a function, GUC or topic.

```sql
SELECT postvec.enable('public.docs', 'body',
                      model => 'sentence-transformers-all-minilm-l6-v2');
SELECT postvec.migrate('public.docs', 'body', new_model => 'baai-bge-m3');
```

## Who this is for

An operator with a working PostgreSQL 16, 17 or 18 install on a host
that permits `shared_preload_libraries`.

RDS and Aurora are unsupported. The worker requires
`shared_preload_libraries = 'postvec'`.

## Starting points

The same table, with a short walkthrough for each row, lives on
[Choose the SQL call](/docs/guides/starting).

| Initial state | Operation | Result |
|---|---|---|
| Text, no vectors | [`enable()`](/docs/guides/enable) | postvec creates and maintains a shadow vector column |
| Existing vector column, same model | [`adopt()`](/docs/guides/adopt) | existing vectors stay as they are; missing rows can backfill |
| Existing vectors in an old or provider-only space | [Bridge search](/docs/guides/bridge) | queries convert into that space; the corpus stays put |
| Existing vectors, ready for a new model | [`migrate()`](/docs/guides/migrate) | stored vectors convert in place, or re-embed if that strategy is chosen |
| Long source documents | [Recursive chunking](/docs/guides/chunking) | a managed 1:N destination stores passage vectors; search returns documents |

## Basic workflow

1. **Install files** - [Docker](/docs/install/docker),
   [packages](/docs/install/packages) or [source](/docs/install/source).
   Then configure the cluster.
2. **Configure** - `postvec setup --embedded` for local inference, or
   `--grpc` / `--http` for remote nodes. See
   [cluster configuration](/docs/install/setup).
3. **Enable or adopt** a column. New vectors fill in the background.
   Adopted vectors stay as they are.
4. **Search** - `postvec.search(...)`, with optional
   [filters](/docs/guides/filters).
5. **`migrate()`** to another model, or
   [bridge](/docs/guides/bridge) and leave the column unchanged.

The [quick start](/docs/quickstart) is the same path in one disposable
container.

## Two inference modes

| | Embedded | Remote (`grpc`) |
|---|---|---|
| Where inference runs | Inside the PostgreSQL launcher | `postvec-server` nodes you operate |
| Text leaves the DB host | No | Only to the configured nodes |
| Models | `postvec model ...` on this host | Administered on each node |
| Catalogue | Public subset; private after `login` | Same channels, pulled onto each node |
| Same SQL? | Yes | Yes |

Embedded is the default and this site leads with it. Remote mode is the
shape for keeping engine faults off the database host, for GPU inference,
and for one engine serving several databases; the nodes are
`postvec-server`, shipped in the postvec repository.
[Embedded vs remote](/docs/concepts/modes) has the differences.

## Out of scope

- Call OpenAI / Gemini / Cohere from PostgreSQL, or store their keys.
- Parse PDFs or HTML in the database.
- Provide `rag()` / chat-completion SQL. Generation belongs in the application.
- Invent a new index access method. Storage and ANN indexes are pgvector's.

## License

The extension, CLI and their packages are under the **PostgreSQL License**.
Converter weights are a separate UniVec product. The public registry is a
subset; a verified account sees the private superset. The bundled open-weight
MiniLM model works offline and does not need registry access.

Terms for `postvec-server`, the remote-mode inference node, are not settled
yet and are deliberately not stated here.
