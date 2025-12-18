---
title: Install packages
description: Package installation for Debian, Ubuntu and EL9.
---

# Install packages

Use packages when the host will keep postvec installed. Each install
matches **one** release, **one** distribution, **one** architecture and
**one** PostgreSQL major. The [release artifacts](/download) page lists
names and publication status.

## 1. Prerequisites (PGDG)

Supported distros do not ship every PostgreSQL-major x pgvector pair in
their default archives. Each release includes `postvec-prerequisites.sh`.

```bash
gh --version   # 2.49 or newer

gh attestation verify postvec-prerequisites.sh \
  --repo univec-ai/stack \
  --signer-workflow univec-ai/stack/.github/workflows/postvec-release.yml
less postvec-prerequisites.sh
sudo bash ./postvec-prerequisites.sh --pg 18
```

`--pg` is required (`16`, `17` or `18`). `--print` / `--dry-run` shows the
commands. `--yes` skips the prompt.

The script is idempotent. It installs **no** postvec package, edits no
cluster and restarts nothing. On CentOS Stream 9 or subscribed RHEL 9 it
prints the commands and requires `--force-untested`.

## 2. Embedded (recommended)

Extension, CLI, ONNX Runtime and the bundled MiniLM model. On-prem
inference; `setup --embedded` afterwards.

:::: code-group

```bash [Debian / Ubuntu]
sudo apt install \
  ./postvec-cli_0.1.0-1+deb12_amd64.deb \
  ./postgresql-18-postvec_0.1.0-1+deb12_amd64.deb \
  ./postvec-onnxruntime_*.deb \
  ./postvec-model-minilm-l6-v2_*.deb \
  ./postvec-extras_*.deb
```

```bash [EL9]
sudo dnf install \
  ./postvec-cli-0.1.0-1.el9.x86_64.rpm \
  ./postgresql18-postvec-0.1.0-1.el9.x86_64.rpm \
  ./postvec-onnxruntime-*.rpm \
  ./postvec-model-minilm-l6-v2-*.rpm \
  ./postvec-extras-*.rpm
```

::::

Files land under `/opt/postvec/ninference`. MiniLM is 384-d and needs no
API key. Installing this payload leaves embedded mode off.
`postvec setup --embedded` turns it on.

## 3. Extension + CLI only

This payload is for remote mode, where a ninference fleet performs
inference:

:::: code-group

```bash [Debian / Ubuntu]
sudo apt install \
  ./postvec-cli_0.1.0-1+deb12_amd64.deb \
  ./postgresql-18-postvec_0.1.0-1+deb12_amd64.deb
```

```bash [EL9]
sudo dnf install \
  ./postvec-cli-0.1.0-1.el9.x86_64.rpm \
  ./postgresql18-postvec-0.1.0-1.el9.x86_64.rpm
```

::::

Use `apt` / `dnf`, not `dpkg` / `rpm -i`, so PostgreSQL, pgvector and ELF
dependencies resolve.

The filename tag (`+deb12`, `+ubuntu22.04`, `+ubuntu24.04`, `.el9`)
identifies the **target OS**. Debian 12 and Ubuntu 22.04 packages are not
interchangeable.

## 4. Verify the download

```bash
SIGNER=univec-ai/stack/.github/workflows/postvec-release.yml

sha256sum --ignore-missing --check SHA256SUMS
gh attestation verify postgresql-18-postvec_0.1.0-1+deb12_amd64.deb \
  --repo univec-ai/stack --signer-workflow "$SIGNER"
```

`--ignore-missing` supports partial downloads and should be omitted when the
complete release is present.

## 5. Verify installed files

```bash
postvec --version
test -f /usr/lib/postgresql/18/lib/postvec.so
test -f /usr/share/postgresql/18/extension/postvec.control
```

:::: tip Expected
These checks do not require a running cluster. Package installation does not
create `99-postvec.conf`, create a database or start a worker. Worker and
database configuration happens during
[cluster configuration](/docs/install/setup).
::::

## Package installation scope

Package installation does not:

- edit PostgreSQL configuration or `pg_hba.conf`
- restart or reload PostgreSQL
- run `CREATE EXTENSION` or `DROP EXTENSION`
- access the model registry
- delete user data during `apt remove`

## Cluster configuration

[Configure the cluster](/docs/install/setup)
