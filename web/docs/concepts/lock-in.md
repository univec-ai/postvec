---
title: Vector lock-in and embedding debt
description: Why stored embeddings are tied to one model, and what it costs to change.
---

# Vector lock-in and embedding debt

Two names for a fact most people already know and then ignore.

**Vector lock-in.** An embedding is a point in the model that produced
it. `text-embedding-ada-002`, BGE-M3, Gemini, Arctic, GTE are different
spaces. Same document, incomparable vectors. Matching dimensions do not
help. A query embedded with model B against a corpus embedded with
model A ranks garbage.

Once the corpus, the HNSW index and every caller assume space A, the
model is no longer a config value. It is load-bearing.

**Embedding debt.** The cost of leaving that space: re-embed the
corpus, rebuild the index, dual-write during cutover, re-run retrieval
eval, or sit on a deprecated provider. Each new row in space A makes
the next move larger. People stop changing models because the
migration *is* the project.

postvec's answers:

| Instead of | Use |
|---|---|
| Re-embed the corpus | [`migrate()`](/docs/guides/migrate) converts the stored vectors |
| Call a hosted embed API | [Embedded mode](/docs/concepts/modes) runs local open-weight models |
| Move the corpus just to try a new query model | Keep the column, [bridge the query](/docs/guides/bridge) |
| Hand-rolled hybrid SQL | [`search()`](/docs/guides/search) + [filters](/docs/guides/filters) |

- [Embedded vs remote](/docs/concepts/modes)
- [Query an existing space](/docs/guides/bridge)
- [migrate()](/docs/guides/migrate)
