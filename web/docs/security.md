---
title: Security
description: Grants, RLS, credentials and worker visibility.
---

# Security

The worker sees source text. Filter values are bind parameters.
Credentials never enter PostgreSQL GUCs.

## On-prem operation

Embedded mode runs the engine in the PostgreSQL launcher. Text, weights
and inference stay on the host. The engine loads packaged libraries such
as ONNX Runtime from the install.

## Credentials stay on the host

Remote inference reaches the configured `postvec-server` nodes on the
deployment network. UniVec API keys are used only by `postvec login` /
`model pull` on the host. They are stored `0600` per effective user and
never written into PostgreSQL GUCs.

Presigned registry URLs are omitted from terminal output, JSON and receipts.

## Provider credentials

Keys for [external providers](/docs/models/providers) live in `0600`
connector files in a `0700` directory, owned by the account the inference
process runs as: the database host in embedded mode, each `postvec-server`
node in remote mode. A connector file, or a key file it references, whose
mode grants group or other bits is refused by name.

The directory itself must not be group- or world-writable, and every
ancestor must be one only root or that same account can rewrite. Anyone
who can write there can drop in a connector and choose where this host
sends source text. The serving host refuses those cases outright.

No key reaches a GUC, a catalog table, a SQL argument, a log line or an
error message. `postvec.providers_path` holds a path. A key is never a
command-line argument, and `provider ls` prints the key source rather than
its value.

The extension itself never calls a provider. The outbound HTTPS request is
made by the inference host, only for models an operator configured, and
only for columns bound to them.

Loopback gRPC (embedded) and node gRPC (remote) are plaintext and
unauthenticated. With providers configured, any local OS account that can
reach the port, or any peer that can reach a node, can spend the provider
budget without a SQL grant. Restrict the node's gRPC port. Use
provider-side quotas and billing alerts as the spend control.

## Worker visibility

The worker connects as the bootstrap superuser and **bypasses RLS**. Source
text that must remain hidden from administrators is therefore unsuitable for
`enable()`. Any role with `SELECT` on the table can read the derived
vector.

Chunk views use `security_invoker` / `security_barrier` and FORCE RLS
keyed on source visibility. Application access needs `GRANT SELECT` on the
destination and view.

## Required grants

The column-scoped `PUBLIC INSERT (registry_id, pk_value)` on
`postvec.jobs` is required for non-owner DML on enabled tables. The TRUNCATE
purge and the shared chunk trigger
functions are `SECURITY DEFINER` with a confused-deputy guard: the
firing table must be the registry entry's source (or a partition).

`embed` / `convert` / `refresh_models` are revoked from PUBLIC. `search`
remains granted to PUBLIC.

## Host writes

The CLI refuses configuration writes through symlinks, multiply-linked files
and group/world-writable parent directories. Every write uses a same-directory temporary file
+ `rename()` + `fsync`. A half-written `shared_preload_libraries` line
is a cluster that will not start.

`--yes` does not override a `Foreign` or `Modified` `99-postvec.conf`.

## Untrusted extension

`CREATE EXTENSION postvec` requires superuser. Generated SQL runs as
superuser. Identifiers go through `quote_ident`; filter values are bind
parameters; template literals use a setting-independent `E'...'` helper.

## Containers

Embedded control listeners are loopback-only. No environment variable
can move them. Published PostgreSQL ports should bind to `127.0.0.1` unless
external exposure is required.
