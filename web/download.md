---
title: Downloads
description: GitHub Release packages, GHCR images, publication status and verification.
outline: deep
---

# Downloads

Packages and images for a given identity. Until a tag is published, the
names below are the contract for a local build.

Publication channels:

| Channel | What it holds |
|---|---|
| [GitHub Releases](https://github.com/univec-ai/stack/releases) tagged `postvec-v*` | `.deb` / `.rpm` packages, `SHA256SUMS`, Sigstore attestations, `postvec-prerequisites.sh` |
| [GHCR](https://github.com/univec-ai/stack/pkgs/container/postvec) `ghcr.io/univec-ai/postvec` | Drop-in PostgreSQL images (`-pg16` / `-pg17` / `-pg18`, with or without `-complete`) |

The selector checks GitHub for a published postvec release and links
assets only when they exist. Until then the names below are the contract
for a local build.

Install from the downloaded files. A signed apt/yum repository is
planned later. The current release identity is `0.1.0-1`.

<DownloadPanel />

## Artifact contents

| Artifact | Contents |
|---|---|
| `postvec-cli` | `/usr/bin/postvec` |
| `postgresql-NN-postvec` / `postgresqlNN-postvec` | Extension library, control file, versioned SQL |
| `postvec-onnxruntime` | CPU ONNX Runtime under `/opt/postvec/ninference/libs`; independently versioned |
| `postvec-model-minilm-l6-v2` | Bundled 384-d model; independently versioned (`2.1.0` = registry revision 2, bundle 1) |
| `postvec-extras` | Metapackage pinning the runtime + model (not a complete install) |
| `...-pgNN` image | PostgreSQL + pgvector + postvec + CLI, remote mode |
| `...-pgNN-complete` image | The above plus ONNX Runtime and MiniLM (both modes) |

Debian 12 and Ubuntu 22.04 packages are not interchangeable, even though both
are `.deb`. The filename tag has to match the host that will run the
binaries.

## After the files are on disk

After the files are on disk, configure the cluster. Packages leave
PostgreSQL configuration, databases and the model inventory for that
later step.

- [Configure the cluster](/docs/install/setup)
- [Docker runtime notes](/docs/install/docker)
- [Uninstall](/docs/install/uninstall)

## Hosting

The selector reads `univec-ai/stack` releases tagged `postvec-v*`. If
packages later move to a dedicated repository or object store, the artifact
identity and image-tag scheme stay the compatibility contract.
