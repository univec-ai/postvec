---
title: When to use postvec-server
description: Process isolation, multi-threaded inference, GPU fleets, remote model management and managed PostgreSQL.
---

# When to use postvec-server

The default install runs inference inside the PostgreSQL process (embedded mode).
[postvec-server](/docs/server/) is the companion process for remote mode
(`postvec.mode = grpc`) and for [managed PostgreSQL](/docs/server/managed). The
SQL surface, the job queue and the wire contract are the same in both modes; only
the host that runs the model changes.

Reasons to run it:

**Process isolation.** Inference runs in its own process, so a native ONNX fault
stops the node and leaves the PostgreSQL launcher running.

**Threads on one VM.** The embedded engine is one thread inside the database
process. postvec-server runs multi-threaded next to PostgreSQL on the same
machine, with its own process limits and its own restart cycle.

**Throughput and a fleet.** Models run on CPU or GPU hosts on the network. Every
member of a [fleet](/docs/server/fleet) loads the same set; postvec spreads
requests over the node list, and one fleet serves several databases. A GPU node
is a source build with the `ort-cuda` or `ort-tensorrt` Cargo feature; the
published image and packages carry the CPU build.

**Remote model management.** Pull, activate, load and inspect models from the
[dashboard](/docs/server/dashboard) or the [HTTP API](/docs/server/http-api),
without shell access to the database host.

**Managed PostgreSQL.** RDS, Aurora, Cloud SQL, Azure Flexible Server, Supabase
and Neon cannot load `postvec.so`. postvec-server installs a plain SQL schema,
runs the worker and can proxy `search(text)`.
[Managed PostgreSQL](/docs/server/managed).

Embedded mode remains the simpler choice for a single self-hosted cluster with
one local model: it is the default, it runs without a second service, and the
bundled MiniLM model works with no configuration.

## Next steps

| Situation | Path |
|---|---|
| Try both containers | [Quick start remote](/docs/quickstart-remote) |
| Install the server | [Docker](/docs/server/docker), [packages](/docs/server/packages) or [from source](/docs/server/source) |
| Point an existing cluster at it | [Connect PostgreSQL](/docs/server/connect) |
| RDS, Aurora, Cloud SQL, Azure, Supabase, Neon | [Managed PostgreSQL](/docs/server/managed) |

## License

`postvec-server` is source-available under the Business Source License 1.1
(`BUSL-1.1`). Development, testing, personal production use and one 30-day
production evaluation per organization are free. Production use by an
organization needs a [postvec pro](/server#plans) subscription at €30/month,
which includes €30 of UniVec API credit and commercial rights to the private
catalogue. Hosting the server for third parties needs an enterprise agreement.
Each version becomes PostgreSQL-licensed four years after release.

The extension, the CLI and their packages stay under the PostgreSQL License in
either inference mode. [License](/docs/license).
