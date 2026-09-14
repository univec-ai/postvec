---
title: Air-gapped hosts
description: Offline model transfer using ordinary directories and receipts.
---

# Air-gapped hosts

A model is an open directory plus `.postvec-install.json`, so it travels with
an ordinary file copy. Pull where there is a network, move the tarball, then
verify and activate it on the isolated host.

## Embedded host

```bash
# Connected host
postvec model pull --path /tmp/postvec-stage baai-bge-m3
tar -C /tmp/postvec-stage -czf postvec-models.tgz models/

# Isolated host
sudo tar -C /opt/postvec -xzf postvec-models.tgz
postvec model show --path /opt/postvec baai-bge-m3 --verify
sudo postvec model activate baai-bge-m3        # or --all for everything staged
```

`pull` stages deactivated copies. The `activate` on the isolated host loads
them, keeps them on across restarts and refreshes the SQL caches. `--path` on
the connected host is files only, so you can also flip the descriptors there
(`postvec model activate --path /tmp/postvec-stage baai-bge-m3`) and ship them
pre-activated. Stop any process that is using the root before you replace
files in it.

## postvec-server nodes

In remote mode the models live on each [postvec-server](/docs/server/models)
node, and a `pull` against the cluster is refused on the database host. Unpack
the tarball into the node's engine root and load it there. The root is
`/opt/postvec` by default, or the one given by `--root` or
`POSTVEC_SERVER_ROOT`.

```bash
# On the node
sudo tar -C /opt/postvec -xzf postvec-models.tgz
postvec model show --path /opt/postvec baai-bge-m3 --verify
postvec model activate --path /opt/postvec baai-bge-m3
postvec-server load baai-bge-m3
```

`activate` writes `enabled: true`, which is what a restart reads.
`postvec-server load` makes the model resident now. Repeat on every node in the
[fleet](/docs/server/fleet), and drain a node before you replace files under a
root it is serving from.

:::: tip Expected
`show --verify` hashes every file against the receipt with **no**
catalogue present. `doctor --deep` does the same across every
CLI-installed model. `registry.reachable` is a **warning**. Offline
hosts are supported.
::::

## Withdrawn names

A withdrawn model stays on disk if it is already installed. A later
`pull` of that name fails.
