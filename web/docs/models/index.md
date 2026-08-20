---
title: How models work
description: Discovery, embed versus convert and which CLI commands apply in each mode.
---

# How models work

`postvec.models` caches what the engine currently advertises.

On an embedded host, add models with `postvec model pull`. In remote
mode, administer them on the `postvec-server` nodes ([models on a
node](/docs/server/models)). Models load from the engine root on disk.

Workers refresh the cache on an interval and prune stale entries only
after **every** discovery endpoint succeeded.

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
query embeddings with the original provider gone and the stored corpus
unchanged. That is the usual first path for a populated column. See
[search a retired space](/docs/guides/bridge).

`convert()` and `strategy => 'convert'` migrations require a direct converter.
That converter can be a local model or a UniVec hosted route. Use
[`migrate()`](/docs/guides/migrate) after search on the current space is
working.

## Hosted models

A model in `postvec.models` can also come from a connector file on the
inference side. Those files declare embed models served by OpenAI, Gemini,
Cohere, Mistral, AWS Bedrock, OpenRouter or UniVec. UniVec can also declare
direct converters. They appear in the same cache with `model_type`, source
and target names and dimensions. The connector file is what the host
serves. There is no `pull` or `activate` step.

The credential lives in that file, never in PostgreSQL.
[External providers](/docs/models/providers) is the walkthrough;
[connector files](/docs/models/providers-file) is the format. See
[UniVec hosted models](/docs/models/univec) for direct conversion.

Two connector files that claim the same public name serve neither. A name
a loaded local model already owns stays local.

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

| Mode | Install / activate / remove / upgrade |
|---|---|
| Embedded | `postvec model pull / activate / deactivate / upgrade / rm` on this host |
| Remote | Each `postvec-server` node, via the CLI or the [dashboard](/docs/server/dashboard). Cluster-local `model pull` is refused. See [models on a node](/docs/server/models) |
| Either | `model ls` - local inventory, or node-advertised names in remote |

`pull` installs a model **deactivated**. `model activate` / `deactivate`
rewrite the descriptor's `enabled` field, so the choice survives a
PostgreSQL restart.

`--path DIR` manages a standalone engine root as **files only**: it flips
descriptors, but performs no engine load and no SQL refresh.

## Bundled model

The extras package and the local image ship `sentence-transformers-all-minilm-l6-v2`
(384-d). It is package-owned, so `model rm` leaves it in place.

## Model revisions

A published name is one vector-space identity. Its head revision can
move forward. `model upgrade` replaces model files. Stored vectors stay
as they are. A column served across four revisions holds a mix of
four generations. To make a column uniform, re-embed or
[`migrate()`](/docs/guides/migrate).

Upgrading the extension is a separate lifecycle.

- [Pull, upgrade, remove](/docs/models/pull)
- [Login](/docs/models/login)
- [Air-gapped](/docs/models/air-gapped)
- [External providers](/docs/models/providers)
- [UniVec hosted models](/docs/models/univec)
- [Connector files](/docs/models/providers-file)
