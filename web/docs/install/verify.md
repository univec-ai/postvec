---
title: Verify artifacts
description: Optional checksums, Sigstore attestations, file checks and doctor for a published release.
---

# Verify artifacts

Use these checks to prove a published release artifact came from the
postvec release workflow, or that a cluster is healthy after setup.

Needs GitHub CLI 2.49 or newer for attestations. Checksums need
`SHA256SUMS` from the same [GitHub Release](/download).

## Packages

<PgSnippet id="packages-verify-download" />

`--ignore-missing` supports a partial download; omit it for a complete
release.

The signer is `univec-ai/postvec/.github/workflows/postvec-release.yml`.

## Prerequisites script

<PgSnippet id="packages-verify-prereq" />

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

## Container health

`postvec-healthcheck` asserts six properties in one query: the extension
is installed; the library version equals the installed SQL version; the
running mode equals `POSTVEC_MODE`; the build has embedded capability;
the worker heartbeat is younger than `postvec.heartbeat_interval_ms`
plus three `postvec.poll_interval_ms` ticks plus two seconds; and the
first name in `POSTVEC_EMBEDDED_MODELS` is installed.
`POSTVEC_HEALTHCHECK_DATABASE` selects the database and
`POSTVEC_HEALTHCHECK_BEAT_AGE` overrides the heartbeat budget. The exit
code is 0 only when all six hold.

## Inference node

On a host that runs a [postvec-server](/docs/server/) node:

```bash
postvec-server status
curl --silent --insecure https://127.0.0.1:22222/ready
```

`status` prints the binary version, the resident models, and every member
of the cluster with its address, status and version; `--fleet` adds the
model count per member. `postvec-server --version` matches the
extension's release. `/ready` answers 200 once a model can serve, and 503
with a reason before the first model loads and during a drain; the node
image uses it as its container healthcheck. [Fleet](/docs/server/fleet)
covers drift between nodes.

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

`--deep` also hashes CLI-installed model receipts, `--strict` fails on
warnings and `--format json` is for scripts.

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
