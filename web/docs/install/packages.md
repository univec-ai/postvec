---
title: Install packages
description: Package installation for Debian, Ubuntu and EL9, with copy-paste snippets for PostgreSQL 16, 17 and 18.
---

# Install packages

Use packages when the host will keep postvec installed. Each install
matches **one** release, **one** distribution, **one** architecture and
**one** PostgreSQL major. The [release artifacts](/download) page lists
names and publication status.

Packages install files. Then [configure the cluster](/docs/install/setup).

Debian examples use the `+deb12` filename tag. Ubuntu files use
`+ubuntu22.04` or `+ubuntu24.04` instead; the rest of the command is
the same.

Checksums, Sigstore attestations and file checks:
[verify artifacts](/docs/install/verify).

## 1. Prerequisites (PGDG)

Each release includes `postvec-prerequisites.sh`. It adds the PGDG
archive so the chosen PostgreSQL major and pgvector are available.

<PgSnippet id="prerequisites" />

`--pg` is required (`16`, `17` or `18`). `--print` / `--dry-run` shows
the commands. `--yes` skips the prompt.

The script is idempotent. It installs PostgreSQL and pgvector for the
chosen major. postvec packages come in the next step. On CentOS Stream
9 or subscribed RHEL 9 it prints the commands and requires
`--force-untested`.

## 2. Local (embedded mode)

Extension, CLI, ONNX Runtime and the bundled MiniLM model.

<PgSnippet id="packages-local" />

Default installation directory is `/opt/postvec`. MiniLM is 384-d.
Embedded mode starts with `postvec setup --embedded`.

## 3. Extension + CLI only

This payload is for remote mode, where `postvec-server` nodes perform
inference:

<PgSnippet id="packages-remote" />

The nodes install from the same release: the `postvec-server` package plus
the runtime and model packages, on hosts that need no PostgreSQL. See
[run a node](/docs/server/node). That package is **Business Source License 1.1**
(source-available; production use by an organization needs a commercial
license); the
identifier is on the [release page](/download#postvec-server).

Use `apt` / `dnf` so PostgreSQL, pgvector and ELF dependencies
resolve. `dpkg` / `rpm -i` skip that resolution.

The filename tag (`+deb12`, `+ubuntu22.04`, `+ubuntu24.04`, `.el9`)
identifies the **target OS**. Pick the tag that matches the host.

[Configure the cluster](/docs/install/setup). `apt remove` / `dnf remove`
remove the files. User data stays in the database.
