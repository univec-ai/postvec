---
title: FAQ
description: Short answers to common postvec questions.
---

# FAQ

## Are managed PostgreSQL services supported?

postvec needs `shared_preload_libraries = 'postvec'`. A self-managed host
or the [Docker image](/docs/install/docker) is required.

## What are embedding debt and vector lock-in?

[Defined here](/docs/concepts/lock-in). Vector lock-in is the dependency of
stored vectors on one model space. Embedding debt is the accumulated cost and
risk of changing that dependency.

## Is a UniVec API key required?

No key is required for the bundled MiniLM model or local search. A key and
`postvec login` are required to pull **private-catalogue** models. That route
is identity-only, so a dedicated key with a $0 spending limit is suitable.

Hosted UniVec embeddings and conversions use a key configured with
`postvec provider add univec`. Those calls are billable, so a $0 spending
limit refuses them. The registry credential and the provider credential are
stored separately. Account details: [univec.ai](https://univec.ai).

## Can postvec use hosted embedding APIs?

Yes. The built-in connectors cover OpenAI, OpenRouter, Mistral, Gemini,
Cohere, AWS Bedrock and UniVec. The connector file and the key live on the
inference side; PostgreSQL holds neither.
[External providers](/docs/models/providers) is the walkthrough.

## Can UniVec convert stored vectors without a local converter?

Yes. A `kind = "convert"` entry in `univec.toml` supplies a direct hosted
route to `migrate()`. Stored vectors leave the inference host for conversion.
Hosted converters are for `migrate()` / `convert()` only; embed-bridge
uses local models. See [UniVec hosted models](/docs/models/univec).

## How do I add OpenAI to an existing cluster?

```bash
sudo postvec provider add openai --model text-embedding-3-small
```

Then `enable()` the column with `model => 'openai-text-embedding-3-small'`.
The call emits a NOTICE that source text will leave the host. Same SQL
as a local model after that.

## Can an ada-002 corpus remain unchanged?

Yes. [Adopt the column](/docs/guides/adopt) and name that space;
[embed-bridge](/docs/guides/bridge) produces query vectors in it without a
corpus re-embed or OpenAI call.

Adding an OpenAI key later changes that: a direct embed route wins over a
bridge, so the column starts being embedded by the provider. `provider add`
lists the affected columns and asks first.

## Does embedded inference require internet access?

No. Models already present on disk run in the launcher. `model pull` and
`ls --available` contact the registry when invoked. Air-gapped hosts are
[supported](/docs/models/air-gapped).

## Where do provider keys go?

Into `0600` connector files in a `providers.d` directory on the inference
side: the database host in embedded mode, each `postvec-server` node in
remote mode. Never a GUC, a catalog table or a SQL argument. The one setting
involved, `postvec.providers_path`, holds a path.
[External providers](/docs/models/providers).

## How do I run inference off the database host?

Run one or more `postvec-server` nodes and point the cluster at them with
`postvec setup --grpc ... --http ...`. [Remote inference](/docs/server/) is
the walkthrough, from one node to a fleet. The node also serves a
[dashboard](/docs/server/dashboard) on port `22222` for querying loaded
models and for registry operations.

## Why is the vector NULL right after INSERT?

[Eventual consistency](/docs/concepts/consistency). Wait for
`pending_jobs = 0`.

## Why is search slow?

Build an ANN index. [Indexes](/docs/guides/indexes).

## Why can a new row produce only a lexical match?

Either the vector is not filled yet, or query embedding failed and
`search_degrade_to_fts` kicked in.

## Can an existing `embedding vector(768)` column be retained?

Yes. [`adopt()`](/docs/guides/adopt). The column stays application-owned.

## Can chunk size be changed later?

Not live. `disable` (optionally `drop_destination`), then `enable`
again.

## Does `model upgrade` rewrite stored vector columns?

No. [`migrate()`](/docs/guides/migrate) or a re-embed changes stored
vectors. `model upgrade` replaces model files.

## Is the extension AGPL?

No. The extension, CLI and packages use the PostgreSQL License, in either
inference mode. `postvec-server` is source-available under the Business
Source License 1.1; production use by an organization needs a commercial
license.

## Which PostgreSQL versions are supported?

16, 17 and 18.

## Does `apt remove` drop database data?

No. Package removal deletes files. The
[uninstall procedure](/docs/install/uninstall) removes the worker from a
database.

## Can two PostgreSQL majors run on one host?

Yes. The CLI is a separate package so `postgresql-16-postvec` and
`postgresql-18-postvec` can coexist. Select with `--cluster`.

## Where are the downloads hosted?

GitHub Releases and GHCR are the configured publication channels. See
[Release artifacts](/download) for the live/preview state. A signed
apt/yum repository is planned later.
