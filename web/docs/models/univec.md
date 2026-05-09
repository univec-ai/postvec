---
title: UniVec hosted models
description: Use UniVec for hosted embeddings or direct vector-space conversion while postvec keeps credentials outside PostgreSQL.
---

# UniVec hosted models

The `univec` connector serves two model types:

| Model type | Provider route | Data sent |
|---|---|---|
| `embed` | UniVec's OpenAI-compatible embeddings API | Source text or a search query |
| `convert` | UniVec's vector conversion API | Stored vectors |

Other external connectors serve embeddings only. A UniVec connector can
also expose a direct converter to `postvec.migrate()` and the one-shot
`postvec.convert()` helper.

Embed and converter entries are independent. They can share one
`univec.toml` file and key.

The API key stays in `providers.d` on the inference host. It does not enter a
GUC, catalog row or SQL argument.

## Add a hosted embedding model

The CLI creates an embed entry, sends one verification request and measures
the model's native dimension:

```bash
sudo postvec provider add univec --model baai-bge-m3
postvec provider ls
sudo postvec doctor --database app
```

The model id becomes `univec-baai-bge-m3` in SQL. `provider_model_id` keeps
the id accepted by the UniVec API.

::::: tip Expected
`provider ls` reports `univec-baai-bge-m3` as `served`, and the model appears
in `postvec.models` after the refresh performed by `provider add`.
:::::

Bind a column with the normal SQL surface:

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

Use converter mode to add a `kind = "convert"` entry to `univec.toml`:

```bash
sudo postvec provider add univec \
  --convert-source snowflake-arctic-embed-l-v2.0 \
  --convert-target baai-bge-m3 \
  --source-model snowflake-arctic-embed-l-v2.0 \
  --target-model baai-bge-m3 \
  --source-dim 1024 \
  --converter-name univec-convert-snowflake-to-bge-m3
```

The command sends one vector through the conversion API, measures the target
dimension, writes the connector file, reloads the inference host and refreshes
`postvec.models`. Pass `--dim` to check a known target dimension. With
`--no-verify`, `--dim` is required.

This example exposes a direct Snowflake Arctic to BGE-M3 conversion. The
target remains a local `baai-bge-m3` model after migration. The command writes
this model entry:

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
whole-file load error, or a request-time dimension error if the declaration
does not match the provider response.

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

`migrate()` emits a NOTICE before work starts:

```text
NOTICE: postvec: conversion route "univec-convert-snowflake-to-bge-m3" is served by external provider "univec"; the stored vectors of column "body" will be sent to that provider for conversion
```

`resolved_via` records the selected route:

```json
{"kind":"direct","model":"univec-convert-snowflake-to-bge-m3"}
```

Continue with `migration_finalize()` when the state reaches
`awaiting_finalize`. The [migration guide](/docs/guides/migrate) covers the
column swap and index rebuild.

## Route constraints

A provider-backed converter is a direct conversion route only. It cannot act
as the converter in an `embed-bridge` route for writes or search. Embed-bridge
resolution runs inside the local inference engine, which cannot resolve
provider gateway entries. Load a local converter when embed-bridge is
required.

When several direct converters declare the same source and target, postvec
selects the first converter name in lexical order. `migration_status()` shows
the chosen name in `resolved_via`. Avoid duplicate pairs unless that ordering
is intentional.

## Credentials, billing and failures

`postvec login` and `provider add univec` configure separate uses of a UniVec
API key:

- `postvec login` authenticates model-registry downloads for the CLI user.
- `provider add univec` makes billable inference requests from the inference
  host.

A key with a zero spending limit can read the private model catalogue but
cannot serve hosted embeddings or conversions. UniVec returns HTTP 402 when
the account has no available credit. postvec classifies 401, 402 and 403 as
configuration failures. Queue work retries up to `postvec.max_retries`, then
moves to `postvec.jobs_dead`. Migrations keep retrying configuration failures.

Provider `max_concurrent` limits requests in flight. It is not a spending
limit. Billing and spending limits are enforced by the UniVec account.

## Remote nodes

For `postvec-server`, put the same `univec.toml` on every node under
`$root/providers.d`. Reload through each node's loopback admin port:

```bash
curl --fail --silent --show-error \
  --request POST http://127.0.0.1:22223/admin/providers/reload
postvec-server status --fleet
```

A node that lacks the converter fails requests routed to it. The fleet report
labels provider-backed embed and convert entries.

## Related documentation

- [External providers](/docs/models/providers) - common setup and failure handling
- [Connector files](/docs/models/providers-file) - schema and loading rules
- [Migrate models](/docs/guides/migrate) - migration lifecycle
- [Search a retired space](/docs/guides/bridge) - local bridge requirements
- [Security](/docs/security) - text, vector and credential boundaries
