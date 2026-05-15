---
title: Troubleshooting
description: Diagnostic checks and common postvec failure conditions.
---

# Troubleshooting

`doctor --deep` is the first command. The table below maps a symptom to
the usual cause. Start with:

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
| Jobs pile up with endpoint errors | Restore the inference nodes; an empty endpoint list leaves jobs pending |
| Embedded engine will not start | Engine path, unversioned `libonnxruntime.so`, readable descriptors, loopback ports |
| Search returns FTS only | Query embedding failed while degradation was enabled; restore inference or disable degradation |
| Search is slow | No usable ANN index for the entry's distance |
| `status().index_error` set | Resolve the recorded automatic-index failure, then run `create_vector_index()` |
| Entry becomes disabled | Table / source / vector / template column vanished; worker quarantined it |
| Migration is `awaiting_index` | Run `suggested_index_sql`, finalize again |
| Rows in `jobs_dead` | Fix `last_error`, then `retry_dead()` |
| Worker log wants `ALTER EXTENSION` | Library / SQL skew. Finish [upgrade](/docs/install/upgrade) |
| `setup` refuses `99-postvec.conf` | Foreign or modified; `--yes` still refuses |
| Container `doctor` finds no cluster | Expected. Healthcheck or a socket URL. |
| Worker FATALs for a missing database | Name still in the **running** launcher list. `uninstall` or edit **and restart** |
| `DROP DATABASE` is blocked | Worker holds a connection. `uninstall` then `dropdb --force` |
| `sudo model pull` is anonymous | Credentials are per user. `sudo postvec login` |
| `model pull` says package/manual owned | Pull through the package manager or as the original owner. |
| Remote `model pull` returns an error | Expected; models are administered on each [`postvec-server` node](/docs/server/models) |
| Remote `MODEL_NOT_LOADED`, intermittently | Fleet inventory drift. `postvec-server status --fleet` names the model and the nodes missing it |
| `provider ls` says `NOT served` | The host has not reloaded the connector file. Rerun a `provider` command or restart. `doctor` names which |
| `provider ls` says the host **REFUSES** a file | The file will not load however often you reload. Fix what the line names. `ls` and `doctor` apply the loader's own rules |
| A connector file is refused | Its mode, or a referenced key file's mode, grants group or other bits. `chmod 600`, then rotate the key |
| `provider add` demands `--acknowledge-in-use` | Existing columns start sending source text to the provider, or `--path` has no cluster to scan. Pass that flag with `--yes` |
| Provider jobs retry with a 401 | Bad or revoked key. Fix it, then `retry_dead()` for rows that already gave up |
| UniVec provider jobs retry with a 402 | No available credit, or the key's spending limit is exhausted. Fix the account limit, then `retry_dead()` for dead rows |
| A hosted converter exists but bridge search has no route | Provider converters are direct `migrate()` / `convert()` routes. Load the embed model, local converter and bridge executor on one engine |
| Uninstall exits 3 | SQL changed; config left alone. Follow the printed file/line |
| Exit 4 | Restart the selected cluster, then `doctor --deep` |

## Vectors stay NULL

1. `SELECT * FROM postvec.status();` - pending vs dead vs last_error.
2. `SELECT * FROM postvec.jobs_dead;`
3. `SHOW shared_preload_libraries; SHOW postvec.database; SHOW postvec.mode; SHOW postvec.path;`
4. `sudo postvec doctor --database app --deep`

If pending stays non-zero and `doctor` fails endpoint checks, inference
is unavailable. Jobs remain pending without consuming retry attempts.

## Version skew

Replacing `postvec.so` parks the worker until
`ALTER EXTENSION postvec UPDATE`. During that window the worker writes
no heartbeat. Application backends still load the new library, so keep
application traffic paused until every database is updated.

## Further diagnostics

`postvec doctor --format json --deep` produces a persistent diagnostic
artifact. Each check includes an identifier, status and remediation.
