---
title: CLI reference
description: postvec setup, doctor, uninstall, model and login. Flags and exit codes.
outline: deep
---

# CLI reference

```
postvec [GLOBAL OPTIONS] COMMAND [COMMAND OPTIONS]
```

A package install places files on disk. Cluster and database configuration
happen only when the `postvec` CLI is invoked.

| Goal | Command |
|---|---|
| Configure | `postvec setup` |
| Diagnose (read-only) | `postvec doctor` |
| Remove SQL + config | `postvec uninstall` |
| Embedded models | `postvec model ...` |
| External providers | `postvec provider ...` |
| Registry identity | `postvec login` / `whoami` / `logout` |

The CLI has no extension-upgrade command. See [upgrade](/docs/install/upgrade).

## Global options

| Option | Use |
|---|---|
| `--cluster 18/main` | `postgresql-common` cluster, including a stopped one |
| `--pg-config PATH` | PGDG-RPM, pgrx or source install |
| `--config-dir DIR` | With `--pg-config`; must already be included by `postgresql.conf` |
| `--database-url URI` | `POSTVEC_DATABASE_URL` keeps credentials out of `/proc` |
| `--format json` | Versioned JSON on stdout; progress on stderr |
| `--no-color` | Also honoured via `NO_COLOR` |
| `--timeout 30s` | Network / subprocess bound |

Auto-select runs only when exactly one supported online cluster is present.
Zero or several candidates produce a listing and an error.

`sudo` is only required for host writes and for a peer-authenticated
database session. Read-only model commands do not need it:

| Need | Commands |
|---|---|
| Root (write `/etc` or `/opt/postvec`) | `setup`, `uninstall`, `model pull` / `upgrade` / `rm`, `provider add` / `rm` |
| Cluster owner (`postgres`) | `doctor`, `setup` / `uninstall`, cluster-targeted `model pull` / `upgrade` / `rm` / `activate` / `deactivate` and cluster-targeted `provider add` / `rm` |
| Neither | `login` / `logout` / `whoami`, `model ls`, `model ls --available`, `model show` |

`sudo postvec ...` covers the first two at once: the parent keeps root
for the filesystem and a child drops to the cluster owner for database
work. `model ls` and `model show` fall back to the owned
`99-postvec.conf` snippet if that login fails. Cluster-targeted
`model pull` / `upgrade` / `rm` / `activate` / `deactivate` refuse that
fallback: without a login
they would write files as you and then be unable to refresh
`postvec.models`. Use `--path DIR` for files only.

`--database-url` alone does not authorize local configuration writes. Add
`--cluster` only after both connections prove they are the same
instance.

## `setup`

```bash
sudo postvec setup --database app \
  --embedded

sudo postvec setup --database app \
  --grpc HOST:PORT --http https://HOST:PORT
```

| Option | Meaning |
|---|---|
| `--database NAME` | Repeatable / comma-separated |
| `--grpc`, `--http` | Remote endpoints. gRPC order is round-robin |
| `--embedded` | In-process inference |
| `--path DIR` | Absolute engine root. Defaults to `/opt/postvec/ninference` |
| `--model NAME` | Embedded preload allow-list; omit to scan-load |
| `--providers-path DIR` | Move the [provider](/docs/models/providers) connector directory. Omit to keep `/etc/postvec/providers.d` |
| `--embedded-grpc-listen`, `--embedded-http-listen` | Loopback only |
| `--switch-mode` | Acknowledge remote <-> embedded; name every database |
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

`doctor` is read-only. It does not run inference and does not call
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
model rm NAME... [--path DIR] [--force] [--acknowledge-in-use]
            [--dry-run] [--yes]
model activate [NAME...|--all] [--path DIR] [--dry-run] [--yes]
model deactivate NAME... [--path DIR] [--force] [--acknowledge-in-use]
            [--dry-run] [--yes]
```

`pull` installs **deactivated**; `activate` / `deactivate` are the only verbs
that change serving state, and both persist across a PostgreSQL restart.
`--acknowledge-in-use` is required (with `--yes`) when managed columns would
lose their embedding route, directly or through a converter/bridge chain the
model is part of; `--yes` and `--force` never stand in for it.

## `provider`

See [External providers](/docs/models/providers).

```
provider add TYPE --model ID... [--name STEM] [--path DIR]
            [--api-key-file FILE | --api-key-env VAR | --key-stdin]
            [--base-url URL] [--region REGION] [--dim N] [--no-verify]
            [--acknowledge-in-use] [--dry-run] [--yes]
provider ls [--path DIR]
provider test NAME [--model ID] [--path DIR]
provider rm NAME [--model ID] [--path DIR] [--acknowledge-in-use]
            [--dry-run] [--yes]
```

`TYPE` is `openai`, `openrouter`, `mistral`, `google`, `cohere` or `aws`.
`gemini` and `amazon` are accepted aliases; the file records the canonical
name.

A key is never a command-line value. Without `--api-key-file`,
`--api-key-env` or `--key-stdin`, an interactive run prompts without echo and
a non-interactive one is a usage error. `--api-key-env` records the variable
name; the **inference process** resolves it, so the variable belongs to the
postmaster or the `postvec-server` unit, not to your shell.

`add` and `test` each make one live embed per model, which the provider
bills. `--no-verify` skips it, and then `--dim` is required for a model the
built-in catalogue does not know.

`--path DIR` manages `DIR/providers.d` as files, with no cluster in scope.
That is how a `postvec-server` node is administered, and it is the only form
accepted on a remote-mode cluster; without it, such a cluster is refused with
a message naming the server root.

`--acknowledge-in-use` is required (with `--yes`) when the change affects
existing columns: on `add`, columns that will start sending source text to
the provider; on `rm`, columns that lose their embedding route. `--yes` never
stands in for it.

## Exit codes

| Code | Meaning |
|---:|---|
| 0 | Done; `doctor` clean (`--strict` includes warnings) |
| 1 | Apply / postcondition / diagnostic failure |
| 2 | Invalid invocation or refused prompt |
| 3 | Partial multi-target result, or configuration left unchanged |
| 4 | Valid changes written; restart still required |

Automation should use `--format json`, not parse the human layout.
