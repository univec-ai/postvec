---
title: How models work
description: Discovery, embed versus convert and which CLI commands apply in each mode.
---

# How models work

postvec does not download models as a side effect of anything. Package
install does not. `setup` does not. `search()` does not.

A name in `postvec.models` is a cache of what the engine currently
advertises. Workers refresh it on an interval and prune stale entries
only after **every** discovery endpoint succeeded.

```sql
SELECT name, model_type, target_model, target_dim
  FROM postvec.models
 ORDER BY name;
```

## Direct and bridged embedding

When `model => 'some-name'` is supplied to `enable()`, `adopt()` or
`search()`, resolution uses:

1. A direct embed model with that public name, or
2. A converter *targeting* that name whose source is embeddable, routed
   through an `embed-bridge` executor.

A convert-only space such as `ada-002` can then accept new writes and
query embeddings without the original provider or a corpus migration.
See [query an existing space](/docs/guides/bridge).

Conversion itself resolves a direct converter or a two-hop
`convert-bridge`. Write, search, one-shot and migration paths share the
same resolver.

## Catalogue channels

| Channel | Access | Contents |
|---|---|---|
| **Public** | Anyone, no login | A subset of open-weight embedders and a subset of conversion pairs |
| **Private** | A verified UniVec account and `postvec login` | The full embedding suite, nearly 100 conversion pairs and additional converter variants |

Public entries retain their public download URLs after authentication.
The private channel is a **superset**. Details:
[login](/docs/models/login).

:::: info Registry publication
The client, schema and authenticated route are implemented, but the
public and private catalogue buckets are still in release preview. The
bundled MiniLM package works without the registry. The
[release page](/download) is the source for publication status.
::::

## Model installation by mode

| Mode | Install / remove / upgrade |
|---|---|
| Embedded | `postvec model pull / upgrade / rm / activate` on this host |
| Remote | The ninference fleet (`nin`). Local mutation is **refused** |
| Either | `model ls` - local inventory, or node-advertised names in remote |

`--path DIR` manages a standalone engine root as **files only**: no
hot-load, no SQL refresh.

## Bundled model

The embedded package/image ships `sentence-transformers-all-minilm-l6-v2`
(384-d). It is package-owned. `model rm` will not delete it.

## Revisions are not migrations

A published name is one vector-space identity. Its head revision can
move forward. `model upgrade` replaces bytes; it does **not** rewrite
stored vectors. A column served across four revisions holds a mix of
four generations. To make a column uniform, re-embed or
[`migrate()`](/docs/guides/migrate).

Upgrading the extension is a different lifecycle. Neither implies the
other.

## Related documentation

- [Pull, upgrade, remove](/docs/models/pull)
- [Login](/docs/models/login) - private catalogue
- [Air-gapped](/docs/models/air-gapped)
