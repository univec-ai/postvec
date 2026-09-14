---
title: Gemini
description: Bind a postvec column to Gemini embeddings. The API key stays in providers.d on the inference host.
---

# Gemini

Hosted embeddings through Google's `gemini-embedding-001` model. The key
stays in `providers.d` on the inference host (the database machine in
embedded mode, each [postvec-server](/docs/server/) in remote mode).

`gemini` is accepted as an alias for `google`. The file records `google`.
The loader accepts this one Gemini model id.

Worker writes send `search_document`. `search()` sends `search_query`.

## 1. Add the provider

```bash
sudo postvec provider add google --model gemini-embedding-001
postvec provider ls
sudo postvec doctor --database app
```

The command prompts for the key without echo, writes
`/etc/postvec/providers.d/google.toml` (`0600`), probes the key with one
embed call, reloads the host and refreshes `postvec.models`.

::::: tip Expected
```text
providers.d: /etc/postvec/providers.d
google  (google, key: inline (redacted))
  gemini-embedding-001                         dim 3072   served
```
`doctor` exits 0. The name is in `postvec.models`.
:::::

Pass `--api-key-file PATH` or `--api-key-env VAR` instead of the prompt.
With `--api-key-file` the key stays in that file and `provider ls` reports
`file:PATH` instead of inline.

Built-in catalogue names:

| `--model` | Name in SQL | Dim |
|---|---|---:|
| `gemini-embedding-001` | `gemini-embedding-001` | 3072 |

`dim` may be any width in Google's documented range `128..=3072`. Pass
`--dim 768` (for example) to request `outputDimensionality`. Google
returns a reduced vector unnormalised, so postvec normalises it.

## 2. Bind a column

```sql
SELECT postvec.enable('docs', 'body',
                      model => 'gemini-embedding-001');
-- NOTICE:  postvec: model "gemini-embedding-001" is served by
--          external provider "google"; source text from column "body" will
--          be sent to that provider for embedding
```

::::: tip Expected
`docs.body_semantic` is `vector(3072)` at the default width.
`pending_jobs` returns to 0.
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
sudo postvec provider add google --model gemini-embedding-001 \
     --acknowledge-in-use --yes
```

Copy the same file onto every node, then reload each host. [External
providers](/docs/models/providers) covers keys, failures and moving a
column off the provider.

- [External providers](/docs/models/providers)
- [Connector files](/docs/models/providers-file)
- [postvec-server](/docs/server/)
