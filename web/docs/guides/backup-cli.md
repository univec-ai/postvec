---
title: Backup and restore (CLI)
description: Cluster files, setup and doctor after a pg_dump restore.
---

# Backup and restore (CLI)

See [backup and restore](/docs/guides/backup) for what `pg_dump` carries,
`refresh_models()` and the queue checks. The CLI restores cluster
configuration and proves the host.

After the dump is restored:

```bash
sudo postvec setup --database app --embedded
sudo postvec doctor --database app --deep
```

Remote mode, point at the same endpoints as before the dump:

```bash
sudo postvec setup --database app \
  --grpc 10.0.0.20:33333 \
  --http https://10.0.0.20:22222 \
  --switch-mode
sudo postvec doctor --database app --deep
```

Remote mode also restores the [postvec-server](/docs/server/) side: every
node must serve the same enabled models and the same converters as before
the dump. A node that lacks one fails that request with `MODEL_NOT_LOADED`
while its peers answer, so the failure looks intermittent.
[`refresh_models()`](/docs/guides/helpers) reloads postvec's cache from
the node list, and [fleet](/docs/server/fleet) covers inventory parity.

::::: tip Expected
`doctor` exits 0. Worker heartbeat advances. `99-postvec.conf` matches
the restored databases in `postvec.database`.
:::::

Install matching packages for that PostgreSQL major first:
[packages](/docs/install/packages). A missing name in
`postvec.database` makes the worker fail and respawn until the list is
edited and PostgreSQL is restarted.

On a container host, `postvec doctor` finds no `pg_lsclusters` cluster:
[Docker](/docs/install/docker#diagnose).
