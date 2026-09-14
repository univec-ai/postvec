---
title: Enable a column (CLI)
description: Confirm the model is served and check the host after postvec.enable().
---

# Enable a column (CLI)

SQL: [`enable()`](/docs/guides/enable). The CLI confirms the model is
served and that the worker is healthy after the call.

## 1. Confirm the model

```bash
postvec model ls
```

::::: tip Expected
The model you will pass to `enable()` is listed and `loaded` (embedded)
or advertised by postvec-server (remote). MiniLM is
`sentence-transformers-all-minilm-l6-v2`.
:::::

On remote mode, inventory is on the server:
[models on postvec-server](/docs/server/models).

## 2. After `enable()`

```bash
sudo postvec doctor --database app --deep
```

::::: tip Expected
`doctor` exits 0. Worker heartbeat advances. The enabled column appears
in the SQL [`status()`](/docs/guides/status) view (`pending_jobs`
returns to 0 as vectors fill).
:::::

Inside a container:

```bash
docker exec postvec postvec-healthcheck
docker exec -u postgres postvec \
  postvec doctor \
  --database-url 'postgresql:///app?host=/var/run/postgresql' \
  --database app --deep
```

See [enable (SQL)](/docs/guides/enable) for SQL options, disable and
refusals, and [troubleshooting](/docs/troubleshooting) for host-side
failures.
