---
title: Install packages
description: apt and dnf install of postvec on Debian, Ubuntu, and EL9.
---

# Install packages

This is the path a third-party host should use. Download the files for
**one** release, **one** distribution, **one** architecture, **one**
PostgreSQL major. The [download](/download) page names them.

## 1. Prerequisites (PGDG)

Supported distros do not ship every PostgreSQL-major × pgvector pair in
their default archives. Each release includes `postvec-prerequisites.sh`.

```bash
gh --version   # 2.49 or newer

gh attestation verify postvec-prerequisites.sh \
  --repo univec-ai/stack \
  --signer-workflow univec-ai/stack/.github/workflows/postvec-release.yml
less postvec-prerequisites.sh
sudo bash ./postvec-prerequisites.sh --pg 18
```

`--pg` is required (`16`, `17`, or `18`). `--print` / `--dry-run` shows the
commands. `--yes` skips the prompt.

The script is idempotent. It installs **no** postvec package, edits no
cluster, and restarts nothing. On CentOS Stream 9 or subscribed RHEL 9 it
prints the commands and requires `--force-untested`.

## 2. Embedded (recommended)

Extension, CLI, ONNX Runtime and the bundled MiniLM model. On-prem
inference; `setup --embedded` afterwards.

::: code-group

```bash [Debian / Ubuntu]
sudo apt install \
  ./postvec-cli_0.1.0-1+deb12_amd64.deb \
  ./postgresql-18-postvec_0.1.0-1+deb12_amd64.deb \
  ./postvec-onnxruntime_*.deb \
  ./postvec-model-minilm-l6-v2_*.deb \
  ./postvec-embedded_*.deb
```

```bash [EL9]
sudo dnf install \
  ./postvec-cli-0.1.0-1.el9.x86_64.rpm \
  ./postgresql18-postvec-0.1.0-1.el9.x86_64.rpm \
  ./postvec-onnxruntime-*.rpm \
  ./postvec-model-minilm-l6-v2-*.rpm \
  ./postvec-embedded-*.rpm
```

:::

Files land under `/opt/postvec/ninference`. MiniLM is 384-d and needs no
API key. Installing this payload does **not** turn embedded mode on —
`postvec setup --embedded` does.

## 3. Extension + CLI only

Use this when a ninference fleet will do inference (organisation /
remote mode):

::: code-group

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

:::

Use `apt` / `dnf`, not `dpkg` / `rpm -i`, so PostgreSQL, pgvector, and ELF
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

`--ignore-missing` is for a partial download. Omit it when you have the
whole release.

## 5. Confirm files, not a running worker

```bash
postvec --version
test -f /usr/lib/postgresql/18/lib/postvec.so
test -f /usr/share/postgresql/18/extension/postvec.control
```

::: tip Expected
The cluster may still be stopped. No `99-postvec.conf`, no database, no
worker. That is correct. [setup](/docs/install/setup) is the next command.
:::

## What packages never do

- Edit PostgreSQL configuration or `pg_hba.conf`
- Restart or reload
- `CREATE` / `DROP EXTENSION`
- Call the registry
- Delete user data on `apt remove`

## Next

[Configure the cluster](/docs/install/setup)
