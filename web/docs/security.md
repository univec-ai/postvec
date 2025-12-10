---
title: Security
description: Grants, RLS, credentials, and worker visibility.
---

# Security

## On-prem operation

Embedded mode runs the engine in the PostgreSQL launcher. There is no
outbound embedding API and no third-party inference service. Text, weights and
inference stay on the host. The engine uses packaged libraries such as ONNX
Runtime locally; “on-prem” does not mean the software has no upstream
dependencies.

## No provider keys in PostgreSQL

Remote inference authenticates to the configured ninference fleet on the
deployment network, not to a SaaS embedding API. UniVec API keys are used only by
`postvec login` / `model pull` on the host, stored `0600`, per effective
user. They are not stored in PostgreSQL GUCs.

Presigned registry URLs are omitted from terminal output, JSON, and receipts.

## Worker visibility

The worker connects as the bootstrap superuser and **bypasses RLS**. Source
text that must remain hidden from administrators is therefore unsuitable for
`enable()`. Any role with `SELECT` access to the table can read the derived
vector.

Chunk views use `security_invoker` / `security_barrier` and FORCE RLS
keyed on source visibility. Application access requires `GRANT SELECT` on the
destination and view.

## Required grants

The column-scoped `PUBLIC INSERT (registry_id, pk_value)` on
`postvec.jobs` is required for non-owner DML on enabled tables. The TRUNCATE
purge and the shared chunk trigger
functions are `SECURITY DEFINER` with a confused-deputy guard: the
firing table must be the registry entry's source (or a partition).

`embed` / `convert` / `refresh_models` are revoked from PUBLIC. `search`
is not.

## Host writes

The CLI refuses configuration writes through symlinks, multiply-linked files,
and group/world-writable parent directories. Every write uses a same-directory temporary file
+ `rename()` + `fsync`. A half-written `shared_preload_libraries` line
is a cluster that will not start.

`--yes` does not override a `Foreign` or `Modified` `99-postvec.conf`.

## Untrusted extension

`CREATE EXTENSION postvec` requires superuser. Generated SQL runs as
superuser. Identifiers go through `quote_ident`; filter values are bind
parameters; template literals use a setting-independent `E'…'` helper.

## Containers

Embedded control listeners are loopback-only. No environment variable
can move them. Published PostgreSQL ports should bind to `127.0.0.1` unless
external exposure is required.
