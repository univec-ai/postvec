---
title: postvec overview
description: Local embedding, hybrid search, and in-place vector migration for PostgreSQL.
---

# postvec overview

A PostgreSQL extension. Point it at a text column. It keeps a shadow
`pgvector` column in sync, does hybrid (FTS + vector) search in one
call, and either converts stored vectors to another model in place or
leaves them and [bridges the query](/docs/guides/bridge) into that
space.

Inference is in-process (embedded) or on a ninference node you run.
Not a third-party embed API. No provider keys in PostgreSQL.

The underlying dependency has names:
[vector lock-in and embedding debt](/docs/concepts/lock-in).

```sql
SELECT postvec.enable('public.docs', 'body',
                      model => 'sentence-transformers-all-minilm-l6-v2');
SELECT postvec.migrate('public.docs', 'body', new_model => 'baai-bge-m3');
```

## Who this site is for

You run PostgreSQL 16, 17, or 18 on a host you control. You want
semantic search, a way off a locked embedding model, or both — without
sending the corpus to a vendor. You do not need to know UniVec's
internal stack.

You **cannot** use postvec on RDS or Aurora: the worker requires
`shared_preload_libraries = 'postvec'`.

## Choose by starting point

| Your database today | Start with | What happens |
|---|---|---|
| Text, no vectors | [`enable()`](/docs/guides/enable) | postvec creates and maintains a shadow vector column |
| Existing vector column, same model | [`adopt()`](/docs/guides/adopt) | existing bytes stay in place; missing rows can backfill |
| Existing vectors in an old/provider-only space | [Bridge search](/docs/guides/bridge) | queries are converted into that space; corpus stays untouched |
| Existing vectors, ready for a new model | [`migrate()`](/docs/guides/migrate) | stored vectors convert in place, or re-embed by explicit strategy |
| Long source documents | [Recursive chunking](/docs/guides/chunking) | a managed 1:N destination stores passage vectors; search returns documents |

## The operating loop

1. **Install files** — Docker, apt/dnf, or source. Nothing in the cluster
   changes yet.
2. **Configure** — `postvec setup --embedded` first. Remote/ninference
   is the organisation product.
3. **Enable or adopt** a column. New vectors fill in the background;
   adopted vectors stay put.
4. **Search** — `postvec.search(...)`, optional
   [filters](/docs/guides/filters).
5. **`migrate()`** to another model, or
   [bridge](/docs/guides/bridge) and leave the column as-is.

The [quick start](/docs/quickstart) is the Docker version of that loop.

## Two inference modes

| | Embedded (start here) | Remote (`grpc`, organisations) |
|---|---|---|
| Where inference runs | Inside the PostgreSQL launcher | A ninference fleet you operate |
| Text leaves the DB host | No | To your ninference service only |
| Models | `postvec model …` on this host | Administered on that fleet |
| Catalogue | Public subset; private after `login` | Same channels, served by the fleet |
| Same SQL? | Yes | Yes |

[Embedded vs remote](/docs/concepts/modes) has the trade-offs.

## What it will not do

- Call OpenAI / Gemini / Cohere from PostgreSQL, or store their keys.
- Parse PDFs or HTML in the database.
- Provide `rag()` / chat-completion SQL. Generation belongs in the application.
- Invent a new index access method. Storage and ANN indexes are pgvector's.

## License

The extension, CLI, and their packages are under the **PostgreSQL License**.
Converter weights are a separate UniVec product. The public registry is
a subset; organisation accounts get the full catalogue. Embedding with
the bundled open-weight MiniLM model is free and offline.

## Next

- [Embedding debt and vector lock-in](/docs/concepts/lock-in)
- [Quick start](/docs/quickstart) — working search in one container
- [Install](/docs/install/) — Docker, packages, or source
- [Usage](/docs/guides/) — every SQL verb, with expected results
