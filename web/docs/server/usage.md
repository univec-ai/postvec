---
title: When to use postvec-server
description: Process isolation, multi-threaded inference, GPU fleets, dashboard and managed PostgreSQL.
---

# When to use postvec-server

The default install runs inference inside PostgreSQL (embedded mode).
[postvec-server](/docs/server/) is the companion process for remote mode
(`postvec.mode = grpc`) and for [managed PostgreSQL](/docs/server/managed).
SQL, the job queue and the wire contract stay the same.

Use postvec-server when you need any of the following.

**Process isolation.** Inference runs in its own process. A model fault
stays in postvec-server. The extension already has guardrails around
PostgreSQL stability; a separate process is the extra boundary some
compliance regimes require.

**Threads on one VM.** The embedded engine is a single thread inside
the database process (tokio actors). postvec-server runs multi-threaded
alongside PostgreSQL on the same machine.

**Throughput and a fleet.** Offload models to CPU or GPU hosts on the
network. Every member of a [fleet](/docs/server/fleet) loads the same
set; postvec round-robins the gRPC list.

**Model management.** Pull, activate, load and query models from the
[dashboard](/docs/server/dashboard) or the [HTTP API](/docs/server/http-api).

**Managed PostgreSQL.** RDS, Aurora, Cloud SQL, Azure Flexible Server,
Supabase and Neon cannot load `postvec.so`. postvec-server installs a
plain SQL schema, runs the worker and optionally proxies
`search(text)`. [Managed PostgreSQL](/docs/server/managed).

## Next steps

| Situation | Path |
|---|---|
| Try both containers | [Quick start remote](/docs/quickstart-remote) |
| Install the server | [Docker](/docs/server/docker), [packages](/docs/server/packages) or [from source](/docs/server/source) |
| Point an existing cluster at it | [Connect PostgreSQL](/docs/server/connect) |
| RDS, Aurora, Cloud SQL, Azure, Supabase, Neon | [Managed PostgreSQL](/docs/server/managed) |

## License

`postvec-server` is **Business Source License 1.1** (source-available).
Development, testing, personal production use and one 30-day production
evaluation per organization are free. Production use by an organization
needs a [postvec Pro](https://univec.ai) subscription (€30/month,
including €30 of UniVec API credit and commercial rights to the private
catalogue). Hosting it for third parties needs an enterprise agreement.
Each version converts to the PostgreSQL License four years after
release.

The extension, CLI and their packages stay under the PostgreSQL License
in either inference mode. [License](/docs/license).
