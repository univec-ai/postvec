---
title: GUCs
description: postvec.* settings, defaults and which ones need a restart.
---

# GUCs

`postvec setup` is the supported way to write these. The table is for
reading what a setting does and whether a restart is required.

Each GUC is read at use time, so a `SIGHUP` takes effect without a worker
restart. The settings a worker reads at start (`database`, `mode`, `path`,
`embedded_*`, `providers_path`) are SIGHUP: set them with `ALTER SYSTEM` and
`pg_reload_conf()`, and they apply to the next worker start. With
`shared_preload_libraries`, that means the next server restart; a worker
started with `start_worker()` picks them up when it is started.

| GUC (`postvec.*`) | Default | Context |
|---|---:|---|
| `grpc_endpoints` | - | SIGHUP |
| `http_endpoints` | - | SIGHUP |
| `database` | - | SIGHUP (comma-separated) |
| `worker_enabled` | on | SIGHUP |
| `poll_interval_ms` | 5000 | SIGHUP - idle poll; writers wake the worker at commit |
| `batch_size` | 64 | SIGHUP (×4 = cursor chunk) |
| `migrate_batch_size` | 256 | SIGHUP |
| `embed_timeout_ms` | 30000 | SIGHUP |
| `query_timeout_ms` | 2000 | USERSET |
| `max_retries` | 5 | SIGHUP |
| `retry_backoff_ms` | 5000 | SIGHUP |
| `job_visibility_timeout_ms` | 300000 | SIGHUP |
| `model_refresh_interval_ms` | 60000 | SIGHUP |
| `discovery_timeout_ms` | 5000 | SIGHUP |
| `search_degrade_to_fts` | on | USERSET |
| `notify_on_write` | off | SIGHUP |
| `worker_lock_timeout_ms` | 10000 | SIGHUP (`0` disables) |
| `max_document_bytes` | 1 MiB | SIGHUP - oversized rows dead-letter; never truncated |
| `max_batch_total_bytes` | 16 MiB | SIGHUP - remaining rows stay pending |
| `ddl_lock_timeout_ms` | 60000 | USERSET - applied `SET LOCAL` in lifecycle verbs |
| `heartbeat_interval_ms` | 30000 | SIGHUP - idle workers write no WAL between beats |
| `mode` | `embedded` | SIGHUP |
| `path` | `/opt/postvec` | SIGHUP |
| `embedded_models` | - | SIGHUP (empty = load every enabled model) |
| `embedded_listen` | `127.0.0.1:33433` | SIGHUP |
| `embedded_http_listen` | `127.0.0.1:33434` | SIGHUP |
| `embedded_max_inflight` | 1 | SIGHUP |
| `providers_path` | `/etc/postvec/providers.d` | SIGHUP - a path, never a credential |

`path` is the engine root (`libs/`, `models/`). Package payloads install
there. CLI `--path` and `POSTVEC_PATH` name the same directory. The CLI
merges `shared_preload_libraries` and owns `99-postvec.conf`.

Only `shared_preload_libraries` needs a restart. `database`, `mode`, `path`,
`embedded_*` and `providers_path` are read when a worker starts, so with a
preloaded launcher they too take effect at the next restart. `pg_reload_conf()`
covers every other setting.

`providers_path` names the directory of
[external provider](/docs/models/providers) connector files. It holds a path.
The keys stay in the `0600` files under it, and no API key is ever stored in
a GUC, a catalog table or a SQL argument. An absent directory means no
provider-backed models, which is the default.

Add databases through `setup`. A missing name makes the worker fail and
respawn about every 15 seconds.
