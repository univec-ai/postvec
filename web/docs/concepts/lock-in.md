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

The first move is to keep the stored space and search it:

- [Search a retired space](/docs/guides/bridge) converts each query
  vector into the stored space (embed-bridge). The corpus stays.
- [`search()`](/docs/guides/search) plus [filters](/docs/guides/filters)
  combines lexical and semantic retrieval.
- [Embedded mode](/docs/concepts/modes) runs local open-weight models on
  the database host.

If later you want the stored contract itself to change,
[`migrate()`](/docs/guides/migrate) converts stored vectors without
replaying source text. The column's model contract then changes.

- [Embedded vs remote](/docs/concepts/modes)
- [Search a retired space](/docs/guides/bridge)
- [`migrate()`](/docs/guides/migrate)
