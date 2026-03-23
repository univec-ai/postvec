---
title: Connector files
description: providers.d format, key sources, loading rules, model names and doctor checks.
---

# Connector files

One TOML file per provider. The host serves what the file declares. It
does not consult a catalogue at serve time.

Walkthrough: [external providers](/docs/models/providers).

## File format

```toml
# /etc/postvec/providers.d/openai.toml
provider = "openai"      # openai | openrouter | mistral | google | cohere | aws
enabled  = true          # false parses and serves nothing

api_key_file = "/etc/postvec/keys/openai.key"

base_url       = "https://api.openai.com"  # optional origin
max_concurrent = 4       # outbound requests in flight
timeout_ms     = 20000   # per HTTP attempt

[[models]]
name              = "openai-text-embedding-3-small"  # SQL name
provider_model_id = "text-embedding-3-small"         # API id
dim               = 1536   # responses are checked against this
max_batch         = 512    # host sub-batches to this
max_tokens        = 8191   # advertised as sequence_len
```

`base_url` replaces the **origin only**. Each connector appends its own
fixed path (`/v1/embeddings` for the OpenAI-shaped APIs, `/v2/embed` for
Cohere, Gemini's batch path). It must be an absolute `http`/`https` URL
with a host, and must not carry a query string, a fragment, userinfo or
a trailing `/`.

Use it for a reverse proxy, a gateway or a self-hosted OpenAI-compatible
endpoint. Azure OpenAI classic needs a deployment path and an
`api-version` query, which this schema cannot write. Azure's `v1` API
under `/openai/v1` works as a `base_url`.

Plain `http` to anything but loopback is refused unless the file opts in:

```toml
base_url = "http://vllm.internal:8000"
allow_insecure_transport = true   # required, and never the default
```

A file that opts in still warns on every `doctor` run. Loopback needs
no opt-in.

Referenced secret paths must be **absolute**. The CLI, the embedded
launcher and `postvec-server` all resolve them and none share a working
directory.

## Key sources

Exactly one of three, mutually exclusive:

| Field | Use it when | Note |
|---|---|---|
| `api_key_file = "/path"` | The usual choice | An absolute path to a regular file (not a symlink, not hard-linked) at `0600`. A trailing newline is trimmed |
| `api_key_env = "OPENAI_API_KEY"` | Containers, systemd units | Resolved in the **inference process's** environment. The postmaster's or the unit's, not your shell's |
| `api_key = "sk-..."` | Last resort | The value sits in the 0600 file |

`api_key_file` is opened with `O_NOFOLLOW` and the mode is checked on
the descriptor the host actually opens. Kubernetes projected secret
volumes are symlinks into a `..data` directory and are mounted
world-readable, so they cannot be referenced this way. Use
`api_key_env` there.

## AWS Bedrock

AWS replaces `api_key*` with `region` plus **either** a Bedrock bearer
token (`bearer_token`, `bearer_token_file`, `bearer_token_env`) **or** a
static SigV4 pair (`access_key_id` and `secret_access_key`, each with
its own `_file` / `_env` variant):

```toml
provider = "aws"
region   = "us-east-1"
bearer_token_file = "/etc/postvec/keys/bedrock.key"

[[models]]
name              = "aws-titan-embed-text-v2-0"
provider_model_id = "amazon.titan-embed-text-v2:0"
dim               = 1024
max_batch         = 1        # Titan invokes one text per request
```

The signer takes static credentials only: no session tokens, no instance
profile, no IMDS.

A few AWS specifics:

- The connector speaks the **Amazon Titan** embedding schema. Other
  vendors hosted on Bedrock use different body shapes and are not in
  the built-in catalogue.
- `region` is the whole endpoint. The connector builds
  `bedrock-runtime.<region>.amazonaws.com` from it, so `base_url` does
  not apply to `aws` and `provider add` refuses the flag. A region is
  restricted to `[a-z0-9-]`.
- Bedrock's `InvokeModel` does not batch, so `max_batch` is 1. On a
  Titan-backed column, lower `postvec.batch_size` rather than raising
  the timeout.
- `provider add` and `provider test` probe the **bearer-token**
  variant. A SigV4 file is served normally but cannot be probed from
  the CLI. Use `--no-verify` and confirm with `provider ls` plus a
  first write.

## Gemini and Cohere dimensions

Where a provider documents it, `dim` is also *requested*: Gemini
receives `outputDimensionality` and Cohere v4 `output_dimension`.

- **Cohere v4** produces 256, 512, 1024 or 1536 and nothing else. A
  descriptor asking for another width is refused at load.
- **Cohere v3** widths are fixed per model (`embed-english-v3.0` and
  `embed-multilingual-v3.0` are 1024, the `-light-` pair 384) and are
  checked exactly.
- **Gemini ships one model**: `gemini-embedding-001`, with `dim`
  anywhere in Google's documented `128..=3072`. Any other Gemini model
  id is refused at load. A reduced Gemini vector is not normalised by
  Google, so postvec renormalises it.

Providers that separate query text from stored text send `search_query`
for `search()` and `search_document` for worker writes, `embed()` and
migration re-embeds.

## Loading rules

- `*.toml` in the directory, read in lexicographic order. Dot-files
  skipped.
- A file that fails to parse, or whose key cannot be resolved, is
  skipped with a logged error. Every other provider and every local
  model keeps working.
- A file is checked as a **whole**. An unknown `provider` type, a
  connector with no credential, an `aws` file with no `region`, an
  implausible `dim` or an unusable `base_url` skips the entire file,
  including its other models.
- The schema is per connector. A field the chosen connector does not
  read is refused rather than ignored.
- `enabled = false` parses and serves nothing.
- A public name may appear once. Two entries in one file: the file is
  refused. Two files claiming the same name: neither serves until one
  drops it.
- A loaded local model keeps a colliding name, in `/config` and on the
  embed path.
- The directory is bounded as a whole: at most 32 connector files, 256
  provider models and 256 total `max_concurrent` across every file. A
  single file is at most 256 KiB and a referenced secret at most
  16 KiB. Breaking a directory-wide ceiling fails the whole scan.

`provider add` and `provider rm` validate the directory as it would be
after the write. A file the host would skip as a whole is refused
before it is written, so existing models on that provider stay up.

## The directory

`postvec setup --embedded` creates `/etc/postvec/providers.d` (0700,
owned by the cluster owner), and so does the first `provider add`. The
packages do not ship it, because only the CLI knows which account owns
the cluster. An absent directory is the default.

The directory must not be group- or world-writable, must be owned by
the account that reads it (or by root) and every ancestor must be one
only root or that same account can rewrite. Anyone who can write there
can drop in a connector file and choose where this host sends source
text. The serving host refuses those cases outright, `provider add`
refuses to write into one and `doctor` fails `provider.directory`.
Read and execute bits only disclose which providers are configured;
those stay a warning.

`provider add` and `provider rm` take an advisory lock on the directory
for the whole read-modify-write, so two administrators running them at
once cannot lose one another's change.

Change the location with `postvec.providers_path` (POSTMASTER, restart)
or `setup --embedded --providers-path DIR`.

`postvec uninstall` reports connector files and leaves them. Package
removal does the same.

## Model names

The public name is `provider-model_id`, lowercased, with `/`, `:`, `.`
and spaces mapped to `-`. These are the ones the CLI can prefill:

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
| aws | `amazon.titan-embed-text-v1` | `aws-titan-embed-text-v1` | 1536 |
| mistral | `mistral-embed` | `mistral-mistral-embed` | 1024 |

Any other id works too. The probe measures the dimension, or `--dim`
states it. OpenRouter ids are namespaced, so
`--model openai/text-embedding-3-large` becomes
`openrouter-openai-text-embedding-3-large`.

For an id the catalogue does not know, the name is derived
mechanically: lowercase, and every character outside `[a-z0-9._-]`
becomes `-`. `provider_model_id` keeps the id exactly as the API
expects it.

`gemini` is accepted as an alias for `google`, and `amazon` for `aws`.
The file always records the canonical name. An alias never changes
what you type in SQL.

## Commands

| Command | Result | Network |
|---|---|---|
| `provider add TYPE --model ID...` | Write or extend the file, verify, reload the host, refresh `postvec.models` | One embed per verified model, unless `--no-verify` |
| `provider ls` | Providers, key sources, models, dims and whether the host serves them now | Loopback |
| `provider test NAME [--model ID]` | The verification probe on demand | One embed per model probed |
| `provider rm NAME [--model ID]` | Drop one model entry or the whole file, then reload | Loopback |

Useful options on `add`:

| Option | Effect |
|---|---|
| `--name STEM` | Write `STEM.toml` instead of the type's name. Two OpenAI-compatible endpoints, two files |
| `--api-key-file P` / `--api-key-env VAR` / `--key-stdin` | Choose the key source. Without any of them and with a TTY, a hidden prompt asks |
| `--base-url URL` | Gateways, Azure-shaped fronts or a mock server. Not accepted for `aws` |
| `--region` | Required for `aws`, and only valid there |
| `--dim N` | For a single model the built-in catalogue does not know, together with `--no-verify` |
| `--no-verify` | Skip the live probe |
| `--dry-run` | Print the plan and change nothing |

The plan is confirmed before the probe. Declining it, or failing the
in-use acknowledgement, makes no API call.

`add` verifies the models it adds, plus (when the run changes the
connector itself: a new key source, a moved `base_url` or `region`)
every model the file already declares.

Target resolution:

| Target | Behaviour |
|---|---|
| `--path DIR` | Filesystem management of `DIR/providers.d`, or of `DIR` itself when it already is one. `DIR` must already exist. New files inherit its owner. `--acknowledge-in-use` is always required |
| Embedded cluster | Manage `postvec.providers_path`, scan the databases for affected columns, reload the running host |
| Remote cluster | Refused by name. The message points at `--path` |

Reading `provider ls`:

```text
providers.d: /etc/postvec/providers.d
openai  (openai, key: file:/etc/postvec/keys/openai.key)
  openai-text-embedding-3-small                dim 1536   served
cohere  (cohere, key: env:COHERE_API_KEY)
  cohere-embed-v4-0                            dim 1536   NOT served (reload or restart the host)
azure  (openai, key: env:AZURE_KEY)  [enabled = false]
  openai-text-embedding-3-large                dim 3072   disabled in the file
mistral  (mistral, key: none)
  ! the host REFUSES this file: provider "mistral" needs an API key: set
    exactly one of api_key_file (recommended), api_key_env or api_key
  mistral-mistral-embed                        dim 1024   not loadable (see above)
```

`NOT served` after a hand-edit: reload or restart. `enabled = false`:
reported as disabled. `REFUSES`: the file will not load. `ls` and
`doctor` apply the loader's rules.

## What `doctor` checks

`postvec doctor` has a `provider.*` family, all read-only:

| Check | Reports |
|---|---|
| `provider.directory` | **Fails** when the directory is group- or world-writable, or when ownership or ancestors fail the write-safety rules. **Warns** on mere read/execute bits. "Does not exist" is a **pass** |
| `provider.file` | A file the serving host would refuse: mode, unknown fields, two sources for one secret, an unknown type, no credential, a bad `dim` / `region` / `base_url`, no `[[models]]`. **Warns** on a plaintext `base_url` to a non-loopback host |
| `provider.key-source` | A referenced key file that is missing, a symlink or too permissive, or a named variable that is absent. Every source, including both halves of an AWS SigV4 pair |
| `provider.served` | A configured model the running host does not currently serve. Skipped for `enabled = false` files |

A complaint about an environment variable can be a false alarm.
`doctor` observes its own environment, and the postmaster's is what
matters. The check says so.

`provider add` / `rm` write the files first and then ask the host to
reload. When no host answers on a cluster target, the result is
partial (exit 3): the files are correct and a restart applies them. On
a `--path` target the same situation is a note, not a partial result.

## Not supported

| | Why |
|---|---|
| Provider API keys as GUCs, catalog rows or SQL arguments | The product position. Not a missing feature |
| A spend or token budget | `max_concurrent` bounds calls in flight, not money |
| AWS session tokens, instance profiles, IMDS, the credential chain | The signer takes static credentials |
| Vertex AI as a distinct connector, Azure OpenAI beyond `base_url` | Deferred. `base_url` already fronts OpenAI-shaped endpoints |
| Cohere int8 and binary embeddings | Deferred |
| Gemini models other than `gemini-embedding-001` | Refused at load. The contracts are not uniform |
| `Retry-After`-aware backoff, per-provider token budgets | Deferred |
| Reranking providers | A separate roadmap item |
| A private or corporate CA for provider TLS | The connectors use rustls with the bundled Mozilla root set, not the system trust store |
| Authenticated inference transport | Loopback gRPC (embedded) and node gRPC (remote) are plaintext and unauthenticated. Restrict the node's gRPC port. Use provider-side quotas as the spend control |

Provider outputs are not covered by the golden-vector suite. Hosted
models are not reproducible, and pinning them would test the provider
rather than postvec.

## Related documentation

- [External providers](/docs/models/providers) - the walkthrough
- [CLI](/docs/reference/cli) - flags and exit codes
- [GUCs](/docs/reference/gucs) - `providers_path` and the timeouts
- [Docker](/docs/install/docker#external-providers) - mounts and permissions
