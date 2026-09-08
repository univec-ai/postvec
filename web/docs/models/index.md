---
title: How models work
description: Discovery, embed versus convert and which CLI commands apply in each mode.
---

# How models work

`postvec.models` caches what the engine currently advertises.

On an embedded host, add models with `postvec model pull`. In remote
mode, administer them on [postvec-server](/docs/server/models). Models
load from the engine root on disk.

Workers refresh the cache on an interval and prune stale entries only
after **every** discovery endpoint succeeded.

```sql
SELECT name, model_type, target_model, target_dim
  FROM postvec.models
 ORDER BY name;
```

## Spaces and routes

You pull, add, remove, test and prefer **routes**. You bind, search,
migrate and convert in **spaces**. A space is a vector-space identity
(`baai-bge-m3`, `gemini-embedding-001`). A route is a way to produce
vectors in that space: a local engine model, or a hosted provider
entry. Routes keep unique, host-prefixed names. Several routes may
serve one space.

Columns bind to spaces. Binding to a route name still works: if that
route is served, it is used; otherwise resolution treats the string as
a space (or as the vanished route's remembered space) and picks the
served route with the lowest priority. Local models are priority 100.
Provider routes without an explicit `priority` follow the order they
were added.

`postvec.routes` lists every embed route. `model prefer` changes the
order without a migration. `model set-space` edits a mis-declared
space. Adding a route never changes where an existing column's text
goes.

## Direct and bridged embedding

When `model => 'some-name'` is supplied to `enable()`, `adopt()` or
`search()`, resolution uses:

1. A served embed route with that exact name, else the preferred route
   of that space, or
2. A converter *targeting* that space whose source is embeddable, routed
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
inference side. Those files declare embed models served by
[OpenAI](/docs/models/openai), [Cohere](/docs/models/cohere),
[Amazon Bedrock](/docs/models/aws), [Gemini](/docs/models/gemini),
[Mistral](/docs/models/mistral), [OpenRouter](/docs/models/openrouter)
or [UniVec](/docs/models/univec). UniVec can also declare direct
converters. They appear in the same cache with `model_type`, source and
target names and dimensions. `provider add` writes the connector file
and the host serves it. `pull` / `activate` apply to local models.

The credential lives in that file, never in PostgreSQL.
[External providers](/docs/models/providers) is the walkthrough;
each provider page is a copy-paste setup. In remote mode the files live
on [postvec-server](/docs/server/).

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
| Remote | Each `postvec-server`, via the CLI or the [dashboard](/docs/server/dashboard). Cluster-local `model pull` is refused. See [models on postvec-server](/docs/server/models) |
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
- [OpenAI](/docs/models/openai) · [Cohere](/docs/models/cohere) ·
  [Amazon Bedrock](/docs/models/aws) · [Gemini](/docs/models/gemini) ·
  [Mistral](/docs/models/mistral) · [OpenRouter](/docs/models/openrouter)
- [UniVec hosted models](/docs/models/univec)
- [Connector files](/docs/models/providers-file)
- [postvec-server](/docs/server/)
