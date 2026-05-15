---
title: Vector lock-in and embedding debt
description: The dependency between stored vectors and their embedding model.
---

# Vector lock-in and embedding debt

Stored vectors are bound to the model that produced them.

## Vector lock-in

An embedding is a point in the vector space of the model that produced
it. `openai-text-embedding-ada-002`, BGE-M3, Gemini, Arctic and GTE each
live in a different space, even when dimensions match. Embed a query
with model B and search a corpus from model A, and the ranks are
invalid.

Once the stored corpus, its ANN index and every query caller assume
space A, the model is part of the data contract. That binding is
vector lock-in.

## Embedding debt

Changing that contract normally means a corpus re-embed, an index
rebuild, dual writes during cutover, a fresh retrieval evaluation and a
rollback plan. Deprecation of the source provider or model makes the
same work time-sensitive. Every new vector written in the old space
grows the eventual migration.

That accumulated, deferred migration work is **embedding debt**.

## Operations

| Requirement | postvec operation |
|---|---|
| Change the stored space without replaying source text | [`migrate()`](/docs/guides/migrate) converts the stored vectors |
| Keep inference on the database host | [Embedded mode](/docs/concepts/modes) runs local open-weight models |
| Query with a different model without moving the corpus | Keep the column and [bridge the query](/docs/guides/bridge) |
| Combine lexical and semantic retrieval | [`search()`](/docs/guides/search) + [filters](/docs/guides/filters) |

Bridge search and migration sit next to each other:

- **Bridge search** keeps stored vectors exactly where they are and
  converts new query vectors into that space. The stored corpus stays
  put.
- **In-place migration** converts the stored corpus to a new space
  without sending source text through the old embedding provider. The
  column's model contract changes.

## Related

- [Embedded vs remote](/docs/concepts/modes)
- [Search a retired space](/docs/guides/bridge)
- [`migrate()`](/docs/guides/migrate)
