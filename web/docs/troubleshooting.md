---
title: Troubleshooting
description: Diagnostic checks and common postvec failure conditions.
---

# Troubleshooting

Start with:

```bash
sudo postvec doctor --database app --deep
```

On a container host the same check is `postvec-healthcheck`, or `doctor`
with `--database-url 'postgresql:///app?host=/var/run/postgresql'`.

## Symptom table

| Symptom | Meaning or next action |
|---|---|
| `CREATE EXTENSION` cannot find pgvector | Install pgvector ≥ 0.8 for the **same** major |
| `CREATE EXTENSION` is denied | Superuser; postvec is untrusted |
| No advancing `worker_last_beat` | Preload, `postvec.database`, restart finished, worker slots, server log |
| Jobs pile up with endpoint errors | Restore the inference nodes; an empty endpoint list does not burn attempts |
| Embedded engine will not start | Engine path, unversioned `libonnxruntime.so`, readable descriptors, loopback ports |
| Search returns FTS only | Query embedding failed while degradation was enabled; restore inference or disable degradation |
| Search is slow | No usable ANN index for the entry's distance |
| `status().index_error` set | Resolve the recorded automatic-index failure, then run `create_vector_index()` |
| Entry becomes disabled | Table / source / vector / template column vanished; worker quarantined it |
| Migration is `awaiting_index` | Run `suggested_index_sql`, finalize again |
| Rows in `jobs_dead` | Fix `last_error`, then `retry_dead()` |
| Worker log wants `ALTER EXTENSION` | Library / SQL skew. Finish [upgrade](/docs/install/upgrade) |
| `setup` refuses `99-postvec.conf` | Foreign or modified; `--yes` will not override |
| Container `doctor` finds no cluster | Expected. Healthcheck or a socket URL. |
| Worker FATALs for a missing database | Name still in the **running** launcher list. `uninstall` or edit **and restart** |
| `DROP DATABASE` is blocked | Worker holds a connection. `uninstall` then `dropdb --force` |
| `sudo model pull` is anonymous | Credentials are per user. `sudo postvec login` |
| `model pull` says package/manual owned | Pull through the package manager or as the original owner. |
| Remote `model pull` returns an error | Expected; models are administered on each `postvec-server` node |
| Uninstall exits 3 | SQL changed; config left alone. Follow the printed file/line |
| Exit 4 | Restart the selected cluster, then `doctor --deep` |

## Vectors stay NULL

1. `SELECT * FROM postvec.status();` - pending vs dead vs last_error.
2. `SELECT * FROM postvec.jobs_dead;`
3. `SHOW shared_preload_libraries; SHOW postvec.database; SHOW postvec.mode;`
4. `sudo postvec doctor --database app --deep`

If pending stays non-zero and `doctor` fails endpoint checks, inference
is unavailable. Jobs remain pending without consuming retry attempts.

## Version skew

Replacing `postvec.so` does not upgrade a database. Until
`ALTER EXTENSION postvec UPDATE` the worker pauses and writes **no**
heartbeat. Application backends are not gated. Application traffic
should stay paused for that window.

## Further diagnostics

`postvec doctor --format json --deep` produces a persistent diagnostic
artifact. Each check includes an identifier, status and remediation.
