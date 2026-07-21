---
title: Downloads
description: GitHub Release packages, GHCR images, publication status and verification.
outline: deep
---

# Downloads

Packages and images for identity `0.1.0-1`. Until a tag is published,
the names below are the local-build contract.

Publication channels:

| Channel | What it holds |
|---|---|
| [GitHub Releases](https://github.com/univec-ai/postvec/releases) tagged `postvec-v*` | `.deb` / `.rpm` packages (including `postvec-server`), `SHA256SUMS`, Sigstore attestations, `postvec-prerequisites.sh` |
| [GHCR](https://github.com/univec-ai/postvec/pkgs/container/postvec) `ghcr.io/univec-ai/postvec` | Drop-in PostgreSQL images (`-pgNN-local` / `-pgNN-remote`) |
| [GHCR](https://github.com/univec-ai/postvec/pkgs/container/postvec-server) `ghcr.io/univec-ai/postvec-server` | The remote inference node (`0.1.0-1`, moving tag `latest`) |

The selector reads `univec-ai/postvec` releases tagged `postvec-v*` and
links assets only when they exist. Install from the downloaded files.
Checksums and Sigstore attestations are optional:
[verify artifacts](/docs/install/verify). A signed apt/yum repository is
planned later.

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
| `...-pgNN-remote` image | PostgreSQL + pgvector + postvec + CLI, remote mode |
| `...-pgNN-local` image | The above plus ONNX Runtime and MiniLM (both modes) |
| `postvec-server` image | The node, the CLI, ONNX Runtime and MiniLM, from the packages above |

The filename tag (`+deb12`, `+ubuntu22.04`, `+ubuntu24.04`, `.el9`)
identifies the target OS. Pick the tag that matches the host.

## postvec-server

The [remote inference node](/docs/server/) ships with every release, as a
package and as an image, built from the same commit as the extension and
tested against it: the release's remote-mode images are smoke-tested through
this exact node image, and the clean-host install test starts the packaged
node against the packaged model on every distribution.

**Licence.** `postvec-server` is **Business Source License 1.1** (`BUSL-1.1`,
source-available; production use by an organization needs a commercial license). That identifier is in
the release manifest's `licenses` block, the package copyright file and
the image's `org.opencontainers.image.licenses` label. The extension,
the CLI, the runtime and model packages and the PostgreSQL images stay
under the PostgreSQL License in either inference mode.

Every debug package has a `postvec-server-dbgsym` / `-debuginfo` sibling.
[Run a node](/docs/server/node) covers installation; the selector above lists
the file for the chosen distribution and architecture.

[Configure the cluster](/docs/install/setup).
[Docker](/docs/install/docker).
[Uninstall](/docs/install/uninstall).
