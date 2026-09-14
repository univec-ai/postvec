---
title: FAQ
description: Short answers to common postvec questions.
---

# FAQ

## I used pgai or pg_vectorize. What maps across?

[Coming from pgai](/docs/from-pgai) maps `create_vectorizer` /
`vectorize.table` onto `enable()` / `adopt()`, and their search helpers
onto `search()`. Existing `vector(N)` columns stay; adopt them, then
search that space.

## Are managed PostgreSQL services supported?

Yes. RDS, Aurora, Cloud SQL, Azure Flexible Server, Supabase and Neon
cannot load a third-party `.so`. [Managed PostgreSQL](/docs/server/managed)
installs a plain SQL schema and runs the worker in `postvec-server`.
Self-hosted clusters that can load the extension still use
[packages](/docs/install/packages) and `postvec setup`.

## I have vectors from a retired model. Do I need to migrate?

[Adopt the column](/docs/guides/adopt) and
[search the existing space](/docs/guides/bridge). Each query is converted
into the stored space (embed-bridge). Stored rows stay. That is the
usual first path. Migrate later if you want a different stored model.

## What are embedding debt and vector lock-in?

[Defined here](/docs/concepts/lock-in). Vector lock-in is the dependency of
stored vectors on one model space. Embedding debt is the accumulated cost and
risk of changing that dependency.

## Is a UniVec API key required?

The bundled MiniLM model and local search run without a key. A key and
`postvec login` are required to pull **private-catalogue** models. That route
is identity-only, so a dedicated key with a $0 spending limit is suitable.

Hosted UniVec embeddings and conversions use a key configured with
`postvec provider add univec`. Those calls are billable, so a $0 spending
limit refuses them. The registry credential and the provider credential are
stored separately. Account details: [univec.ai](https://univec.ai).

## Can postvec use hosted embedding APIs?

Yes. The built-in connectors cover [OpenAI](/docs/models/openai),
[Cohere](/docs/models/cohere), [Amazon Bedrock](/docs/models/aws),
[Gemini](/docs/models/gemini), [Mistral](/docs/models/mistral),
[OpenRouter](/docs/models/openrouter) and [UniVec](/docs/models/univec).
The connector file and the key live on the inference side; PostgreSQL
holds neither. [External providers](/docs/models/providers) is the
walkthrough. On a remote cluster the files live on
[postvec-server](/docs/server/).

## Can UniVec convert stored vectors without a local converter?

Yes. A `kind = "convert"` entry in `univec.toml` supplies a direct hosted
route to `migrate()`. Stored vectors leave the inference host for conversion.
Hosted converters are for `migrate()` / `convert()` only; embed-bridge
uses local models. See [UniVec hosted models](/docs/models/univec).

## How do I add a hosted provider?

Each connector is a copy-paste page. The key is prompted for without
echo:

| Provider | Command |
|---|---|
| [OpenAI](/docs/models/openai) | `sudo postvec provider add openai --model text-embedding-3-small` |
| [Cohere](/docs/models/cohere) | `sudo postvec provider add cohere --model embed-v4.0` |
| [Amazon Bedrock](/docs/models/aws) | `sudo postvec provider add aws --model amazon.titan-embed-text-v2:0 --region us-east-1` |
| [Gemini](/docs/models/gemini) | `sudo postvec provider add google --model gemini-embedding-001` |
| [Mistral](/docs/models/mistral) | `sudo postvec provider add mistral --model mistral-embed` |
| [OpenRouter](/docs/models/openrouter) | `sudo postvec provider add openrouter --model openai/text-embedding-3-small` |
| [UniVec](/docs/models/univec) | `sudo postvec provider add univec --model baai-bge-m3` |

Then `enable()` the column with the SQL name printed by `provider ls`.
The call emits a NOTICE that source text will leave the host. Same SQL
as a local model after that. On postvec-server, add `--path <server-root>
--acknowledge-in-use`.

## Can an ada-002 corpus remain unchanged?

Yes. [Adopt the column](/docs/guides/adopt) and name that space;
[search a retired space](/docs/guides/bridge) produces query vectors in
that space from a local embed-bridge route.

Adding an OpenAI key later changes that: a direct embed route wins over a
bridge, so the column starts being embedded by the provider. `provider add`
lists the affected columns and asks first. The route order in
`postvec.routes` decides this: `postvec model prefer <space> <route>` sets
it, and `provider add --prefer` puts the new provider route first.
See [external providers](/docs/models/providers).

## Does embedded inference require internet access?

Models already present on disk run in the launcher. `model pull` and
`ls --available` contact the registry when invoked. Air-gapped hosts are
[supported](/docs/models/air-gapped).

## Where do provider keys go?

Into `0600` connector files in a `providers.d` directory on the inference
side: the database host in embedded mode, each `postvec-server` in
remote mode. The one setting involved, `postvec.providers_path`, holds a
path. [External providers](/docs/models/providers).

## When should I use postvec-server?

For process isolation from PostgreSQL, multi-threaded inference on the
same VM, a CPU or GPU fleet and model management from the dashboard or
HTTP API. Managed cloud databases use it too. SQL is unchanged.
[When to use postvec-server](/docs/server/usage).

Install [postvec-server](/docs/server/)
([Docker](/docs/server/docker), [packages](/docs/server/packages))
and point the cluster at it with `postvec setup --grpc ... --http ...`.
[Quick start remote](/docs/quickstart-remote) runs both containers on
one network. The [dashboard](/docs/server/dashboard) is port `22222`.
Managed clouds: [managed PostgreSQL](/docs/server/managed).

## Why is the vector NULL right after INSERT?

[Eventual consistency](/docs/concepts/consistency). Wait for
`pending_jobs = 0`.

## Why is search slow?

Build an ANN index. [Indexes](/docs/guides/indexes). Keyword traffic
also wants a GIN: `create_fts_index => true` at enable time. See
[BM25](/docs/guides/bm25).

## How do I search by keywords only?

```sql
SELECT * FROM postvec.search(
  'public.docs', 'body', 'reset password',
  semantic_weight => 0.0
);
```

`semantic_weight => 1.0` is vector only. Default `0.5` is hybrid.
[BM25](/docs/guides/bm25) covers the GIN, corpus stats and flags.

## Why can a new row produce only a lexical match?

Either the vector is not filled yet, or query embedding failed and
`search_degrade_to_fts` kicked in.

## Can an existing `embedding vector(768)` column be retained?

Yes. [`adopt()`](/docs/guides/adopt). The column stays application-owned.

## Can chunk size be changed later?

`disable` (optionally `drop_destination`), then `enable` again with the
new splitter settings. Live splitter reconfiguration is refused.

## Does `model upgrade` rewrite stored vector columns?

[`migrate()`](/docs/guides/migrate) or a re-embed changes stored
vectors. `model upgrade` replaces model files.

## Is the extension AGPL?

The extension, CLI and packages use the PostgreSQL License, in either
inference mode. `postvec-server` is source-available under the Business
Source License 1.1: free for development, testing, personal noncommercial
production and one 30-day production evaluation per organization,
including its affiliates under common control. Production use by an
organization needs a [postvec Pro](https://univec.ai) subscription, and
hosting or embedding the server for third parties needs a platform/OEM
agreement. Each version becomes PostgreSQL-licensed four years after
release. Table and Pro contents: [License](/docs/license).

## Which PostgreSQL versions are supported?

16, 17 and 18.

## Does `apt remove` drop database data?

Package removal deletes files. The
[uninstall procedure](/docs/install/uninstall) removes the worker from a
database.

## Can two PostgreSQL majors run on one host?

Yes. The CLI is a separate package so `postgresql-16-postvec` and
`postgresql-18-postvec` can coexist. Select with `--cluster`.

## Where are the downloads hosted?

GitHub Releases and GHCR are the configured publication channels. See
[Release artifacts](/download) for the live/preview state. A signed
apt/yum repository is planned later.
