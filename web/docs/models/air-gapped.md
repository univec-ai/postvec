---
title: Air-gapped hosts
description: Offline model transfer using ordinary directories and receipts.
---

# Air-gapped hosts

A model is an open directory plus `.postvec-install.json`. There is no
private bundle format and no `install` verb.

```bash
# Connected host
postvec model pull --path /tmp/postvec-stage baai-bge-m3
tar -C /tmp/postvec-stage -czf postvec-models.tgz models/

# Isolated host
sudo tar -C /opt/postvec/ninference -xzf postvec-models.tgz
postvec model show --path /opt/postvec/ninference baai-bge-m3 --verify
sudo postvec model activate baai-bge-m3        # or --all for everything staged
```

The copied models arrive **deactivated** — that is how `pull` installs them —
so the `activate` on the isolated host is what makes them serve, there and at
every restart afterwards. It also refreshes the SQL caches. `--path` on the
connected host is files only; you can flip the descriptors there too
(`postvec model activate --path /tmp/postvec-stage baai-bge-m3`) if you would
rather ship them pre-activated.

:::: tip Expected
`show --verify` hashes every file against the receipt with **no**
catalogue present. `doctor --deep` does the same across every
CLI-installed model. `registry.reachable` is a **warning**. Offline
hosts are supported.
::::

Processes using the engine root must be stopped before files are
replaced via `--path`.

## Withdrawn names

A withdrawn model cannot be pulled again. Installed copies remain
functional, but removal prevents subsequent restoration from the
catalogue.
