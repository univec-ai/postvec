---
title: External providers
description: Serve OpenAI, Gemini, Cohere, Mistral, AWS Bedrock or OpenRouter embeddings without an API key in PostgreSQL.
---

# External providers

postvec embeds with open-weight models by default. It can also call a hosted
embedding API for the columns you bind to one. No API key enters PostgreSQL.

This is not a third mode. `postvec.mode` still selects `embedded` or `grpc`. A
connector file on the inference side declares which hosted models to serve.
Those models then appear in `postvec.models` next to local ones, and
`enable()`, the job queue, retries, `search()` and `migrate()` treat them the
same way. A column on MiniLM and a column on Gemini coexist in one database.

With no connector file present, nothing changes. Not the model list, not the
concurrency bound, not a log line.

| | Local model | Provider-backed model |
|---|---|---|
| Where inference runs | The launcher, or a `postvec-server` node | The provider's API, called from the launcher or the node |
| Where source text goes | Nowhere off the host | To the provider, for the columns you bind |
| Configuration | `postvec model pull` / `activate` | `postvec provider add` |
| Credential | None | A `providers.d` file. Never PostgreSQL |
| Name in SQL | `baai-bge-m3` | `openai-text-embedding-3-small` |

## Where the key lives

- Connector files sit in a `providers.d` directory on the **inference** side.
  Embedded mode: the database host. Remote mode: each `postvec-server` node,
  and the database host holds nothing.
- Files are `0600` in a `0700` directory, owned by the account the inference
  process runs as. A connector file, or a key file it references, whose mode
  grants group or other bits is refused by name. Rotate a key that others
  could read.
- No credential reaches a GUC, a catalog table, a SQL argument, a log line or
  an error message. The one new GUC, `postvec.providers_path`, holds a path.
- A key is never a command-line argument. `postvec provider add` prompts
  without echo, reads `--key-stdin` or records a file or environment variable
  to read at serve time. `provider ls` prints the key *source*.
- The extension never dials a provider. A backend running `search()` reaches
  the inference host over the loopback gRPC it already uses.

## Add a provider

```bash
sudo postvec provider add openai --model text-embedding-3-small
postvec provider ls
```

The key is prompted for without echo. `--api-key-file`, `--api-key-env` and
`--key-stdin` are the non-interactive forms.

`provider add` writes `/etc/postvec/providers.d/openai.toml`, verifies the key
with one live single-input embed per model, and asks the running host to
reload. The probe settles the vector dimension: measured for a model the
built-in catalogue does not know, checked against the catalogue for one it
does. It costs one API call per model; `--no-verify` skips it.

Then bind a column, as with any model:

```sql
SELECT postvec.enable('docs', 'body',
                      model => 'openai-text-embedding-3-small');
-- NOTICE:  postvec: model "openai-text-embedding-3-small" is served by
--          external provider "openai"; source text from column "body" will
--          be sent to that provider for embedding
```

No restart and no `CREATE EXTENSION`. If the host was not running, the files
are still correct and it reads them at its next start.

## The connector file

One TOML file per provider. The file is the complete serving truth: the host
consults no catalogue at serve time, so a model serves what its descriptor
says.

```toml
# /etc/postvec/providers.d/openai.toml
provider = "openai"      # openai | openrouter | mistral | google | cohere | aws
enabled  = true          # false parses and serves nothing

api_key_file = "/etc/postvec/keys/openai.key"

base_url       = "https://api.openai.com"  # optional: Azure-style fronts, proxies
max_concurrent = 4       # outbound requests in flight for this provider
timeout_ms     = 20000   # per HTTP attempt

[[models]]
name              = "openai-text-embedding-3-small"  # the name SQL uses
provider_model_id = "text-embedding-3-small"         # what the API expects
dim               = 1536   # authoritative; responses are checked against it
max_batch         = 512    # items per request; the host sub-batches to it
max_tokens        = 8191   # advertised as sequence_len
```

Exactly one key source per file. The three are mutually exclusive:

| Field | Use it when | Note |
|---|---|---|
| `api_key_file = "/path"` | The default recommendation | The referenced file must itself be `0600`. A trailing newline is trimmed |
| `api_key_env = "OPENAI_API_KEY"` | Containers, systemd units | Read in the **inference process's** environment. The postmaster's or the unit's, not your shell's |
| `api_key = "sk-..."` | Last resort | The value sits in the 0600 file |

AWS Bedrock replaces `api_key*` with `region` plus either a Bedrock bearer
token (`bearer_token`, `bearer_token_file`, `bearer_token_env`) or a static
SigV4 pair (`access_key_id` and `secret_access_key`, each with its own `_file`
and `_env` variant). The signer takes static credentials only: no session
tokens, no instance profile, no IMDS.

Loading rules:

- `*.toml` in the directory, read in lexicographic order. Dot-files skipped.
- A file that fails to parse, or whose key cannot be resolved, is skipped with
  a logged error. Every other provider and every local model keeps working.
- Duplicate model names across files: the first file wins, with a warning.
- A name that collides with a **loaded local model**: the local model wins, in
  discovery and on the embed path. Local by default is not negotiable at
  runtime.

`postvec setup --embedded` creates `/etc/postvec/providers.d` (0700, cluster
owner), and so does the first `provider add`. The packages do not ship it,
because only the CLI knows which account owns the cluster. Move it with
`postvec.providers_path` (POSTMASTER, restart) or
`setup --embedded --providers-path DIR`.

Nothing deletes that directory for you. `postvec uninstall` reports the
connector files it found and leaves them.

## Commands

| Command | Result | Network |
|---|---|---|
| `provider add TYPE --model ID...` | Write or extend the file, verify, reload the host | One embed per new model, unless `--no-verify` |
| `provider ls` | Providers, key sources, models, dims and whether the host serves them now | Loopback |
| `provider test NAME [--model ID]` | The verification probe on demand | One embed per model probed |
| `provider rm NAME [--model ID]` | Drop one model entry or the whole file, then reload | Loopback |

Target resolution follows the `model` family:

| Target | Behaviour |
|---|---|
| `--path DIR` | Filesystem management of `DIR/providers.d`, or of `DIR` itself when it already is one. This is how `postvec-server` nodes are administered |
| Embedded cluster | Manage `postvec.providers_path`, scan the databases for affected columns, reload the running host |
| Remote cluster | Refused by name. The files live on the nodes, and the message points at `--path` |

`gemini` is accepted as an alias for `google`, and `amazon` for `aws`. The
file always records the canonical name.

`postvec doctor` gains a `provider.*` family, all read-only:

| Check | Reports |
|---|---|
| `provider.directory` | The path and file count. "Does not exist" is a **pass**, because that is the zero-config state. A warning when the directory itself is group- or world-readable |
| `provider.file` | A file that cannot be read or parsed, or whose mode the host will refuse |
| `provider.key-source` | A referenced key file that is missing or too permissive, or a named variable that is absent |
| `provider.descriptors` | A model with no positive `dim`, or a public name that breaks the naming rule |
| `provider.served` | A configured model the running host does not currently serve |

A complaint about an environment variable can be a false alarm. `doctor`
observes its own environment, and the postmaster's is what matters. The check
says so.

## Model names

The public name is `provider-model_id`, lowercased, with `/`, `:`, `.` and
spaces mapped to `-`. These are the ones the CLI can prefill:

| Provider | Model id | Name in SQL | Dim |
|---|---|---|---:|
| openai | `text-embedding-3-small` | `openai-text-embedding-3-small` | 1536 |
| openai | `text-embedding-3-large` | `openai-text-embedding-3-large` | 3072 |
| openai | `text-embedding-ada-002` | `openai-text-embedding-ada-002` | 1536 |
| google | `gemini-embedding-001` | `gemini-embedding-001` | 3072 |
| cohere | `embed-v4.0` | `cohere-embed-v4-0` | 1536 |
| cohere | `embed-english-v3.0` | `cohere-embed-english-v3-0` | 1024 |
| cohere | `embed-multilingual-v3.0` | `cohere-embed-multilingual-v3-0` | 1024 |
| aws | `amazon.titan-embed-text-v2:0` | `aws-titan-embed-text-v2-0` | 1024 |
| mistral | `mistral-embed` | `mistral-mistral-embed` | 1024 |

Any other id works too. The probe measures the dimension, or `--dim` states
it. OpenRouter ids are namespaced, so `--model openai/text-embedding-3-large`
becomes `openrouter-openai-text-embedding-3-large`.

Providers that separate query text from stored text get `search_query` for
`search()` and `search_document` for worker writes, one-shot `embed()` and
migration re-embeds. Nothing to configure. Vectors from local models are
unchanged by this feature.

## Adding a key can change an existing column

:::: warning Read this before adding a provider you already bridge into
[Route resolution](/docs/guides/bridge) prefers a direct embed over a
converter. A column bound to a name that was previously **bridge-only** starts
being embedded directly by the provider the moment the key exists. No SQL
change, no NOTICE, and source text leaves the host on the next worker cycle.

That is the designed upgrade. It is also a privacy event, so `provider add`
scans every configured database for columns bound to the names it is about to
make live, lists them, and requires an explicit acknowledgement. `--yes` does
not answer it; `--acknowledge-in-use` or the typed interactive answer does.
::::

A database the command could not inspect is listed as `UNKNOWN` rather than
assumed clean. Columns whose name a loaded local model also serves are
excluded, because the local model wins and nothing changes for them.

`provider rm` takes the mirror-image acknowledgement: removing a provider
takes the route away from those same columns.

## Local and provider models side by side

Routing has always been per model, not per cluster, so both kinds of column
coexist. A common split is a hosted model where retrieval quality is the
product, and the bundled local model where it is a convenience:

```sql
-- Support articles: quality matters, text may leave the host.
SELECT postvec.enable('articles', 'body',
                      model => 'openai-text-embedding-3-small');
-- NOTICE:  postvec: model "openai-text-embedding-3-small" is served by
--          external provider "openai"; ...

-- Internal notes: stays on the host, no per-token cost.
SELECT postvec.enable('notes', 'body',
                      model => 'sentence-transformers-all-minilm-l6-v2');
```

```sql
SELECT relation, model, dim, pending_jobs FROM postvec.status();
```

:::: tip Expected
```text
 relation |               model               | dim  | pending_jobs
----------+-----------------------------------+------+--------------
 articles | openai-text-embedding-3-small     | 1536 |            0
 notes    | sentence-transformers-all-minilm.. |  384 |            0
```
One worker, one queue, two routes. Each row goes to whichever model its entry
names, and a provider outage leaves the local column filling normally.
::::

The reverse also holds: a provider outage does not stop `search()` on a local
column, and `postvec.search_degrade_to_fts` keeps the provider-backed column
answering lexically while it lasts.

## Adopting a column a provider already embedded

If a table already holds vectors produced by a hosted model, adopt the column
and add the key. Live writes resume with no re-embedding of the corpus:

```bash
sudo postvec provider add openai --model text-embedding-ada-002
```

```sql
SELECT postvec.adopt('docs', 'body',
                     vector_column => 'body_vec',
                     model => 'openai-text-embedding-ada-002');
```

`adopt()`'s `model` is an operator assertion. The column's declared dimension
is the only thing that can contradict it, and nothing can prove which model
produced the stored bytes. A wrong assertion silently embeds queries into the
wrong space. [Adopt existing vectors](/docs/guides/adopt) covers the checks.

Without the key, the same column is still searchable through [bridge
search](/docs/guides/bridge): postvec embeds the query locally and converts
that one vector into the stored space.

## Moving a column off a provider

To stop sending text to a provider, migrate the column to a local model and
then remove the connector.

```sql
SELECT postvec.migrate('docs', 'body',
                       new_model => 'baai-bge-m3') AS migration_id \gset

SELECT state, rows_done, rows_total FROM postvec.migration_status(:migration_id);
-- repeat until state = 'awaiting_finalize'

SELECT postvec.migration_finalize(:migration_id);
```

:::: tip Expected
With a converter installed, the default `convert` strategy translates the
stored vectors and **the source text is never sent anywhere again**. Without
one, `strategy => 'reembed'` embeds the current text with the new model, which
is a local read and a local embed. Either way the column ends on a model the
host serves itself.
::::

Then take the key away:

```bash
sudo postvec provider rm openai --acknowledge-in-use --yes
sudo rm /etc/postvec/keys/openai.key
```

`provider rm` lists any column still routed to the removed model and requires
the acknowledgement before it writes. Removing the key file is a separate,
manual step: nothing in postvec deletes a credential it did not create.

[Migrate models](/docs/guides/migrate) covers the lifecycle, the index rebuild
and the abort path.

## Remote mode and fleets

Each node reads `root/providers.d` (default
`/var/lib/postvec-server/providers.d`, moved with `--providers-path` or
`POSTVEC_SERVER_PROVIDERS_PATH`). Administer each node from the node:

```bash
sudo postvec provider add openai --model text-embedding-3-small \
     --path /var/lib/postvec-server
postvec-server status --fleet
```

**Every node must carry the same connector files.** The rule already applies
to models: postvec round-robins the endpoints it was given, so a provider
configured on three nodes out of four fails intermittently and looks like a
flaky network. Provider-backed entries are labelled in the drift report,
because the fix is a file or a key on that node rather than a model directory.

A node picks changes up on restart, or through its loopback admin port
(`POST 127.0.0.1:22223/admin/providers/reload`), which is what the `provider`
commands try for you.

## Latency and cost

`search()` embeds the query inline. Against a provider that is an internet
round trip, and `postvec.query_timeout_ms` defaults to 2000 ms:

```sql
SET postvec.query_timeout_ms = 10000;   -- USERSET: per session or per role
```

`postvec.search_degrade_to_fts` (default on) turns a timeout into lexical-only
results rather than an error. Worth knowing before you conclude that vector
search stopped working. Applications that already hold a query vector should
call [`search_with_vector()`](/docs/guides/search) and pay the embedding cost
once.

Writes are asynchronous and bounded by `postvec.embed_timeout_ms` (30 s), so
provider latency appears as queue lag, not as failed statements.

Providers bill per token. Two blunt controls: `postvec.max_document_bytes`
(1 MiB) dead-letters an oversized document rather than paying to embed it, and
`max_concurrent` in the connector file caps outbound requests per provider,
which caps the rate at which you can spend.

Provider calls deliberately bypass `postvec.embedded_max_inflight`, the bound
that caps ONNX memory on the database host, and are limited by
`max_concurrent` instead. A slow provider call never serializes behind local
inference. Raising `max_concurrent` after the host started needs a restart for
the full budget; the models serve either way, and the CLI says so.

## When a provider fails

Provider outcomes map onto the retry taxonomy the queue already implements:

| What happened | Class | Effect |
|---|---|---|
| Network error, timeout, HTTP 429 or 5xx | Transient | Queue backoff and retry |
| HTTP 401 or 403, a bad or revoked key | Config | Retried with backoff, and failover-eligible: another node may hold a valid key |
| Unknown model id at the provider | Config | Retried, failover-eligible |
| Input too long | PoisonRow | Batch bisection. The offending row alone goes to `jobs_dead` |
| Other HTTP 400, or a wrong count or dimension | Permanent | The batch is dead-lettered |

An expired key is an operations fault and postvec treats it as one: rows are
retried rather than thrown away on the first 401. Config is not infinite,
though. Like any retryable failure it dead-letters after `postvec.max_retries`
(default 5), so fix the key and re-drive with
[`retry_dead()`](/docs/guides/retry). A migration retries Config indefinitely
instead of failing.

A connector file that fails to load never blocks local models and never blanks
the model list.

## Not supported

| | Why |
|---|---|
| Provider API keys as GUCs, catalog rows or SQL arguments | The product position. Not a missing feature |
| AWS session tokens, instance profiles, IMDS, the credential chain | The signer takes static credentials |
| Vertex AI as a distinct connector, Azure OpenAI beyond `base_url` | Deferred. `base_url` already fronts OpenAI-shaped endpoints |
| Gemini `taskType`, Cohere int8 and binary embeddings | Deferred |
| `Retry-After`-aware backoff, per-provider token budgets | Deferred |
| Reranking providers | A separate roadmap item |

Provider outputs are not covered by the golden-vector suite. Hosted models are
not reproducible, and pinning them would test the provider rather than
postvec.

## Containers

Both images run with no provider configured. To add one, mount a
`providers.d` or name an environment variable the postmaster already has.
[Docker](/docs/install/docker#external-providers) has the mount table and the
permissions a bind mount has to carry.

## Related documentation

- [Search a retired space](/docs/guides/bridge) - route resolution and the bridge
- [Remote inference](/docs/server/) - providers on a `postvec-server` node
- [Embedded vs remote](/docs/concepts/modes) - where inference runs
- [Security](/docs/security) - credentials, grants and worker visibility
- [GUCs](/docs/reference/gucs) - `providers_path` and the timeouts named here
- [CLI](/docs/reference/cli) - the `provider` command family
