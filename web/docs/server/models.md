---
title: Models on a node
description: Pull, activate, load, unload and remove models on a postvec-server node, plus external providers.
---

# Models on a node

Three separate things happen to a model on a node, and conflating them is the
most common day-one surprise:

| Step | What it does | Where it lands |
|---|---|---|
| `postvec model pull NAME --path $root` | Writes the descriptor and weights | The node's filesystem |
| `postvec model activate NAME --path $root` | Sets `enabled: true` in the descriptor | The node's filesystem |
| `postvec-server load NAME` | Makes it resident in the running engine | The node's engine |

`GET /config` advertises what the node can serve **right now**, which is what
is loaded. After a pull and an activate, a model is on disk and still invisible
to discovery until it is loaded, or until a restart loads every enabled
descriptor.

## Add a model

```bash
postvec model pull baai-bge-m3 --path /var/lib/postvec-server
postvec model activate baai-bge-m3 --path /var/lib/postvec-server
postvec-server load baai-bge-m3
postvec-server status
```

:::: tip Expected
`pull` prints a plan with the download size, peak disk and licences, then
installs the model **deactivated**. `activate` sets `enabled: true` so a
restart would load it. `postvec-server load` makes it resident now and
returns a per-model result. `status` then lists it, and the database sees
it in `postvec.models` after the next discovery refresh.
::::

A deactivated descriptor never loads, on any path. That is why
`model activate` means the same thing on a node as it does on a database
host. `load` of a deactivated model fails and names `activate`.

The registry side of `pull` (the catalogue, private models, `postvec login`) is
identical to the database-host case: [pull, activate, upgrade,
remove](/docs/models/pull).

## Remove a model

```bash
postvec-server unload baai-bge-m3            # out of memory, still on disk
postvec model deactivate baai-bge-m3 --path /var/lib/postvec-server
postvec model rm baai-bge-m3 --path /var/lib/postvec-server
```

Unloading frees memory now. Deactivating is what keeps it from coming back at
the next restart. Removing deletes the files.

`--path` manages a root as **files only**: it flips descriptors and deletes
directories, with no engine load and no SQL refresh. Stop or drain the node
before replacing files under a root it is serving from.

Removing a model that a database column depends on takes that column's
embedding route away. On a `--path` root the CLI cannot see any database, so
nothing warns you. Check from the database side first:

```sql
SELECT DISTINCT model FROM postvec.status();
```

## Air-gapped nodes

The same directory-copy flow works, because a model is an open directory plus
a receipt file:

```bash
# On a connected host
postvec model pull --path /tmp/stage baai-bge-m3
tar -C /tmp/stage -czf models.tgz models/

# On the node
sudo tar -C /var/lib/postvec-server -xzf models.tgz
postvec model show --path /var/lib/postvec-server baai-bge-m3 --verify
postvec model activate --path /var/lib/postvec-server baai-bge-m3
postvec-server load baai-bge-m3
```

Details and the verification contract: [air-gapped
hosts](/docs/models/air-gapped).

## What a load refuses, and why

| Situation | Outcome |
|---|---|
| Not on disk | Per-model error naming `postvec model pull` |
| `enabled: false` | Per-model error naming `postvec model activate` |
| The same name under two backends | Per-model error. The engine would pick by directory order, so the node refuses to pick at all |
| Not in a configured `--models` allow-list | Per-model error |
| Would exceed `--max-resident-models`, closure included | Per-model error |

Per-model outcomes ride a `200`. Only request-level problems (a malformed
body, an impossible name, more than 32 models at once) are `4xx`.

## External providers

A node can also serve hosted embedding APIs from connector files under
`$root/providers.d`. A UniVec file can also declare direct hosted converters.
There is no engine descriptor or load step: the connector file is the serving
truth, and its entries appear in `/config` next to the loaded models.

```bash
sudo postvec provider add openai --model text-embedding-3-small \
     --path /var/lib/postvec-server \
     --acknowledge-in-use --yes
postvec provider ls --path /var/lib/postvec-server
```

The key stays in that file, `0600`, in a `0700` directory owned by the
account the node runs as. New files inherit the root's owner, so a `sudo`
run still writes files the node can read. Nothing about them reaches
PostgreSQL. `--path` has no cluster to scan, so
`--acknowledge-in-use` is always required.

Changes apply at the next restart, or immediately through the loopback
admin port, which is what the `provider` commands try for you when run on
the node itself:

```bash
curl -s -X POST http://127.0.0.1:22223/admin/providers/reload
```

If two connector files claim the same public name, neither serves. If a
loaded local model and a provider file claim the same name, the **local
model wins**, in `/config` and on the embed or convert path alike.

Format, key sources and the rest: [external
providers](/docs/models/providers), then [connector
files](/docs/models/providers-file).

UniVec conversion setup: [UniVec hosted models](/docs/models/univec).

## Related documentation

- [Run a fleet](/docs/server/fleet) - every node carries the same set
- [Pull, activate, upgrade, remove](/docs/models/pull) - the CLI in general
- [How models work](/docs/models/) - discovery, direct and bridged routes
