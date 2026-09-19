---
layout: page
sidebar: false
title: Downloads
description: PostgreSQL images, .deb and .rpm packages and postvec-server, with checksums and Sigstore attestations.
---

<div class="dl-page vp-doc">

<header class="dl-intro">
<p class="dl-eyebrow">Downloads</p>

# Choose your postvec deployment

Use a PostgreSQL image for a new instance, packages for an existing cluster or postvec-server for remote inference and managed databases.

<div class="dl-platforms" aria-label="Supported platforms">
<span>PostgreSQL 16 / 17 / 18</span>
<span>Linux amd64 / arm64</span>
<span>Docker / Debian / RPM</span>
</div>
</header>

<DownloadHero />

## Choose a build

Select your distribution, PostgreSQL version and architecture. Available
release files link to GitHub; the selector shows publication status alongside
the package names. Verify downloaded files with the release checksums and
Sigstore attestations.

| Channel | What it holds |
|---|---|
| [GitHub Releases](https://github.com/univec-ai/postvec/releases) tagged `postvec-v*` | `.deb` / `.rpm` packages (including `postvec-server`), `SHA256SUMS`, Sigstore attestations, `postvec-prerequisites.sh` |
| [GHCR](https://github.com/univec-ai/postvec/pkgs/container/postvec) `ghcr.io/univec-ai/postvec` | PostgreSQL images (`-pgNN-local` / `-pgNN-remote`) |
| [GHCR](https://github.com/univec-ai/postvec/pkgs/container/postvec-server) `ghcr.io/univec-ai/postvec-server` | postvec-server (versioned tags, moving tag `latest`) |

<DownloadPanel />

## Artifact contents

| Artifact | Contents |
|---|---|
| `postvec-cli` | `/usr/bin/postvec` |
| `postgresql-NN-postvec` / `postgresqlNN-postvec` | Extension library, control file, versioned SQL |
| `postvec-onnxruntime` | CPU ONNX Runtime under `/opt/postvec/libs`; independently versioned |
| `postvec-model-minilm-l6-v2` | Bundled 384-d model; independently versioned (`2.1.0` = registry revision 2, bundle 1) |
| `postvec-extras` | Metapackage pinning ONNX Runtime and the bundled model. Install it with the extension and CLI. |
| `postvec-server` | `/usr/bin/postvec-server`, its systemd unit and configuration; one per distribution and architecture, no PostgreSQL major |
| `...-pgNN-remote` image | PostgreSQL + pgvector + postvec + CLI, remote mode (points at postvec-server) |
| `...-pgNN-local` image | The above plus ONNX Runtime and MiniLM (both modes) |
| `postvec-server` image | postvec-server, the CLI, ONNX Runtime and MiniLM, from the packages above |

The filename tag (`+deb12`, `+ubuntu22.04`, `+ubuntu24.04`, `.el9`)
identifies the target OS. Pick the tag that matches the host.
The `postvec-server` image holds the inference node:
[postvec-server](/docs/server/) covers what it serves and how a cluster
reaches it.

## postvec-server

[postvec-server](/server) ships with every release, as a package
and as an image, built from the same commit as the extension and tested
against it: the release's remote-mode images are smoke-tested through
this exact server image, and the clean-host install test starts the
packaged server against the packaged model on every distribution.

The same binary includes [managed PostgreSQL](/docs/server/managed):
schema install, the sync worker and the optional `search(text)` proxy
for RDS, Aurora, Cloud SQL, Azure, Supabase and Neon.

**License.** `postvec-server` is **Business Source License 1.1** (`BUSL-1.1`,
source-available; production use by an organization needs
[postvec pro](/server#plans)).
Terms: [License](/docs/license). That identifier is in the release
manifest's `licenses` block, the package copyright file and the image's
`org.opencontainers.image.licenses` label. The extension and CLI use the
PostgreSQL License in either inference mode. Bundled models and third-party
runtimes retain their own licenses.

Every debug package has a `postvec-server-dbgsym` / `-debuginfo` sibling.
[Packages](/docs/server/packages) covers installation; the selector
above lists the file for the chosen distribution and architecture.

[Configure the cluster](/docs/install/setup).
[Docker](/docs/install/docker).
[Uninstall](/docs/install/uninstall).

</div>
