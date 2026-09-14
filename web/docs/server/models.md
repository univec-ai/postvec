---
title: Models on postvec-server
description: Pull, activate, load, unload and remove models on postvec-server, plus external providers.
---

# Models on postvec-server

Pull, activate and load are separate steps:

| Step | What it does | Where it lands |
|---|---|---|
| `postvec model pull NAME --path $root` | Writes the descriptor and weights | The node's filesystem |
| `postvec model activate NAME --path $root` | Sets `enabled: true` in the descriptor | The node's filesystem |
| `postvec-server load NAME` | Makes it resident in the running engine | The node's engine |

`$root` is the engine root, `/opt/postvec` by default.

`GET /config` advertises what the node can serve **right now**, which is what
is loaded. After a pull and an activate, a model is on disk and still invisible
to discovery until it is loaded, or until a restart loads every enabled
descriptor.

The shipped postvec-server build runs inference on CPU. GPU inference needs a
build with the `ort-cuda` or `ort-tensorrt` Cargo feature; without one the
engine skips the requested execution provider and logs a warning.

## Add a model

```bash
postvec model pull baai-bge-m3 --path $root
postvec model activate baai-bge-m3 --path $root
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

A deactivated descriptor is skipped at load and at restart.
`model activate` is the same switch on a node as on a database host.
`load` of a deactivated model fails and names `activate`.

The same pull, activate, deactivate and remove steps are available from
the node's [dashboard](/docs/server/dashboard) (Registries tab) when the
node runs with `--manage`. Model upgrades are CLI-only:
`postvec model upgrade`.

The registry side of `pull` (the catalogue, private models, `postvec login`)
is identical to the database-host case: [pull, activate, upgrade,
remove](/docs/models/pull).

## Remove a model

```bash
postvec-server unload baai-bge-m3            # out of memory, still on disk
postvec model deactivate baai-bge-m3
postvec model rm baai-bge-m3
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
sudo tar -C /opt/postvec -xzf models.tgz
postvec model show baai-bge-m3 --verify
postvec model activate baai-bge-m3
postvec-server load baai-bge-m3
```

Details and the verification contract: [air-gapped
hosts](/docs/models/air-gapped).

## Load refusals

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
     --acknowledge-in-use --yes
postvec provider ls
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

If two connector files claim the same public name, neither serves. A
`[[models]]` entry whose name is already served by a local engine model is
refused at load, because a public name has exactly one owner: the host refuses
the whole connector file and reports the collision in the reload result, so
the entry never mounts. A model pulled and loaded after the last reload serves
its name at once, and the next reload refuses the file. `postvec doctor` lists
those models as configured but not served. Rename the entry in the file.

Format, key sources and the rest: [external
providers](/docs/models/providers), then [connector
files](/docs/models/providers-file).

UniVec conversion setup: [UniVec hosted models](/docs/models/univec).

- [Dashboard](/docs/server/dashboard)
- [Fleet](/docs/server/fleet)
- [Pull, activate, upgrade, remove](/docs/models/pull)
- [How models work](/docs/models/)
