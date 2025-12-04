---
title: Air-gapped hosts
description: Move models as ordinary directories plus receipts. No private bundle format.
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

::: tip Expected
`show --verify` hashes every file against the receipt with **no**
catalogue present. `doctor --deep` does the same across every
CLI-installed model. `registry.reachable` is a **warning**, never a
failure — offline hosts are a supported shape.
:::

Stop any process using the root before you replace files via `--path`.

## Withdrawn names

A withdrawn model cannot be pulled again. Do not `rm` it on a host that
must keep serving it. Installed copies keep working.
