---
title: GUCs
description: postvec.* settings, defaults and which ones need a restart.
---

# GUCs

`postvec setup` is the supported way to write these. The table is for
reading what a setting does and whether a restart is required.

Each GUC is read at use time, so a `SIGHUP` takes effect without a worker
restart. POSTMASTER settings exist only when `shared_preload_libraries`
includes `postvec`.

| GUC (`postvec.*`) | Default | Context |
|---|---:|---|
| `ninference_grpc_endpoints` | - | SIGHUP |
| `ninference_http_endpoints` | - | SIGHUP |
| `database` | - | **POSTMASTER** (comma-separated) |
| `worker_enabled` | on | SIGHUP |
| `poll_interval_ms` | 5000 | SIGHUP - empty-queue backstop; writers wake the worker at commit |
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
| `mode` | `embedded` | **POSTMASTER** |
| `ninference_path` | `/opt/postvec/ninference` | **POSTMASTER** (else `$NINFERENCE_PATH`) |
| `embedded_models` | - | **POSTMASTER** (empty = scan-load) |
| `embedded_listen` | `127.0.0.1:33433` | **POSTMASTER** |
| `embedded_http_listen` | `127.0.0.1:33434` | **POSTMASTER** |
| `embedded_max_inflight` | 1 | **POSTMASTER** |
| `providers_path` | `/etc/postvec/providers.d` | **POSTMASTER** - a path, never a credential |

`postvec setup` is the supported way to write these. The CLI merges
`shared_preload_libraries` and owns `99-postvec.conf`.

`pg_reload_conf()` does not apply POSTMASTER settings. A restart is required
after a change to `database`, `mode`, `ninference_path`, `embedded_*`,
`providers_path` or preload.

`providers_path` names the directory of
[external provider](/docs/models/providers) connector files. It holds a path.
The keys stay in the `0600` files under it, and no API key is ever stored in
a GUC, a catalog table or a SQL argument. An absent directory means no
provider-backed models, which is the default.

Add databases through `setup`. A name that does not exist makes the worker
fail and respawn about every 15 seconds.
