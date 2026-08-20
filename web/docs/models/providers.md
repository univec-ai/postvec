---
title: External providers
description: Bind a column to OpenAI, Gemini, Cohere, Mistral, AWS Bedrock, OpenRouter or UniVec. The API key stays out of PostgreSQL.
---

# External providers

`postvec provider add` points the inference host at a hosted embedding API.
The key goes in a `0600` file under `providers.d`. PostgreSQL never stores it.

Provider embed models appear in `postvec.models`. `enable()`, `search()` and
re-embedding migrations then use the same SQL as local models. UniVec can
also expose hosted direct converters for `migrate()`.

`postvec.mode` stays `embedded` or `grpc`. Connector files are extra inventory
on whichever host already runs inference.

| | Local model | Provider model |
|---|---|---|
| Inference | The launcher, or a `postvec-server` node | The provider's API, called from that same host |
| Source text | Stays on the host | Sent to the provider for bound columns |
| Command | `postvec model pull` / `activate` | `postvec provider add` |
| SQL name | `baai-bge-m3` | `openai-text-embedding-3-small` |

TOML, names and `doctor` checks: [connector files](/docs/models/providers-file).
[UniVec hosted models](/docs/models/univec) covers its embedding and vector
conversion entries.

## Keys

| Mode | Directory |
|---|---|
| Embedded | `/etc/postvec/providers.d` on the database host |
| Remote | `$root/providers.d` on each `postvec-server` node |

Files are `0600` in a `0700` directory, owned by the inference process
account. A wider mode is refused. Rotate a key that others could read.

`postvec.providers_path` holds a path. `provider add` prompts without echo,
or takes `--api-key-file`, `--api-key-env` or `--key-stdin`. A key is
refused as a bare argument. `provider ls` prints the source.

A bound column sends its source text to the provider on every insert and
update, and `search()` sends the query text. A migration through a hosted
UniVec converter sends the stored vectors instead.

## Catalogue listing

`postvec provider ls --available [PROVIDER]` asks the provider's own model
list instead of reading `providers.d`. UniVec publishes kinds and dimensions,
so its catalogue lists in full, unauthenticated, with your configured
entries marked. No other supported provider states embedding dimensions;
for those the command prints why it cannot list and points at `--model`.
See [UniVec hosted models](/docs/models/univec).

## 1. Add a provider

Writes `/etc/postvec/providers.d/openai.toml`, probes the key and reloads
the host:

```bash
sudo postvec provider add openai --model text-embedding-3-small
postvec provider ls
sudo postvec doctor --database app
```

The probe is one live embed per model (one billed call). `--no-verify`
skips it. If the host is down, the files are still written and are read
at the next start.

::::: tip Expected
```text
providers.d: /etc/postvec/providers.d
openai  (openai, key: inline (redacted))
  openai-text-embedding-3-small                dim 1536   served
```
`doctor` exits 0. The name is in `postvec.models`. No restart.
:::::

`NOT served` after a hand-edit: reload or restart. `REFUSES`: the file
will not load. Fix the reason `ls` prints.

## 2. Bind a column

```sql
SELECT postvec.enable('docs', 'body',
                      model => 'openai-text-embedding-3-small');
-- NOTICE:  postvec: model "openai-text-embedding-3-small" is served by
--          external provider "openai"; source text from column "body" will
--          be sent to that provider for embedding

INSERT INTO docs(body) VALUES ('quarterly revenue guidance increased');

SELECT relation, model, dim, pending_jobs, dead_jobs
  FROM postvec.status();
```

`enable()`, `adopt()` and `migrate()` emit that NOTICE for a provider
embed model. `migrate(..., strategy => 'reembed')` onto a provider sends
every existing row. A direct hosted conversion emits a separate NOTICE that
the stored vectors will leave the host.

::::: tip Expected
`pending_jobs` returns to 0. `docs.body_semantic` is `vector(1536)`.
`dead_jobs` is 0.
:::::

```sql
SELECT postvec.create_vector_index('docs', 'body');

SELECT * FROM postvec.search('docs', 'body', 'revenue outlook', limit_n => 5);
```

`search()` embeds the query in the connection backend.
`postvec.query_timeout_ms` defaults to 2000 ms, which is tight for a
hosted API:

```sql
SET postvec.query_timeout_ms = 10000;   -- USERSET
```

If the embed times out, `postvec.search_degrade_to_fts` (default on)
returns lexical ranks only. To embed once in the application, call
[`search_with_vector()`](/docs/guides/search).

Writes use `postvec.embed_timeout_ms` (30 s). Slow providers show up as
queue lag.

## Two models in one database

Each column names its own model:

```sql
SELECT postvec.enable('articles', 'body',
                      model => 'openai-text-embedding-3-small');

SELECT postvec.enable('notes', 'body',
                      model => 'sentence-transformers-all-minilm-l6-v2');
```

```sql
SELECT relation, model, dim, pending_jobs FROM postvec.status();
```

::::: tip Expected
```text
 relation |               model               | dim  | pending_jobs
----------+-----------------------------------+------+--------------
 articles | openai-text-embedding-3-small     | 1536 |            0
 notes    | sentence-transformers-all-minilm.. |  384 |            0
```
One worker. A provider outage leaves `notes` filling. `articles` search
falls back to FTS while the outage lasts.
:::::

## Adding a key can change an existing column

::::: warning Columns already bound to this name
[Route resolution](/docs/guides/bridge) prefers a direct embed over a
converter. A column that was bridging into this space starts sending
source text to the provider on the next worker cycle. No SQL change, no
NOTICE.

`provider add` lists those columns and waits for
`--acknowledge-in-use` (or the typed answer). `--yes` confirms the
write; this acknowledgement is separate.
:::::

A database the command could not inspect is listed as `UNKNOWN`. Columns
already served by a loaded local model are omitted: the local model
keeps the name.

`provider rm` asks the same way when columns would lose the route, or
when removing one claimant of a contested name would hand the name to
the other file.

`--path` cannot scan a cluster. The flag is required and databases are
listed as `UNKNOWN`.

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

`adopt()` leaves stored bytes as they are. `model` is an assertion: a
wrong name embeds later queries into the wrong space.
[Adopt](/docs/guides/adopt) lists the checks.

To have the provider embed new writes and queries, add the key first:

```bash
sudo postvec provider add openai --model text-embedding-ada-002
```

Adding the key makes a direct embed route, so the column starts sending
source text to the provider. `provider add` lists affected columns and
asks first.

## Move a column off a provider

```sql
SELECT postvec.migrate('docs', 'body',
                       new_model => 'baai-bge-m3') AS migration_id \gset

SELECT state, rows_done, rows_total FROM postvec.migration_status(:migration_id);
-- until state = 'awaiting_finalize'

SELECT postvec.migration_finalize(:migration_id);
```

::::: tip Expected
`convert` (default) translates stored vectors when a converter is
installed. Source text is not sent again. Without a converter, use
`strategy => 'reembed'`.
:::::

```bash
sudo postvec provider rm openai --acknowledge-in-use --yes
sudo rm /etc/postvec/keys/openai.key
```

`provider rm` lists remaining columns first, then removes the connector
file. Remove a referenced key file by hand if you also want the
credential gone.

Lifecycle: [migrate](/docs/guides/migrate).

## Remote nodes

Each node reads `$root/providers.d` (default
`/var/lib/postvec-server/providers.d`):

```bash
sudo postvec provider add openai --model text-embedding-3-small \
     --path /var/lib/postvec-server \
     --acknowledge-in-use --yes
postvec-server status --fleet
```

`--path` writes files only. `DIR` must exist. New files inherit its
owner, so the node can read them after `sudo`.
`--acknowledge-in-use` is required.

Every node needs the same connector files. Round-robin to a node that
lacks one fails that request. The fleet drift report labels
provider-backed names.

A node reloads on restart, or on `POST /admin/providers/reload` at its
loopback admin port. The `provider` commands try that when they run on
the node.

A remote-mode cluster without `--path` is refused. The message names
`--path <server-root>`.

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
alone.

## Cost

Embedding providers usually bill per token. UniVec conversion bills per
vector. `postvec.max_document_bytes` (1 MiB) dead-letters an oversized
document before an embedding call. `max_concurrent` in the file caps
in-flight requests. Set spend quotas on the provider.

Provider calls ignore `postvec.embedded_max_inflight`. A slow hosted
call runs beside local ONNX. Raising `max_concurrent` after
start needs a restart for the full budget. The models serve either way.
The CLI says so.

- [Connector files](/docs/models/providers-file)
- [UniVec hosted models](/docs/models/univec)
- [Search a retired space](/docs/guides/bridge)
- [Remote inference](/docs/server/)
- [Docker](/docs/install/docker#external-providers)
- [Security](/docs/security)
- [CLI](/docs/reference/cli)
