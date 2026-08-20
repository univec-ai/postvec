---
title: One-shot helpers
description: Administrative embed, convert and refresh_models calls.
---

# One-shot helpers

`embed()`, `convert()` and `refresh_models()` are administrative. They
are revoked from PUBLIC. Application search uses
[`search()`](/docs/guides/search).

Use these to prove a model loads, to translate one vector or to rebuild
the SQL model cache. To keep searching an existing space, see
[search a retired space](/docs/guides/bridge). To change a stored column
after that, use [`migrate()`](/docs/guides/migrate).

:::: code-group

```sql [SQL]
SELECT vector_dims(postvec.embed(
  'the isolated image performs inference',
  'sentence-transformers-all-minilm-l6-v2'
)::vector);

SELECT postvec.convert(
  postvec.embed('...', 'sentence-transformers-all-minilm-l6-v2'),
  'sentence-transformers-all-minilm-l6-v2',
  'baai-bge-m3'
);

SELECT postvec.refresh_models();
```

```bash [CLI]
postvec model ls
sudo postvec doctor --database app --deep
```

::::

:::: tip Expected
MiniLM returns a 384-d vector. `refresh_models()` returns the number of
rows written into the `postvec.models` cache. `model ls` lists inventory;
`doctor` checks the host.
::::

## Functions

| Function | Use |
|---|---|
| `embed(text, model)` / `embed(text[], model)` | Confirm a model loads, or produce a vector for `search_with_vector()` |
| `convert(embedding, source, target)` | Translate one vector through a local or direct UniVec hosted converter without starting a column migration |
| `refresh_models()` | Rebuild the SQL cache after an inference-node inventory change |

Grant explicitly if an application role must call them:

```sql
GRANT EXECUTE ON FUNCTION postvec.embed(text, text) TO app_admin;
GRANT EXECUTE ON FUNCTION postvec.convert(real[], text, text) TO app_admin;
GRANT EXECUTE ON FUNCTION postvec.refresh_models() TO app_admin;
```

Workers refresh `postvec.models` on an interval. `refresh_models()` is
the immediate path after `postvec model pull` (already done by the CLI
on an embedded cluster) or after a remote fleet change.

Column-level work stays on [`enable`](/docs/guides/enable),
[`adopt`](/docs/guides/adopt),
[search a retired space](/docs/guides/bridge) and
[`migrate`](/docs/guides/migrate).
A one-shot `convert()` returns a vector.
