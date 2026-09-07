---
title: Status (CLI)
description: postvec doctor, healthcheck and JSON diagnostics.
---

# Status (CLI)

SQL: [`status()` / `stats()`](/docs/guides/status). The CLI adds
read-only checks of the host installation, cluster settings and the
heartbeat.

```bash
sudo postvec doctor --database app --deep
sudo postvec doctor --database app --deep --format json \
  | jq '{summary, failures: [.checks[] | select(.status == "FAIL")]}'
```

| Flag | Effect |
|---|---|
| `--deep` | Heartbeat must advance; hash CLI-installed model receipts |
| `--strict` | Warnings fail the command |
| `--format json` | Versioned object on stdout; progress on stderr |

`doctor` is read-only: host files, cluster settings and the heartbeat.

::::: tip Expected
Exit 0. JSON `summary` has no `FAIL` checks. If `doctor` (or the worker
log) asks for `ALTER EXTENSION`, the library and installed SQL disagree:
[upgrade](/docs/install/upgrade).
:::::

Inside a container, use `postvec-healthcheck` or an explicit socket URL.
See [Docker](/docs/install/docker).

```bash
docker exec postvec postvec-healthcheck
docker exec -u postgres postvec \
  postvec doctor \
  --database-url 'postgresql:///app?host=/var/run/postgresql' \
  --database app --deep
```

On postvec-server:

```bash
postvec-server status
curl -sk https://127.0.0.1:22222/ready
```

`status` prints version, engine root, addresses and loaded models.
`/ready` is `200` once a model can serve.

Column queues and worker counters: [status (SQL)](/docs/guides/status).
Symptom table: [troubleshooting](/docs/troubleshooting).
