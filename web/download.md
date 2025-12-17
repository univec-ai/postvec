---
title: Downloads
description: GitHub Release packages, GHCR images, publication status and verification.
outline: deep
---

# Downloads

GitHub Releases and GHCR are the publication channels. The selector checks
GitHub for a published postvec release and links assets only when they exist.
Until then the names below are the contract for a local build.

There is no signed apt/yum repository. Installation is from downloaded files,
so a key-rotation and repository lifecycle are not implied before they exist.
The current release identity is `0.1.0-1`.

<DownloadPanel />

## Artifact contents

| Artifact | Contents |
|---|---|
| `postvec-cli` | `/usr/bin/postvec` |
| `postgresql-NN-postvec` / `postgresqlNN-postvec` | Extension library, control file, versioned SQL |
| `postvec-onnxruntime` | CPU ONNX Runtime under `/opt/postvec/ninference/libs`; independently versioned |
| `postvec-model-minilm-l6-v2` | Bundled 384-d model; independently versioned (`2.1.0` = registry revision 2, bundle 1) |
| `postvec-embedded` | Metapackage pinning the runtime + model |
| `...-pgNN` image | PostgreSQL + pgvector + postvec + CLI, remote mode |
| `...-pgNN-embedded` image | The above plus the engine and MiniLM |

Debian 12 and Ubuntu 22.04 packages are not interchangeable, even though both
are `.deb`. The filename tag has to match the host that will run the
binaries.

## After the files are on disk

Packages install files and stop. They do not edit PostgreSQL, create a
database or download models.

- [Configure the cluster](/docs/install/setup)
- [Docker runtime notes](/docs/install/docker)
- [Uninstall](/docs/install/uninstall)

## Hosting

The selector reads `univec-ai/stack` releases tagged `postvec-v*`. If
packages later move to a dedicated repository or object store, the artifact
identity and image-tag scheme stay the compatibility contract.
