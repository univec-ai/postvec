---
title: OpenAI
description: Bind a postvec column to OpenAI embeddings. The API key stays in providers.d on the inference host.
---

# OpenAI

Hosted embeddings through OpenAI's API. The key stays in `providers.d`
on the inference host (the database machine in embedded mode, each
[postvec-server](/docs/server/) in remote mode).

## 1. Add the provider

```bash
sudo postvec provider add openai --model text-embedding-3-small
postvec provider ls
sudo postvec doctor --database app
```

In a container: `docker exec -it postvec postvec provider add openai --model text-embedding-3-small`.

The command prompts for the key without echo, writes
`/etc/postvec/providers.d/openai.toml` (`0600`), probes the key with one
embed call, reloads the host and refreshes `postvec.models`.

::::: tip Expected
```text
providers.d: /etc/postvec/providers.d
openai  (openai, key: file:/etc/postvec/keys/openai.key)
  openai-text-embedding-3-small                dim 1536   served
```
`doctor` exits 0. The name is in `postvec.models`. No PostgreSQL restart.
:::::

Built-in catalogue names:

| `--model` | Name in SQL | Dim |
|---|---|---:|
| `text-embedding-3-small` | `openai-text-embedding-3-small` | 1536 |
| `text-embedding-3-large` | `openai-text-embedding-3-large` | 3072 |
| `text-embedding-ada-002` | `openai-text-embedding-ada-002` | 1536 |

Any other OpenAI embedding id works. The probe measures the dimension, or
`--dim` states it with `--no-verify`. Repeat `--model` to add several in
one file.

Pass `--api-key-file`, `--api-key-env OPENAI_API_KEY` or `--key-stdin`
instead of the prompt. `--base-url` fronts an OpenAI-compatible origin
(Azure's `/openai/v1` API is this shape).

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

::::: tip Expected
`pending_jobs` returns to 0. `docs.body_semantic` is `vector(1536)`.
`dead_jobs` is 0.
:::::

```sql
SET postvec.query_timeout_ms = 10000;

SELECT postvec.create_vector_index('docs', 'body');

SELECT * FROM postvec.search('docs', 'body', 'revenue outlook', limit_n => 5);
```

To keep an existing ada-002 column and search it through a local
converter, [search a retired space](/docs/guides/bridge). Adding this
key later creates a direct OpenAI route for that name; `provider add`
lists affected columns and asks first.

## On postvec-server

```bash
sudo postvec provider add openai --model text-embedding-3-small \
     --acknowledge-in-use --yes
```

Copy the same file onto every node. [External providers](/docs/models/providers)
covers keys, failures and moving a column off the provider.

- [External providers](/docs/models/providers)
- [Connector files](/docs/models/providers-file)
- [postvec-server](/docs/server/)
