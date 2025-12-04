---
title: Security
description: Grants, RLS, credentials, and what the worker can see.
---

# Security

## On-prem is the default story

Embedded mode runs the engine in the PostgreSQL launcher. There is no
outbound embedding API and no third-party runtime. Text, weights and
inference stay on the host.

## No provider keys in PostgreSQL

Remote inference authenticates to *your* ninference fleet — still your
network, not a SaaS embed API. UniVec API keys are used only by
`postvec login` / `model pull` on the host, stored `0600`, per effective
user. They are never a GUC.

Presigned registry URLs never appear in terminal output, JSON, or
receipts.

## What the worker can read

The worker connects as the bootstrap superuser and **bypasses RLS**. Do
not `enable()` a column whose source text must be hidden from
administrators. Anyone who can `SELECT` the table can read the derived
vector.

Chunk views use `security_invoker` / `security_barrier` and FORCE RLS
keyed on source visibility — after you `GRANT SELECT` on the destination
and view.

## Grants that look surprising

The column-scoped `PUBLIC INSERT (registry_id, pk_value)` on
`postvec.jobs` is load-bearing. Tightening it breaks non-owner DML on
enabled tables. The TRUNCATE purge and the shared chunk trigger
functions are `SECURITY DEFINER` with a confused-deputy guard: the
firing table must be the registry entry's source (or a partition).

`embed` / `convert` / `refresh_models` are revoked from PUBLIC. `search`
is not.

## Host writes

The CLI's config writes refuse symlinks, multiply-linked files, and
group/world-writable parents. Every write is a same-directory temp file
+ `rename()` + `fsync`. A half-written `shared_preload_libraries` line
is a cluster that will not start.

`--yes` never overrides a `Foreign` or `Modified` `99-postvec.conf`.

## Untrusted extension

`CREATE EXTENSION postvec` requires superuser. Generated SQL runs as
superuser. Identifiers go through `quote_ident`; filter values are bind
parameters; template literals use a setting-independent `E'…'` helper.

## Containers

Embedded control listeners are loopback-only. No environment variable
can move them. Bind published PostgreSQL ports to `127.0.0.1` unless
you mean to expose them.
