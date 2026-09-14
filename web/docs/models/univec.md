---
title: UniVec hosted models
description: Use UniVec for hosted embeddings or direct vector-space conversion while postvec keeps credentials outside PostgreSQL.
---

# UniVec hosted models

UniVec is an [external provider](/docs/models/providers) that serves
hosted embeddings and hosted vector conversion; every other connector
(OpenAI, Cohere, Amazon Bedrock, Gemini, Mistral, OpenRouter) serves
embeddings only.

The `univec` connector serves two model types:

| Model type | Provider route | Data sent |
|---|---|---|
| `embed` | UniVec's OpenAI-compatible embeddings API | Source text or a search query |
| `convert` | UniVec's vector conversion API | Stored vectors |

A `convert` entry is a direct route for `postvec.migrate()` and the
one-shot `postvec.convert()` helper.

Embed and converter entries are independent. They can share one
`univec.toml` file and key.

The API key stays in `providers.d` on the inference host.

## Catalogue listing

UniVec publishes its catalogue at `GET https://api.univec.ai/v1/models`, no
key needed. `provider ls --available` lists it, with each entry's kind and
dimensions, and marks what your `providers.d` already configures:

```bash
postvec provider ls --available univec
postvec provider ls --available univec --to baai-bge-m3     # converters into bge-m3
postvec provider ls --available univec --kind embed --format json
```

::::: tip Expected
```text
univec  (https://api.univec.ai, public catalogue, 18 embed, 98 convert)
  name                                   kind     dim         configured
  baai-bge-m3                            embed    1024        yes (univec-baai-bge-m3)
  snowflake-arctic-embed-l-v2.0          embed    1024        no
  snowflake-arctic-embed-l-v2.0 -> baai-bge-m3   convert  1024->1024  no   cos 0.933
```
:::::

UniVec is the one connector whose model list states dimensions, so
`--available` covers it alone. For every other connector the command
points at `--model`.

## Add hosted embedding models

Paste a key and the CLI does the rest. With no model flags it adds every
embed model in the catalogue, with dimensions and sequence lengths taken
from the catalogue, and no converters:

```bash
sudo postvec provider add univec                        # every embed model
sudo postvec provider add univec --model baai-bge-m3    # one of them
postvec provider ls
sudo postvec doctor --database app
```

Before anything is billed, the command tries an unbilled identity check
against the registry route: a 401 or 403 stops it with nothing spent. That
check is best-effort: a front missing the route, or an outage, is only a
warning. Billing applies to successful inference requests. It then makes
**one** paid embed request against the cheapest model it is adding, checks
the measured width against the catalogue, writes the connector file, reloads
the inference host and refreshes `postvec.models`. A model not in the
catalogue is still accepted with `--model`; its dimension is measured
instead. A catalogue that breaks its contract (an entry of a known kind with
a missing or contradictory dimension) is refused whole, with the row named;
nothing is written or billed.

The route name becomes `univec-baai-bge-m3`. `provider_model_id` keeps
the id accepted by the UniVec API. The route joins space `baai-bge-m3`,
so a column bound to the local model keeps working through whichever
route currently serves that space. The new route joins at the back;
`model prefer` is what moves it first.

::::: tip Expected
`provider ls` reports `univec-baai-bge-m3` as `served`, and the model appears
in `postvec.models` after the refresh performed by `provider add`. The
summary line reads `catalogue: 18 embed, 0 convert added, 0 already present`.
:::::

Bind a column:

```sql
SELECT postvec.enable(
  'public.docs', 'body',
  model => 'univec-baai-bge-m3'
);
-- NOTICE: postvec: model "univec-baai-bge-m3" is served by external
--         provider "univec"; source text from column "body" will be sent
--         to that provider for embedding
```

Worker writes use `search_document`. `search()` uses `search_query`. UniVec
applies the corresponding model prompt when the model defines one.

## Configure hosted conversion

Conversion routes are opt-in, one flag. Name the model you are migrating to
and every catalogue converter into it is added, or name one pair:

```bash
sudo postvec provider add univec --convert-to baai-bge-m3
sudo postvec provider add univec --convert snowflake-arctic-embed-l-v2.0:baai-bge-m3 \
  --converter-name univec-convert-snowflake-to-bge-m3
sudo postvec provider add univec --convert-from snowflake-arctic-embed-l-v2.0
sudo postvec provider add univec --all-converters
```

Source and target names, both dimensions and both name vocabularies come
from the catalogue. The command checks that a converter's stated widths
agree with the listed embed models of the same names, sends **one** vector
through the conversion API for one of the routes it adds (no embed request
for a convert-only add), writes the connector file, reloads the inference
host and refreshes `postvec.models`. Selectors combine; a selection nothing
matches is refused with the catalogue's actual targets or sources listed.

The single-pair example exposes a direct Snowflake Arctic to BGE-M3
conversion. The target remains a local `baai-bge-m3` model after migration:
`--convert-to` adds no embed model, and search after `migrate()` still needs
a local embed of the target (or a bridge). The command writes this model
entry:

```toml
# /etc/postvec/providers.d/univec.toml
provider = "univec"
api_key_file = "/etc/postvec/keys/univec.key"

[[models]]
name              = "univec-convert-snowflake-to-bge-m3"
kind              = "convert"
provider_source_id = "snowflake-arctic-embed-l-v2.0"
provider_model_id  = "baai-bge-m3"
source_model       = "snowflake-arctic-embed-l-v2.0"
target_model       = "baai-bge-m3"
source_dim         = 1024
dim                = 1024
```

The two name pairs have different jobs:

| Fields | Meaning |
|---|---|
| `provider_source_id`, `provider_model_id` | Names sent to UniVec's `/v1/convert` route |
| `source_model`, `target_model` | Names used by postvec route resolution |
| `source_dim`, `dim` | Input and output dimensions |

Use the exact public model names and dimensions for the pair. A mismatch is a
whole-file load error, or a request-time dimension error if the declared
dimension and the provider response disagree.

### Manual entries

When the catalogue is unreachable, or for a model UniVec has not listed yet,
the explicit flags still work (hidden from `--help`):

```bash
sudo postvec provider add univec --no-catalog --model my-model --dim 1024 --no-verify
sudo postvec provider add univec \
  --convert-source snowflake-arctic-embed-l-v2.0 --convert-target baai-bge-m3 \
  --source-model snowflake-arctic-embed-l-v2.0 --target-model baai-bge-m3 \
  --source-dim 1024
```

`--dim` is then the target dimension; the probe measures it when omitted,
and `--no-verify` requires it.

Keep the file at `0600` in its existing `0700` directory. Reload the embedded
host and refresh the database cache only after a hand edit:

```bash
curl --fail --silent --show-error \
  --request POST http://127.0.0.1:33434/admin/providers/reload
```

Then refresh the database cache:

```sql
SELECT postvec.refresh_models();

SELECT name, model_type, source_model, target_model,
       source_dim, target_dim,
       raw->'extra'->>'provider' AS provider
  FROM postvec.models
 WHERE name = 'univec-convert-snowflake-to-bge-m3';
```

::::: tip Expected

```text
                    name                    | model_type |             source_model             | target_model | source_dim | target_dim | provider
--------------------------------------------+------------+--------------------------------------+--------------+------------+------------+----------
 univec-convert-snowflake-to-bge-m3         | convert    | snowflake-arctic-embed-l-v2.0       | baai-bge-m3 |       1024 |       1024 | univec
```

The exact spacing depends on `psql`.
:::::

`provider test univec` verifies each entry with its matching API operation.
An embed entry sends one text input. A converter sends one source vector and
checks the target dimension. Each probe is a billable request.

## Migrate through the hosted converter

To keep the stored corpus and convert only each query, use
[search a retired space](/docs/guides/bridge). Embed-bridge uses local
models. The hosted converter below is for a later
[`migrate()`](/docs/guides/migrate), once that search is working.

The source column must currently name the converter's `source_model`. The
target model must also have a direct embedding route for new writes after the
swap. In this example both embedding models are local; only the stored-vector
conversion is hosted.

Install and activate the target first if it is not already served:

```bash
sudo postvec model pull baai-bge-m3 --yes
sudo postvec model activate baai-bge-m3 --yes
```

```sql
SELECT postvec.migrate(
  'public.docs', 'body',
  new_model => 'baai-bge-m3',
  strategy => 'convert'
) AS migration_id \gset

SELECT resolved_via, state, rows_done, rows_total
  FROM postvec.migration_status(:migration_id);
```

On the extension, `migrate()` emits a NOTICE before work starts:

```text
NOTICE: postvec: conversion route "univec-convert-snowflake-to-bge-m3" is served by external provider "univec"; the stored vectors of column "body" will be sent to that provider for conversion
```

`resolved_via` records the selected route. A hosted converter is a direct
route, so the extension reports:

```json
{"kind":"direct","model":"univec-convert-snowflake-to-bge-m3"}
```

The SQL mirror on managed PostgreSQL, where the extension cannot load,
reports the same route as `kind: "convert"`:

```json
{"kind":"convert","model":"univec-convert-snowflake-to-bge-m3"}
```

Continue with `migration_finalize()` when the state reaches
`awaiting_finalize`. The [migration guide](/docs/guides/migrate) covers the
column swap and index rebuild.

## Route constraints

A provider-backed converter is a direct conversion route for
`migrate()` and `convert()`. Embed-bridge resolution runs inside the
local inference engine and uses local models. Load a local converter
when embed-bridge is required.

When several direct converters declare the same source and target, postvec
prefers a local converter, then the first hosted one by name.
`migration_status()` shows the chosen name in `resolved_via`. Avoid duplicate
pairs unless that ordering is intentional.

## Credentials, billing and failures

`postvec login` and `provider add univec` configure separate uses of a UniVec
API key:

- `postvec login` authenticates model-registry downloads for the CLI user.
- `provider add univec` makes billable inference requests from the inference
  host.

`provider add univec` can reuse the `postvec login` key. An interactive run
that finds one offers it (default no); scripts opt in with
`--api-key-from-login`. The key is **copied** to
`/etc/postvec/keys/<name>.key` (or `<server-root>/keys/<name>.key`, one per
connector file, `univec.key` by default) and referenced from the connector
file. `postvec logout` leaves serving keys in place.

Rules of a copied or rotated key:

- Two connector files never share one credential.
- An existing key file holding a different key is never replaced.
  `--replace-copied-key` rotates a key this command copied for exactly this
  connector, verifying the new key before the old one goes and restoring it
  if the write fails.
- Rerunning with the same copied key changes nothing and sends nothing.
- A key is installed only if the destination is still exactly what the check
  saw, including its mode, owner and link count.
- A copied key is never deleted by this command: if the connector write
  fails, the key is kept and named, and a rerun reuses it. A rotated key is
  restored only if the file is still the one this command installed.
- In every partial outcome the run ends incomplete, reports nothing as
  written, and says exactly what to check.

The plan states how many billable probe attempts a run makes: one per kind
added, and on a key rotation one existing embed and one existing converter
(UniVec keys are account-wide; other connectors re-verify every route).
`POSTVEC_API_KEY` wins over stored logins; when root's store and the
invoking user's store hold different keys, an interactive run asks which and
a scripted run refuses. A dedicated inference key keeps billing and rotation
separate from downloads.

A key with a zero spending limit can read the private model catalogue but
cannot serve hosted embeddings or conversions. UniVec returns HTTP 402 when
the account has no available credit; `provider add` says so when the
identity check passes (or is unavailable) and the billed probe fails. postvec classifies 401, 402 and 403 as
configuration failures. Queue work retries up to `postvec.max_retries`, then
moves to `postvec.jobs_dead`. Migrations keep retrying configuration failures.

Provider `max_concurrent` limits requests in flight. Billing and spending
limits are enforced by the UniVec account.

## On postvec-server

Put the same `univec.toml` on every [postvec-server](/docs/server/)
under `$root/providers.d`. Reload through each loopback admin port:

```bash
curl --fail --silent --show-error \
  --request POST http://127.0.0.1:22223/admin/providers/reload
postvec-server status --fleet
```

A node that lacks the converter fails requests routed to it. The fleet report
labels provider-backed embed and convert entries.

- [External providers](/docs/models/providers)
- [OpenAI](/docs/models/openai)
- [Cohere](/docs/models/cohere)
- [Amazon Bedrock](/docs/models/aws)
- [Gemini](/docs/models/gemini)
- [Mistral](/docs/models/mistral)
- [OpenRouter](/docs/models/openrouter)
- [Connector files](/docs/models/providers-file)
- [postvec-server](/docs/server/)
- [Search a retired space](/docs/guides/bridge)
- [Change the stored model](/docs/guides/migrate)
- [Security](/docs/security)
