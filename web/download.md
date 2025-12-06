---
title: Release artifacts
description: GitHub Release packages, GHCR images, publication status, and verification.
outline: deep
---

# Release artifacts

GitHub Releases and GHCR are the configured publication channels. The selector
checks GitHub for a published postvec release and links assets only when they
exist. Until then, it acts as an exact naming reference for locally built
artifacts.

There is no signed apt/yum repository. Installation is from downloaded files so
the key-rotation and repository lifecycle are not implied before they exist.
The current release identity is `0.1.0-1`.

<DownloadPanel />

## What each artifact is

| Artifact | What you get |
|---|---|
| `postvec-cli` | `/usr/bin/postvec` |
| `postgresql-NN-postvec` / `postgresqlNN-postvec` | Extension library, control file, versioned SQL |
| `postvec-onnxruntime` | CPU ONNX Runtime under `/opt/postvec/ninference/libs`; independently versioned |
| `postvec-model-minilm-l6-v2` | Bundled 384-d model; independently versioned |
| `postvec-embedded` | Metapackage pinning the runtime + model |
| `…-pgNN` image | PostgreSQL + pgvector + postvec + CLI, remote mode |
| `…-pgNN-embedded` image | The above plus the engine and MiniLM |

Debian 12 and Ubuntu 22.04 packages are **not** interchangeable even though
both are `.deb`. Match the host that will *run* the binaries.

## After the files land

Packages install files and stop. They do not edit PostgreSQL, create a
database, or download models.

- [Configure the cluster](/docs/install/setup)
- [Docker runtime notes](/docs/install/docker)
- [Uninstall](/docs/install/uninstall)

## Hosting note

The selector reads `univec-ai/stack` releases tagged `postvec-v*`. If packages
later move to a dedicated repository or object store, the artifact identity and
image-tag scheme remain the compatibility contract.
