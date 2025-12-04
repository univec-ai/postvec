---
title: How models work
description: Discovery, embed vs convert, and which CLI commands apply in each mode.
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

## Two tiers of embedding

When you write `model => 'some-name'` on `enable()` / `adopt()` /
`search()`:

1. A direct embed model with that public name, or
2. A converter *targeting* that name whose source is embeddable, routed
   through an `embed-bridge` executor.

That is how a convert-only space (classic example: `ada-002`) can still
accept new writes and query embeddings **without** the original provider
and without migrating the corpus. See
[query an existing space](/docs/guides/bridge).

Conversion itself resolves a direct converter or a two-hop
`convert-bridge`. All write, search, one-shot, and migration paths use
the same resolver.

## Two catalogues

| Channel | Who | What is on it |
|---|---|---|
| **Public** | Anyone, no login | A subset of open-weight embedders and a subset of conversion pairs |
| **Private** | Organisation accounts, `postvec login` | The full embed suite, 100+ conversion pairs, higher-fidelity converters |

Public entries keep their public download URLs even when you are logged
in. The private channel is a **superset**. Details:
[login](/docs/models/login).

## Who installs bytes

| Mode | Install / remove / upgrade |
|---|---|
| Embedded | `postvec model pull / upgrade / rm / activate` on this host |
| Remote | The ninference fleet (`nin`). Local mutation is **refused** |
| Either | `model ls` — local inventory, or node-advertised names in remote |

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

## Next

- [Pull, upgrade, remove](/docs/models/pull)
- [Login](/docs/models/login) — private catalogue
- [Air-gapped](/docs/models/air-gapped)
