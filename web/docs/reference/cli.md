---
title: CLI reference
description: postvec setup, doctor, uninstall, model, login — flags and exit codes.
outline: deep
---

# CLI reference

```
postvec [GLOBAL OPTIONS] COMMAND [COMMAND OPTIONS]
```

Package installation does not configure a cluster or database. The `postvec`
CLI performs those changes only when explicitly invoked.

| Goal | Command |
|---|---|
| Configure | `postvec setup` |
| Diagnose (read-only) | `postvec doctor` |
| Remove SQL + config | `postvec uninstall` |
| Embedded models | `postvec model …` |
| Registry identity | `postvec login` / `whoami` / `logout` |

There is no extension-upgrade command. See [upgrade](/docs/install/upgrade).

## Global options

| Option | Use |
|---|---|
| `--cluster 18/main` | `postgresql-common` cluster, including a stopped one |
| `--pg-config PATH` | PGDG-RPM, pgrx, or source install |
| `--config-dir DIR` | With `--pg-config`; must already be included by `postgresql.conf` |
| `--database-url URI` | Prefer `POSTVEC_DATABASE_URL` so credentials stay out of `/proc` |
| `--format json` | Versioned JSON on stdout; progress on stderr |
| `--no-color` | Also honoured via `NO_COLOR` |
| `--timeout 30s` | Network / subprocess bound |

Auto-select works only when exactly one supported online cluster exists.
When zero or multiple candidates exist, the CLI lists them and returns an
error.

`sudo postvec …` retains root for filesystem operations and drops to the
cluster owner for database work. Peer authentication as the invoking non-cluster
role usually fails, as expected.

`--database-url` alone does not authorize local configuration. Pair it with
`--cluster` only when both connections prove they are the same instance.

## `setup`

```bash
sudo postvec setup --database app \
  --embedded --path /opt/postvec/ninference

sudo postvec setup --database app \
  --grpc HOST:PORT --http https://HOST:PORT
```

| Option | Meaning |
|---|---|
| `--database NAME` | Repeatable / comma-separated |
| `--grpc`, `--http` | Remote endpoints. gRPC order is round-robin |
| `--embedded --path DIR` | Absolute engine root |
| `--model NAME` | Embedded preload allow-list; omit to scan-load |
| `--embedded-grpc-listen`, `--embedded-http-listen` | Loopback only |
| `--switch-mode` | Acknowledge remote ↔ embedded; name every database |
| `--allow-unreachable` | Stage config before inference exists |
| `--no-restart` | Write state, exit 4 |
| `--dry-run`, `--yes` | Preview / non-interactive |

Owned file: `conf.d/99-postvec.conf`. Foreign or modified files are
refused; `--yes` does not override.

## `doctor`

```bash
sudo postvec doctor --database app --deep
sudo postvec doctor --format json --strict
```

| Option | Effect |
|---|---|
| `--database NAME` | Default: the configured set |
| `--deep` | Heartbeat must advance; hash CLI model receipts |
| `--strict` | Warnings become failure |
| `--ninference-path DIR` | Diagnose an env-supplied root without writing it |
| `--tls extension-compatible\|strict` | Self-signed tolerance vs a trusted chain |

`doctor` is read-only, does not run inference, and does not call
`refresh_models()`.

## `uninstall`

```bash
sudo postvec uninstall --database app
sudo postvec uninstall --database app \
  --drop-columns --acknowledge-data-loss --yes
```

No `--drop-destinations`. See [uninstall](/docs/install/uninstall).

| Option | Effect |
|---|---|
| `--drop-columns --acknowledge-data-loss` | Postvec-created shadow columns only |
| `--keep-config` | Leave launcher config; required for URI-only |
| `--no-restart`, `--dry-run`, `--yes` | As elsewhere |

## `model`

See [pull / upgrade / rm](/docs/models/pull) and [login](/docs/models/login).

```
model ls [--available] [--path DIR]
model show NAME [--verify] [--path DIR]
model pull NAME... [--path DIR] [--api-key-file FILE]
            [--accept-license ID@VERSION] [--dry-run] [--yes]
model upgrade NAME...|--all   (same credential / path flags)
model rm NAME... [--path DIR] [--force] [--dry-run] [--yes]
model activate [--yes]
```

## Exit codes

| Code | Meaning |
|---:|---|
| 0 | Done; `doctor` clean (`--strict` includes warnings) |
| 1 | Apply / postcondition / diagnostic failure |
| 2 | Invalid invocation or refused prompt |
| 3 | Partial multi-target result, or configuration left unchanged |
| 4 | Valid changes written; restart still required |

Automation should use `--format json`, not parse the human layout.
