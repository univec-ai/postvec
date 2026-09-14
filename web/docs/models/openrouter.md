---
title: OpenRouter
description: Bind a postvec column to an OpenRouter embedding model. The API key stays in providers.d on the inference host.
---

# OpenRouter

Hosted embeddings through OpenRouter's OpenAI-compatible API. Use it for
a namespaced model id (`openai/text-embedding-3-small`, a self-hosted
front, another vendor on OpenRouter). The key stays in `providers.d` on
the inference host (the database machine in embedded mode, each
[postvec-server](/docs/server/) in remote mode).

OpenRouter ids are namespaced, so the route name derives from the full id:
`--model openai/text-embedding-3-small` becomes
`openrouter-openai-text-embedding-3-small` in space
`openai-text-embedding-3-small`, and `google/gemini-embedding-001` joins
space `gemini-embedding-001`. A column bound to the OpenAI-hosted name is
served by this route when no OpenAI route serves that space, without a
migration.

## 1. Add the provider

```bash
sudo postvec provider add openrouter \
  --model openai/text-embedding-3-small
postvec provider ls
sudo postvec doctor --database app
```

The command prompts for the key without echo, writes
`/etc/postvec/providers.d/openrouter.toml` (`0600`), probes the key with
one embed call, reloads the host and refreshes `postvec.models`. The
probe measures the dimension.

::::: tip Expected
```text
providers.d: /etc/postvec/providers.d
openrouter  (openrouter, key: inline (redacted))
  openrouter-openai-text-embedding-3-small     dim 1536   served
```
`doctor` exits 0. The name is in `postvec.models`.
:::::

Pass `--api-key-file PATH` or `--api-key-env VAR` instead of the prompt.
With `--api-key-file` the key stays in that file and `provider ls` reports
`file:PATH` instead of inline.

`--base-url` fronts a different OpenAI-shaped origin. Two fronts need
two files: pass `--name STEM` so the second writes `STEM.toml`.

`--no-verify` requires `--dim` for an unlisted id.

## 2. Bind a column

```sql
SELECT postvec.enable('docs', 'body',
                      model => 'openai-text-embedding-3-small');
-- NOTICE:  postvec: model "openai-text-embedding-3-small" is served by
--          external provider "openrouter"; source text from column "body" will
--          be sent to that provider for embedding
```

The NOTICE names the model you bound, which is the shared space name. The
route doing the work is `openrouter-openai-text-embedding-3-small`.

::::: tip Expected
`docs.body_semantic` is `vector(1536)` for this model.
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
sudo postvec provider add openrouter \
  --model openai/text-embedding-3-small \
  --acknowledge-in-use --yes
```

Copy the same file onto every node, then reload each host. [External
providers](/docs/models/providers) covers keys, failures and moving a
column off the provider.

- [External providers](/docs/models/providers)
- [Connector files](/docs/models/providers-file)
- [postvec-server](/docs/server/)
