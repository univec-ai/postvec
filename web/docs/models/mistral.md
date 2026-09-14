---
title: Mistral
description: Bind a postvec column to Mistral embeddings. The API key stays in providers.d on the inference host.
---

# Mistral

Hosted embeddings through Mistral's API. The key stays in `providers.d`
on the inference host (the database machine in embedded mode, each
[postvec-server](/docs/server/) in remote mode).

## 1. Add the provider

```bash
sudo postvec provider add mistral --model mistral-embed
postvec provider ls
sudo postvec doctor --database app
```

The command prompts for the key without echo, writes
`/etc/postvec/providers.d/mistral.toml` (`0600`), probes the key with one
embed call, reloads the host and refreshes `postvec.models`.

::::: tip Expected
```text
providers.d: /etc/postvec/providers.d
mistral  (mistral, key: inline (redacted))
  mistral-mistral-embed                        dim 1024   served
```
`doctor` exits 0. The name is in `postvec.models`.
:::::

Built-in catalogue names:

| `--model` | Name in SQL | Dim |
|---|---|---:|
| `mistral-embed` | `mistral-mistral-embed` | 1024 |

Pass `--api-key-file PATH` or `--api-key-env VAR` instead of the prompt.
With `--api-key-file` the key stays in that file and `provider ls` reports
`file:PATH` instead of inline.

Any other Mistral embedding id works. The probe measures the dimension,
or `--dim` states it with `--no-verify`. `--base-url` fronts a
Mistral-compatible origin.

## 2. Bind a column

```sql
SELECT postvec.enable('docs', 'body',
                      model => 'mistral-mistral-embed');
-- NOTICE:  postvec: model "mistral-mistral-embed" is served by
--          external provider "mistral"; source text from column "body" will
--          be sent to that provider for embedding
```

::::: tip Expected
`docs.body_semantic` is `vector(1024)`. `pending_jobs` returns to 0.
:::::

```sql
SET postvec.query_timeout_ms = 10000;

SELECT postvec.create_vector_index('docs', 'body');
SELECT * FROM postvec.search('docs', 'body', 'revenue outlook', limit_n => 5);
```

## On postvec-server

Run the add on a [postvec-server](/docs/server/) node. The CLI finds no
local cluster there and writes `<server-root>/providers.d`, the directory
that node serves from:

```bash
sudo postvec provider add mistral --model mistral-embed \
     --acknowledge-in-use --yes
```

Copy the same file onto every node, then reload each host. [External
providers](/docs/models/providers) covers keys, failures and moving a
column off the provider.

- [External providers](/docs/models/providers)
- [Connector files](/docs/models/providers-file)
- [postvec-server](/docs/server/)
