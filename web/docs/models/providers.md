---
title: External providers
description: Point a column at OpenAI, Cohere, Amazon Bedrock, Gemini, Mistral, OpenRouter or UniVec. The API key stays on the inference host.
---

# External providers

An external provider is a hosted embedding API that postvec calls from
the inference host. You drop a connector file into `providers.d`, bind a
column to one of that file's model names, and `enable()` / `search()` /
`migrate()` keep the same SQL as a local model.

The key lives in that file. PostgreSQL holds a path
(`postvec.providers_path`), never a credential. Source text for a bound
column is sent to the provider on every insert, update and query
embed.

This is inventory on whichever host already runs inference. `postvec.mode`
stays `embedded` or `grpc`. In embedded mode the host is the database
machine. In remote mode it is each [postvec-server](/docs/server/)
process, which is also where you put the files when you want keys off
the database host.

| Provider | Typical SQL name | Serves |
|---|---|---|
| [OpenAI](/docs/models/openai) | `openai-text-embedding-3-small` | Embeddings |
| [Cohere](/docs/models/cohere) | `cohere-embed-v4-0` | Embeddings |
| [Amazon Bedrock](/docs/models/aws) | `aws-titan-embed-text-v2-0` | Titan embeddings |
| [Gemini](/docs/models/gemini) | `gemini-embedding-001` | Embeddings |
| [Mistral](/docs/models/mistral) | `mistral-mistral-embed` | Embeddings |
| [OpenRouter](/docs/models/openrouter) | `openrouter-openai-text-embedding-3-small` | Embeddings (OpenAI-shaped fronts) |
| [UniVec](/docs/models/univec) | `univec-baai-bge-m3` | Embeddings and hosted vector conversion |

TOML, key sources and `doctor` checks:
[connector files](/docs/models/providers-file).

## Keys

| Mode | Directory |
|---|---|
| Embedded | `/etc/postvec/providers.d` on the database host |
| Remote (`postvec-server`) | `/opt/postvec/providers.d` on each node (the engine root) |

Files are `0600` in a `0700` directory, owned by the inference process
account. A wider mode is refused. `provider add` prompts without echo, or
takes `--api-key-file`, `--api-key-env` or `--key-stdin`. A key is
refused as a bare argument. `provider ls` prints the source, not the
value.

`provider add TYPE --model ID` writes the file, probes the key (one
billed embed per model), reloads the running host and refreshes
`postvec.models`. `--no-verify` skips the probe. If the host is down, the
files are still written and are read at the next start.

```bash
sudo postvec provider add openai --model text-embedding-3-small
postvec provider ls
sudo postvec doctor --database app
```

In a container the CLI runs as root, and the `-local` and postvec-server
images set `POSTVEC_PROVIDERS_PATH`, so no `sudo` and no `--path`:

```bash
docker exec -it postvec postvec provider add openai --model text-embedding-3-small
```

On a postvec-server host there is no cluster, so the command edits the
files under `/opt/postvec` and checks no columns; `--acknowledge-in-use`
stands in for that check:

```bash
sudo postvec provider add openai --model text-embedding-3-small \
     --acknowledge-in-use --yes
```

Put the same connector files on every node in a [fleet](/docs/server/fleet).
Round-robin to a node that lacks one fails that request.

## Bind a column

```sql
SELECT postvec.enable('docs', 'body',
                      model => 'openai-text-embedding-3-small');
-- NOTICE:  postvec: model "openai-text-embedding-3-small" is served by
--          external provider "openai"; source text from column "body" will
--          be sent to that provider for embedding
```

`enable()`, `adopt()` and `migrate()` emit that NOTICE for a provider
embed model. A UniVec hosted conversion emits a separate NOTICE that
stored vectors will leave the host.

`search()` embeds the query in the connection backend.
`postvec.query_timeout_ms` defaults to 2000 ms, which is tight for a
hosted API:

```sql
SET postvec.query_timeout_ms = 10000;
```

If the embed times out, `postvec.search_degrade_to_fts` (default on)
returns lexical ranks only. To embed once in the application, call
[`search_with_vector()`](/docs/guides/search). Writes use
`postvec.embed_timeout_ms` (30 s).

Local and hosted columns coexist in one database. Each column names its
own model.

## Adding a key can change an existing column

Adding a route never changes where an existing column's text goes. A
new provider joins at the back of its space until `model prefer` (or
`provider add --prefer`) says otherwise. Exact route bindings stay on
that route while it is served.

::::: warning Columns already bound to this name
[Route resolution](/docs/guides/bridge) prefers a direct embed over a
converter. A column that was bridging into this space starts sending
source text to the provider on the next worker cycle. `enable()` stays
as it is.

`provider add` lists those columns and waits for
`--acknowledge-in-use` (or the typed answer). `--yes` confirms the
write; this acknowledgement is separate.
:::::

## Switch providers without a migration

A column bound to `gemini-embedding-001` keeps embedding after the
Google key is removed and an OpenRouter route for the same space is
added. Resolution follows whatever route currently serves the space.

```
sudo postvec provider add openrouter --model google/gemini-embedding-001
sudo postvec model prefer gemini-embedding-001 openrouter-google-gemini-embedding-001
```

`status()` shows `space`, `route` and `route_execution`. Nothing is
re-embedded.

A database the command could not inspect is listed as `UNKNOWN`. Columns
already served by a loaded local model are omitted: the local model
keeps the name.

## Adopt existing provider vectors

To keep searching an existing ada-002 (or similar) column, name that
space at adopt time and [search the existing space](/docs/guides/bridge).
Stored rows stay. A provider key is optional on that path.

```sql
SELECT postvec.adopt('docs', 'body',
                     vector_column => 'body_vec',
                     model => 'openai-text-embedding-ada-002',
                     backfill => 'none');
```

`model` is an assertion: a wrong name embeds later queries into the
wrong space. [Adopt](/docs/guides/adopt) lists the checks.

To have the provider embed new writes and queries, add the key first.
That creates a direct embed route, so the column starts sending source
text to the provider. `provider add` lists affected columns and asks
first.

## Move a column off a provider

```sql
SELECT postvec.migrate('docs', 'body',
                       new_model => 'baai-bge-m3') AS migration_id \gset

SELECT state, rows_done, rows_total FROM postvec.migration_status(:migration_id);
-- until state = 'awaiting_finalize'

SELECT postvec.migration_finalize(:migration_id);
```

`convert` (default) translates stored vectors when a converter is
installed. Without a converter, use `strategy => 'reembed'`.
[Change the stored model](/docs/guides/migrate). Then:

```bash
sudo postvec provider rm openai --acknowledge-in-use --yes
```

`provider rm` lists remaining columns first, then removes the connector
file. Remove a referenced key file by hand if you also want the
credential gone.

## Failures

| What happened | Class | Effect |
|---|---|---|
| Network error, timeout, HTTP 408, 424, 429 or 5xx | Transient | Backoff and retry |
| HTTP 401, 402 or 403 | Config | Retry. On remote, try another node. For UniVec, 402 means the account has no available credit |
| Unknown model id at the provider | Config | Retry. On remote, try another node |
| Empty input, or a NUL | Bad row | Caught before the request. That row goes to `jobs_dead` |
| Other HTTP 4xx, or a wrong count, dimension or index | Permanent | The batch goes to `jobs_dead` |

Config retries until `postvec.max_retries` (default 5), then
dead-letters. Fix the key and re-drive with
[`retry_dead()`](/docs/guides/retry). A migration retries Config until
it succeeds.

A file that fails to load leaves local models and other providers
running.

## Cost

Embedding providers usually bill per token. UniVec conversion bills per
vector. `postvec.max_document_bytes` (1 MiB) dead-letters an oversized
document before an embedding call. `max_concurrent` in the file caps
in-flight requests. Set spend quotas on the provider.

Provider calls ignore `postvec.embedded_max_inflight`. A slow hosted
call runs beside local ONNX.

- [OpenAI](/docs/models/openai)
- [Cohere](/docs/models/cohere)
- [Amazon Bedrock](/docs/models/aws)
- [Gemini](/docs/models/gemini)
- [Mistral](/docs/models/mistral)
- [OpenRouter](/docs/models/openrouter)
- [UniVec](/docs/models/univec)
- [Connector files](/docs/models/providers-file)
- [postvec-server](/docs/server/)
- [Docker](/docs/install/docker#external-providers)
- [Security](/docs/security)
