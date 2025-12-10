---
title: postvec overview
description: Local embedding, hybrid search, and in-place vector migration for PostgreSQL.
---

# postvec overview

postvec keeps a shadow `pgvector` column synchronized with a source text
column, provides hybrid full-text and vector search, and either converts stored
vectors to another model in place or [bridges each query](/docs/guides/bridge)
into the existing space.

Inference runs in-process (embedded) or on separately operated ninference
nodes. No third-party embedding API or provider key in PostgreSQL is required.

[Vector lock-in and embedding debt](/docs/concepts/lock-in) describe the
dependency between stored vectors and their embedding model.

```sql
SELECT postvec.enable('public.docs', 'body',
                      model => 'sentence-transformers-all-minilm-l6-v2');
SELECT postvec.migrate('public.docs', 'body', new_model => 'baai-bge-m3');
```

## Scope

postvec supports PostgreSQL 16, 17, and 18 on hosts that permit shared preload
libraries. It is intended for semantic search and embedding-model changes
without sending a corpus to a hosted embedding provider. Knowledge of the
internal UniVec stack is not required.

RDS and Aurora are unsupported because the worker requires
`shared_preload_libraries = 'postvec'`.

## Starting points

| Initial state | Operation | Result |
|---|---|---|
| Text, no vectors | [`enable()`](/docs/guides/enable) | postvec creates and maintains a shadow vector column |
| Existing vector column, same model | [`adopt()`](/docs/guides/adopt) | existing vectors remain unchanged; missing rows can backfill |
| Existing vectors in an old/provider-only space | [Bridge search](/docs/guides/bridge) | queries are converted into that space; the corpus remains unchanged |
| Existing vectors, ready for a new model | [`migrate()`](/docs/guides/migrate) | stored vectors convert in place, or re-embed by explicit strategy |
| Long source documents | [Recursive chunking](/docs/guides/chunking) | a managed 1:N destination stores passage vectors; search returns documents |

## Basic workflow

1. **Install files** — Docker, apt/dnf, or source. File installation does not
   modify cluster state.
2. **Configure** — `postvec setup --embedded` for local inference, or configure
   remote ninference endpoints.
3. **Enable or adopt** a column. New vectors fill in the background;
   adopted vectors remain unchanged.
4. **Search** — `postvec.search(...)`, optional
   [filters](/docs/guides/filters).
5. **`migrate()`** to another model, or
   [bridge](/docs/guides/bridge) while leaving the column unchanged.

The [quick start](/docs/quickstart) provides the same workflow in Docker.

## Two inference modes

| | Embedded | Remote (`grpc`) |
|---|---|---|
| Where inference runs | Inside the PostgreSQL launcher | Separately operated ninference nodes |
| Text leaves the DB host | No | Only to the configured ninference service |
| Models | `postvec model …` on this host | Administered on that fleet |
| Catalogue | Public subset; private after `login` | Same channels, served by the fleet |
| Same SQL? | Yes | Yes |

[Embedded vs remote](/docs/concepts/modes) has the trade-offs.

## Out of scope

- Call OpenAI / Gemini / Cohere from PostgreSQL, or store their keys.
- Parse PDFs or HTML in the database.
- Provide `rag()` / chat-completion SQL. Generation belongs in the application.
- Invent a new index access method. Storage and ANN indexes are pgvector's.

## License

The extension, CLI, and their packages are under the **PostgreSQL License**.
Converter weights are a separate UniVec product. The public registry is
a subset; organisation accounts provide access to the full catalogue. The
bundled open-weight MiniLM model operates offline and does not require registry
access.

## Related documentation

- [Embedding debt and vector lock-in](/docs/concepts/lock-in)
- [Quick start](/docs/quickstart) — working search in one container
- [Install](/docs/install/) — Docker, packages, or source
- [Usage](/docs/guides/) — every SQL verb, with expected results
