---
title: Amazon Bedrock
description: Bind a postvec column to Amazon Titan embeddings on Bedrock. Credentials stay in providers.d on the inference host.
---

# Amazon Bedrock

Hosted embeddings through Amazon Titan on Bedrock. The connector speaks
the Titan body shape (`inputText` in, `embedding` out). Credentials stay
in `providers.d` on the inference host (the database machine in embedded
mode, each [postvec-server](/docs/server/) in remote mode).

`amazon` is accepted as an alias for `aws`. The file records `aws`.

## 1. Add the provider

Bedrock needs a region and either a bearer token or a static SigV4 pair.
The CLI probe uses the bearer-token variant:

```bash
sudo postvec provider add aws \
  --model amazon.titan-embed-text-v2:0 \
  --region us-east-1
postvec provider ls
sudo postvec doctor --database app
```

The command prompts for the token without echo, writes
`/etc/postvec/providers.d/aws.toml` (`0600`), probes Titan with one
embed call, reloads the host and refreshes `postvec.models`. Pass
`--api-key-file PATH` or `--api-key-env VAR` instead of the prompt; with
a file the token stays there and `provider ls` reports
`bearer file:PATH`. `--base-url` is refused: the region is the whole
endpoint (`bedrock-runtime.<region>.amazonaws.com`).

::::: tip Expected
```text
providers.d: /etc/postvec/providers.d
aws  (aws, key: bearer inline (redacted))
  aws-titan-embed-text-v2-0                    dim 1024   served
```
`doctor` exits 0. The name is in `postvec.models`.
:::::

Built-in catalogue names:

| `--model` | Name in SQL | Dim |
|---|---|---:|
| `amazon.titan-embed-text-v2:0` | `aws-titan-embed-text-v2-0` | 1024 |
| `amazon.titan-embed-text-v1` | `aws-titan-embed-text-v1` | 1536 |

Bedrock `InvokeModel` sends one text per request, so `max_batch` is 1.
On a Titan-backed column, lower `postvec.batch_size` rather than raising
the timeout.

For a SigV4 pair, write the file by hand (or from a template) with
`access_key_id_file` and `secret_access_key_file`, then:

```bash
sudo postvec provider add aws \
  --model amazon.titan-embed-text-v2:0 \
  --region us-east-1 \
  --no-verify
```

`--no-verify` is required for SigV4: the CLI probe covers bearer tokens
only. Confirm with `provider ls` and a first write. The signer takes
static credentials (no session tokens, no instance profile, no IMDS).

## 2. Bind a column

```sql
SELECT postvec.enable('docs', 'body',
                      model => 'aws-titan-embed-text-v2-0');
-- NOTICE:  postvec: model "aws-titan-embed-text-v2-0" is served by
--          external provider "aws"; source text from column "body" will
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
sudo postvec provider add aws \
  --model amazon.titan-embed-text-v2:0 \
  --region us-east-1 \
  --acknowledge-in-use --yes
```

Copy the same file onto every node, then reload each host. [External
providers](/docs/models/providers) covers keys, failures and moving a
column off the provider. [Connector files](/docs/models/providers-file)
has the TOML for a bearer token and for a SigV4 pair.

- [External providers](/docs/models/providers)
- [Connector files](/docs/models/providers-file)
- [postvec-server](/docs/server/)
