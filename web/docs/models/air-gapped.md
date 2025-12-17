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
sudo postvec model show --path /opt/postvec/ninference baai-bge-m3 --verify
sudo postvec model activate
```

`--path` is files only. `activate` (on the embedded cluster) is what
loads them and refreshes SQL caches.

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
