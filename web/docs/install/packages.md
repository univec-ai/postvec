---
title: Install packages
description: Package installation for Debian, Ubuntu and EL9, with copy-paste snippets for PostgreSQL 16, 17 and 18.
---

# Install packages

Use packages on an existing PostgreSQL host. Dedicated releases exist per distribution, architecture and PostgreSQL major. Requires PostgreSQL 16, 17 or 18, pgvector >= 0.8,
superuser and a restart. The [release artifacts](/download) page lists
names and publication status.

#### Operation modes:
- **embedded** mode (default) runs inference entirely inside PostgreSQL
- **remote** mode (grpc) with inference handled by [postvec-server](/docs/server/), an optional companion inference server working in tandem with postvec-enabled databases
- [managed PostgreSQL](/docs/server/managed)

#### Installation stages:

- Local install - the packages below
- Cluster configuration - [postvec setup](/docs/install/setup)

RDS, Aurora, Cloud SQL, Azure, Supabase and Neon use [managed PostgreSQL](/docs/server/managed).

Debian examples use the `+deb12` filename tag. Ubuntu files use
`+ubuntu22.04` or `+ubuntu24.04` (the rest of the command is identical)

Checksums, Sigstore attestations and file checks:
[verify artifacts](/docs/install/verify).

## 1. Prerequisites (PGDG)

Each release includes `postvec-prerequisites.sh`. It adds the PGDG
archive so the chosen PostgreSQL major and pgvector are available.

<PgSnippet id="prerequisites" />

`--pg` is required (`16`, `17` or `18`). `--print` / `--dry-run` shows
the commands. `--yes` skips the prompt.

The script is idempotent. It installs PostgreSQL and pgvector for the
chosen major. postvec packages will be installed in the next step. On CentOS Stream
9 or subscribed RHEL 9 it prints the commands and requires
`--force-untested`.

## 2. Local (embedded mode)

Extension, CLI, ONNX Runtime and the bundled MiniLM model.

<PgSnippet id="packages-local" />

Default installation directory is `/opt/postvec`. MiniLM is 384-d.
Embedded mode starts with `postvec setup --embedded`.

## 3. Extension + CLI only (remote mode)

This payload is for remote mode, where [postvec-server](/docs/server/)
performs inference on this host or on another machine:

:::: info Use remote mode and/or postvec-server when you need to
- better insulate the postgres service from any in-process failure: postvec has a great number of guardrails to preserve PostgreSQL stability however compliance may dictate this in mission-critical scenarios
- achieve higher throughput and optimize resource contention: postvec-server can run multi-threaded alongside PostgreSQL on the same VM or offload models execution to a mixed fleet of CPU or GPU nodes within the network for distributed inference
- run against managed PostgreSQL instances (Amazon RDS, Aurora, Cloud SQL, Azure Flexible Server, Supabase, Neon and similar hosts)
- perform model management remotely (UI or API calls)
::::

<PgSnippet id="packages-remote" />

## 4. postvec-server (optional)

Install postvec-server from the same release: the `postvec-server`
package plus the runtime and model packages, on hosts that need no
PostgreSQL. 

:::: info postvec-server licensing

postvec-server is released under **Business Source License 1.1**
Details on the [release page](/download#postvec-server)

::::

[Download packages](/docs/server/packages)


Use `apt` / `dnf` so PostgreSQL, pgvector and ELF dependencies
resolve.

The filename tag (`+deb12`, `+ubuntu22.04`, `+ubuntu24.04`, `.el9`)
identifies the **target OS**. Pick the tag that matches the host.

Last step is [configuring the cluster](/docs/install/setup).
