---
title: CLI reference
description: postvec setup, doctor, uninstall, model, provider and login commands. Flags and exit codes.
outline: deep
---

# CLI reference

```
postvec [GLOBAL OPTIONS] COMMAND [COMMAND OPTIONS]
```

A package install places files on disk. Cluster and database configuration
happen through the `postvec` CLI. [postvec-server](/docs/server/) is a
separate binary; see [postvec-server reference](/docs/server/reference)
and [dashboard](/docs/server/dashboard).

| Goal | Command |
|---|---|
| Configure | `postvec setup` |
| Diagnose (read-only) | `postvec doctor` |
| Remove SQL + config | `postvec uninstall` |
| Embedded models | `postvec model ...` |
| External providers | `postvec provider ...` |
| Registry identity | `postvec login` / `whoami` / `logout` |

Extension upgrades run through the package manager and `ALTER EXTENSION`: see
[upgrade](/docs/install/upgrade).

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

`sudo` is required for host writes and for a peer-authenticated
database session. Read-only model commands need neither:

| Need | Commands |
|---|---|
| Root (write `/etc` or `/opt/postvec`) | `setup`, `uninstall`, `model pull` / `upgrade` / `rm` / `activate` / `deactivate`, `provider add` / `rm` |
| Cluster owner (`postgres`) | `doctor`, `setup` / `uninstall`, cluster-targeted `model pull` / `upgrade` / `rm` / `activate` / `deactivate` and cluster-targeted `provider add` / `rm` / `test` |
| Neither | `login` / `logout` / `whoami`, `model ls`, `model ls --available`, `model show`, `provider ls --available` |

`sudo postvec ...` covers the first two at once: the parent keeps root
for the filesystem and a child drops to the cluster owner for database
work. `model ls` and `model show` fall back to the owned
`99-postvec.conf` snippet if that login fails. Cluster-targeted
`model pull` / `upgrade` / `rm` / `activate` / `deactivate` refuse that
fallback: without a login
they would write files as you and then be unable to refresh
`postvec.models`. Use `--path DIR` for files only.

`--database-url` selects a session. Local configuration writes still need
`--cluster` after both connections prove they are the same instance.

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
| `--path DIR` | With `--embedded`: absolute engine root. Defaults to `/opt/postvec` |
| `--model NAME` | With `--embedded`: preload allow-list; omit to load every enabled model |
| `--providers-path DIR` | With `--embedded`: move the [provider](/docs/models/providers) connector directory. Omit to keep `/etc/postvec/providers.d` |
| `--embedded-grpc-listen`, `--embedded-http-listen` | With `--embedded`: loopback listeners, `127.0.0.1:33433` and `127.0.0.1:33434` |
| `--switch-mode` | Acknowledge remote <-> embedded; name every database |
| `--allow-unreachable` | Stage config before inference exists |
| `--no-restart` | Write state, exit 4 |
| `--dry-run`, `--yes` | Preview / non-interactive |

`--path`, `--model`, `--providers-path` and both listener flags require
`--embedded`. Without it they are a usage error (exit 2).

Owned file: `conf.d/99-postvec.conf`. Foreign or modified files are
refused, including with `--yes`.

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
| `--path DIR` | Diagnose an engine root the cluster settings do not reveal, without writing it |
| `--tls extension-compatible\|strict` | Self-signed tolerance vs a trusted chain |

`doctor` is read-only: host files, cluster settings and the heartbeat.

## `uninstall`

```bash
sudo postvec uninstall --database app
sudo postvec uninstall --database app \
  --drop-columns --acknowledge-data-loss --yes
sudo postvec uninstall --all \
  --drop-columns --acknowledge-data-loss --purge --yes
```

| Option | Effect |
|---|---|
| `--database NAME` | One database; repeatable |
| `--all` | Every configured database plus any other database with the extension installed |
| `--drop-columns --acknowledge-data-loss` | Postvec-created shadow columns and, for chunked entries, the managed chunk destination tables and views |
| `--keep-destinations` | With `--drop-columns`: keep chunk destinations as ordinary frozen tables |
| `--purge` | **Experimental**. With `--all`: delete postvec's own files on this host, between a cluster stop and start |
| `--keep-config` | Leave launcher config; required for URI-only |
| `--no-restart`, `--dry-run`, `--yes` | As elsewhere |

`--database` and `--all` are exclusive. `--purge` requires `--all` and
refuses `--keep-config` and `--no-restart`. See
[uninstall](/docs/install/uninstall).

## `model`

See [pull / upgrade / rm](/docs/models/pull) and [login](/docs/models/login).

```
model ls [--path DIR]
model ls --available [--api-key-file FILE]
model show NAME [--verify] [--path DIR]
model pull NAME... [--path DIR] [--api-key-file FILE]
            [--accept-license ID@VERSION] [--dry-run] [--yes]
model upgrade NAME...|--all   (same credential / path flags)
model rm NAME... [--path DIR] [--force] [--acknowledge-in-use]
            [--dry-run] [--yes]
model activate [NAME...|--all] [--path DIR] [--dry-run] [--yes]
model deactivate NAME... [--path DIR] [--force] [--acknowledge-in-use]
            [--dry-run] [--yes]
model prefer SPACE ROUTE... [--default] [--path DIR] [--acknowledge-in-use]
            [--dry-run] [--yes]
model set-space ROUTE SPACE [--path DIR] [--acknowledge-in-use] [--dry-run] [--yes]
```

`--path` and `--available` are exclusive; `--api-key-file` requires
`--available`. `POSTVEC_PATH` has the same meaning as `--path`. Container
images set it, so `docker exec <ctr> postvec model ...` needs no extra
flags.

`pull` installs **deactivated**; `activate` / `deactivate` are the only verbs
that change serving state, and both persist across a PostgreSQL restart.
`--acknowledge-in-use` is required (with `--yes`) when managed columns would
lose their embedding route, directly or through a local `embed-bridge` route
the model is part of; `--yes` and `--force` never stand in for it. `prefer`
and `set-space` ask for the same flag when columns change where their text
is embedded: `prefer` lists provider routes only (a local model is always
priority 100), warns about and skips a route no file declares, and with
`--default` drops every explicit priority in the space; `set-space` refuses
a space served at another dimension and refuses local routes. `--default`
also requires acknowledgement when the reset can change a column's provider.
No-op priority changes do not reload the host. Multi-file edits check each
file against its inspected version and restore earlier writes if a later
write fails.

Doctor reports columns with no available embedding route, incompatible
route dimensions, and bindings using their remembered-space fallback.
Incomplete discovery produces a warning rather than a definitive missing-route
failure; local embedding bridges are included in this check.

## `provider`

See [External providers](/docs/models/providers).

```
provider add TYPE --model ID... [--name STEM] [--path DIR]
            [--api-key-file FILE | --api-key-env VAR | --key-stdin]
            [--base-url URL] [--region REGION] [--dim N] [--space SPACE]
            [--prefer] [--no-verify]
            [--acknowledge-in-use] [--dry-run] [--yes]
provider add univec [--model ID...] [--convert SRC:DST...] [--convert-to MODEL...]
            [--convert-from MODEL...] [--all-converters] [--converter-name NAME]
            [--api-key-from-login] [--no-catalog] [common add options]
provider ls [--path DIR]
provider ls --available [PROVIDER] [--kind embed|convert] [--to MODEL]
            [--from MODEL] [--path DIR]
provider test NAME [--model ID] [--path DIR]
provider rm NAME [--model ID] [--path DIR] [--acknowledge-in-use]
            [--dry-run] [--yes]
```

`TYPE` is `openai`, `openrouter`, `mistral`, `google`, `cohere`, `aws` or
`univec`. `gemini` and `amazon` are accepted aliases; the file records the
canonical name. `--name STEM` writes `STEM.toml` instead of the type's name:
two OpenAI-compatible endpoints, two files.

A key is never a command-line value. Without `--api-key-file`,
`--api-key-env` or `--key-stdin`, an interactive run prompts without echo and
a non-interactive one is a usage error. `--api-key-env` records the variable
name; the **inference process** resolves it, so the variable belongs to the
postmaster or the `postvec-server` unit, not to your shell.

`add` and `test` make one live request per selected entry. Embed entries send
one text input. UniVec converter entries send one source vector and check the
target dimension. `--no-verify` skips the `add` probe. It then requires
`--dim` for an unknown embed model or converter target. AWS SigV4 files cannot
be probed from the CLI; use `--no-verify` and confirm with `provider ls` plus
a first write. The plan is shown before the probe, so declining it costs no
API call.

`univec` discovers models from UniVec's public catalogue. With no selector
it adds every embed model; `--convert SRC:DST`, `--convert-to`,
`--convert-from` and `--all-converters` add converters, and they combine.
Dimensions come from the catalogue, so `--no-verify` needs no `--dim` for
listed models; the probe proves the key and one added route per kind. A
selection over the 256-entry per-file ceiling is refused whole.
`--api-key-from-login` copies the `postvec login` key into
`<root>/keys/<NAME>.key` (one per connector file; never over a different
existing key. `--replace-copied-key` rotates one this command copied for
that connector). The plan carries a `verify against univec with N billable
embed probe attempt(s) and M billable convert probe attempt(s)` step (JSON
`kind: "verify-providers"`, fields `embed_probes` and `convert_probes`)
before confirmation. Attempts, not charges: UniVec debits only a successful
inference. A key rotation on `univec` re-verifies one existing entry per
kind; on every other connector it re-verifies every existing route. The listing document reports a configured file that cannot
be read as `failed` with a `file` field; an explicit `--path`,
`--database-url`, `--cluster` or `POSTVEC_PROVIDERS_PATH` that cannot be
honoured is an error, while an implicit cluster that is not there lists the
catalogue with `configured_state_unavailable` set. `--no-catalog` skips discovery. The manual
converter flags (`--convert-source`, `--convert-target`, `--source-model`,
`--target-model`, `--source-dim`) remain for an unlisted pair. See
[UniVec hosted models](/docs/models/univec).

`provider ls --available` lists what a provider offers. With no PROVIDER it
asks every configured file's connector plus `univec`; PROVIDER is a file
stem or connector type. It needs no cluster. The JSON document has
`available: true`, one `providers[]` entry per file with `outcome`
(`entries`, `needs_key`, `unsupported` or `failed`), `reason` when not
`entries`, `models[]` carrying `provider_model_id`, `kind`, `dim`,
`source`, `sequence_len`, `quality`, `configured` and `configured_name`,
and `errors[]`. A configured file that cannot be read or parsed is a
`failed` entry too. Any `failed` entry makes the exit code 1 after every
provider is shown; there is never a second error document.

`--path DIR` manages `DIR/providers.d` as files, with no cluster in scope.
`DIR` must already exist, and new files inherit its owner. That is how a
`postvec-server` node is administered, and it is the only form accepted on
a remote-mode cluster; without it, such a cluster is refused with a message
naming the server root. `POSTVEC_PROVIDERS_PATH` has the same meaning as
`--path`. The postvec-server image sets it to the server root. An embed change through `--path` requires
`--acknowledge-in-use` because there is no cluster to scan. Adding a converter
leaves source text on the host, so that acknowledgement is skipped. Removing
one can break an active migration, so `provider rm` requires it.

`--acknowledge-in-use` is required (with `--yes`) when the change affects
existing columns: on `add`, columns that will start sending source text to
the provider; on `rm`, columns that lose their embedding route, or that
start sending text to a surviving claimant of a contested name. `--yes`
never stands in for it.

When no host answers a reload on a cluster target, the result is partial
(exit 3): the files are correct and a restart applies them. On a `--path`
target the same situation is a note.

Format, loading rules and `doctor` checks:
[connector files](/docs/models/providers-file).

## Exit codes

| Code | Meaning |
|---:|---|
| 0 | Done; `doctor` clean (`--strict` includes warnings) |
| 1 | Apply / postcondition / diagnostic failure |
| 2 | Invalid invocation or refused prompt |
| 3 | Partial result: a retained chunk destination, an uninspectable database under `--all`, or configuration left unchanged |
| 4 | Valid changes written; restart still required |

Automation should use `--format json`, not parse the human layout.
