---
title: Vector lock-in and embedding debt
description: Why stored embeddings are tied to one model, and what it costs to change.
---

# Vector lock-in and embedding debt

These terms describe a database dependency that is easy to omit from a
schema diagram.

## Vector lock-in

An embedding is a point in the vector space of the model that produced it.
`openai-text-embedding-ada-002`, BGE-M3, Gemini, Arctic, and GTE produce
different spaces. Matching dimensions do not make those spaces compatible.
A query embedded with model B against a corpus embedded with model A produces
invalid ranks.

Once the stored corpus, its ANN index, and every query caller assume space A,
the model is no longer an ordinary configuration value. It is part of the data
contract. That is **vector lock-in**.

## Embedding debt

Changing that contract normally requires a corpus re-embed, an index rebuild,
dual writes during cutover, a fresh retrieval evaluation, and a rollback plan.
If the source provider or model is deprecated, the work becomes time-sensitive.
Every new vector in the old space increases the eventual migration.

That accumulated, deferred migration work is **embedding debt**. It is not a
claim that every vector corpus should move immediately; it is a name for the
cost and risk that otherwise remain implicit.

## Two ways to service the debt

postvec's answers:

| Instead of | Use |
|---|---|
| Re-embed the corpus | [`migrate()`](/docs/guides/migrate) converts the stored vectors |
| Call a hosted embed API | [Embedded mode](/docs/concepts/modes) runs local open-weight models |
| Move the corpus just to try a new query model | Keep the column, [bridge the query](/docs/guides/bridge) |
| Hand-rolled hybrid SQL | [`search()`](/docs/guides/search) + [filters](/docs/guides/filters) |

Bridge search and migration are complementary:

- **Bridge search** keeps stored vectors exactly where they are and converts
  new query vectors into that space. It is the low-change adoption path.
- **In-place migration** converts the stored corpus to a new space without
  sending source text through the old embedding provider. It is the path when
  the database is ready to change its model contract.

## Related

- [Embedded vs remote](/docs/concepts/modes)
- [Search without migrating](/docs/guides/bridge)
- [`migrate()`](/docs/guides/migrate)
