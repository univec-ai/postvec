---
title: Install packages
description: Package installation for Debian, Ubuntu and EL9, with copy-paste snippets for PostgreSQL 16, 17 and 18.
---

# Install packages

Use packages on an existing PostgreSQL 16, 17 or 18 host with
pgvector >= 0.8. Superuser and a restart are required. The
[release artifacts](/download) page lists filenames and publication
status.

Install the packages below, then run
[postvec setup](/docs/install/setup) to configure the cluster.

RDS, Aurora, Cloud SQL, Azure, Supabase and Neon use
[managed PostgreSQL](/docs/server/managed).

Debian examples use the `+deb12` filename tag. Ubuntu files use
`+ubuntu22.04` or `+ubuntu24.04` (the rest of the command is identical).

Use `apt` / `dnf` so PostgreSQL, pgvector and ELF dependencies resolve.
The filename tag (`+deb12`, `+ubuntu22.04`, `+ubuntu24.04`, `.el9`)
identifies the **target OS**. Pick the tag that matches the host.

Checksums, Sigstore attestations and file checks:
[verify artifacts](/docs/install/verify).

## Modes

| Mode | Where inference runs | Packages on the database host |
|---|---|---|
| **Embedded** (default) | Inside PostgreSQL | Extension, CLI, ONNX Runtime, MiniLM |
| **Remote** (`grpc`) | [postvec-server](/docs/server/) on this host or another | Extension and CLI only |
| **Managed PostgreSQL** | postvec-server, including the worker | None; see [managed](/docs/server/managed) |

Use [postvec-server](/docs/server/) for process isolation from
PostgreSQL, multi-threaded inference on the same VM, a CPU or GPU fleet
and model management from the dashboard or HTTP API. Managed cloud
databases use it too. [When to use it](/docs/server/usage). SQL is
unchanged.

## 1. Prerequisites (PGDG)

Each release includes `postvec-prerequisites.sh`. It configures the
PGDG repository and its trust root, then confirms that the chosen
PostgreSQL major and pgvector resolve.

<PgSnippet id="prerequisites" />

`--pg` is required (`16`, `17` or `18`). `--print` / `--dry-run` shows
the commands. `--yes` skips the prompt.

The script is idempotent. It adds the PGDG repository and its trust
root, then confirms that the selected PostgreSQL major and pgvector
resolve, so the install lines below can run. On EL9 it also enables
CodeReady Builder and EPEL and disables the distribution's `postgresql`
module, which would otherwise shadow the PGDG packages. On CentOS
Stream 9 or subscribed RHEL 9 it prints the commands and requires
`--force-untested`.

## 2. Local (embedded mode)

Extension, CLI, ONNX Runtime and the bundled MiniLM model.

<PgSnippet id="packages-local" />

Default installation directory is `/opt/postvec`. MiniLM is 384-d.
Embedded mode starts with `postvec setup --embedded`.

## 3. Extension + CLI only (remote mode)

This payload is for remote mode. [postvec-server](/docs/server/)
performs inference on this host or on another machine.

<PgSnippet id="packages-remote" />

Then install postvec-server from the same release (next section) and
[connect PostgreSQL](/docs/server/connect).

## 4. postvec-server

Install postvec-server from the same release: the `postvec-server`
package plus the runtime and model packages, on hosts that need no
PostgreSQL. [Packages](/docs/server/packages) has the copy-paste
commands, TLS and the systemd unit.

postvec-server is **Business Source License 1.1**. Personal production
use, non-production environments and a 30-day production evaluation per
organization are free. Production use by an organization needs
[postvec Pro](https://univec.ai). [License](/docs/license).

Last step is [configuring the cluster](/docs/install/setup).
