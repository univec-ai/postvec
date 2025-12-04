---
title: Download
description: GitHub Release packages, GHCR images, and how to verify them.
outline: deep
---

# Download

postvec ships as **GitHub Release assets** and **GHCR images**. There is no
signed apt/yum repository yet — that is deliberate until key rotation is
rehearsed. Use the selector for the exact filenames of the current release
identity, `0.1.0-1`.

<DownloadPanel />

## What each artifact is

| Artifact | What you get |
|---|---|
| `postvec-cli` | `/usr/bin/postvec` |
| `postgresql-NN-postvec` / `postgresqlNN-postvec` | Extension library, control file, versioned SQL |
| `postvec-onnxruntime` | CPU ONNX Runtime under `/opt/postvec/ninference/libs` |
| `postvec-model-minilm-l6-v2` | Bundled 384-d model |
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

If this page later moves to another bucket or a dedicated `postvec` GitHub
repository, the filenames and image tags stay the same. The selector above
reads `univec-ai/stack` releases tagged `postvec-v*`.
