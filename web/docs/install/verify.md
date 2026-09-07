---
title: Verify artifacts
description: Optional checksums, Sigstore attestations, file checks and doctor for a published release.
---

# Verify artifacts

Use these checks to prove a published `0.1.0-1` artifact came from the
postvec release workflow, or that a cluster is healthy after setup.

Needs GitHub CLI 2.49 or newer for attestations. Checksums need
`SHA256SUMS` from the same [GitHub Release](/download).

## Packages

<PgSnippet id="packages-verify-download" />

`--ignore-missing` supports a partial download. Omit it when you have
the complete release.

The signer is `univec-ai/postvec/.github/workflows/postvec-release.yml`.

## Prerequisites script

<PgSnippet id="packages-verify-prereq" />

`less` is so you can read the script before running it.

## Installed files

<PgSnippet id="packages-verify-files" />

:::: tip Expected
These checks succeed on the files alone. A local install also has
`libonnxruntime.so` under `/opt/postvec/libs` and a model tree under
`/opt/postvec/models`.
::::

## Container images

<PgSnippet id="docker-verify" />

The node image:

<PgSnippet id="docker-server-verify" />

## Cluster

After [setup](/docs/install/setup):

```bash
sudo postvec doctor --database app --deep
```

```sql
SHOW postvec.mode;
SHOW postvec.path;
SELECT postvec.version(), postvec.build_info();
SELECT extname, extversion FROM pg_extension
 WHERE extname IN ('vector', 'postvec');
SELECT name, model_type, target_dim FROM postvec.models ORDER BY name;
SELECT * FROM postvec.status();
```

:::: tip Expected
`doctor` exit 0: library and SQL versions match, the heartbeat
advances, on-disk / engine / SQL inventories agree. `postvec.mode` is
`embedded` or `grpc`. At least one model is listed when inference is
reachable.
::::

`--deep` also hashes CLI-installed model receipts. `--format json` is
for scripts. `--strict` fails on warnings.

On a container host, `postvec doctor` finds no `pg_lsclusters` cluster.
Use `postvec-healthcheck`, or:

```bash
docker exec -u postgres postvec \
  postvec doctor \
  --database-url 'postgresql:///app?host=/var/run/postgresql' \
  --database app
```

- [Downloads](/download)
- [Configure the cluster](/docs/install/setup)
- [CLI](/docs/reference/cli)
