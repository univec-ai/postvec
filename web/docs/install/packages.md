---
title: Install packages
description: Package installation for Debian, Ubuntu and EL9, with copy-paste snippets for PostgreSQL 16, 17 and 18.
---

# Install packages

Use packages when the host will keep postvec installed. Each install
matches **one** release, **one** distribution, **one** architecture and
**one** PostgreSQL major. The [release artifacts](/download) page lists
names and publication status.

The packages place files. They do not configure the cluster. After
step 5, run [setup](/docs/install/setup).

Tabs pick the PostgreSQL major and the package family. Debian examples
use the `+deb12` filename tag. Ubuntu files use `+ubuntu22.04` or
`+ubuntu24.04` instead; the rest of the command is the same.

## 1. Prerequisites (PGDG)

Supported distros do not ship every PostgreSQL-major x pgvector pair in
their default archives. Each release includes `postvec-prerequisites.sh`.

<PgSnippet id="prerequisites" />

`--pg` is required (`16`, `17` or `18`). `--print` / `--dry-run` shows
the commands. `--yes` skips the prompt.

The script is idempotent. It installs **no** postvec package, edits no
cluster and restarts nothing. On CentOS Stream 9 or subscribed RHEL 9 it
prints the commands and requires `--force-untested`.

## 2. Embedded (recommended)

Extension, CLI, ONNX Runtime and the bundled MiniLM model. On-prem
inference; `setup --embedded` afterwards.

<PgSnippet id="packages-complete" />

Files land under `/opt/postvec/ninference`. MiniLM is 384-d and needs no
API key. Installing this payload leaves embedded mode off.
`postvec setup --embedded` turns it on.

## 3. Extension + CLI only

This payload is for remote mode, where `postvec-server` nodes perform
inference:

<PgSnippet id="packages-remote" />

Use `apt` / `dnf`, not `dpkg` / `rpm -i`, so PostgreSQL, pgvector and
ELF dependencies resolve.

The filename tag (`+deb12`, `+ubuntu22.04`, `+ubuntu24.04`, `.el9`)
identifies the **target OS**. Debian 12 and Ubuntu 22.04 packages are
not interchangeable.

## 4. Verify the download

<PgSnippet id="packages-verify-download" />

`--ignore-missing` supports partial downloads and should be omitted when
the complete release is present.

## 5. Verify installed files

<PgSnippet id="packages-verify-files" />

:::: tip Expected
These checks succeed on the files alone. Worker and database
configuration happens during
[cluster configuration](/docs/install/setup).
::::

## After the packages are installed

The packages place files. `postvec setup` writes cluster configuration,
creates the extension and starts the worker. `apt remove` / `dnf remove`
remove those files and leave user data in place.

## Cluster configuration

[Configure the cluster](/docs/install/setup)
