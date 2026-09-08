---
title: Connector files
description: providers.d format, key sources, loading rules, model names and doctor checks.
---

# Connector files

One TOML file per provider. The host serves what the file declares.

Copy-paste setup: [OpenAI](/docs/models/openai),
[Cohere](/docs/models/cohere), [Amazon Bedrock](/docs/models/aws),
[Gemini](/docs/models/gemini), [Mistral](/docs/models/mistral),
[OpenRouter](/docs/models/openrouter), [UniVec](/docs/models/univec).
Shared rules: [external providers](/docs/models/providers).

## File format

```toml
# /etc/postvec/providers.d/openai.toml
provider = "openai"      # openai | openrouter | mistral | google | cohere | aws | univec
enabled  = true          # false parses and serves nothing

api_key_file = "/etc/postvec/keys/openai.key"

base_url       = "https://api.openai.com"  # optional origin
max_concurrent = 4       # outbound requests in flight
timeout_ms     = 20000   # per HTTP attempt

[[models]]
name              = "openai-text-embedding-3-small"  # route name
provider_model_id = "text-embedding-3-small"         # API id
dim               = 1536   # responses are checked against this
max_batch         = 512    # host sub-batches to this
max_tokens        = 8191   # advertised as sequence_len
# space    = "openai-text-embedding-3-small"  # vector space; default is name
# priority = 1                                # lower wins; omit for derived order
# added    = "2026-09-08T00:00:00Z"           # RFC 3339; orders unprioritised routes
```

`base_url` replaces the **origin only**. Each connector appends its own path.
OpenAI-shaped APIs and UniVec embeds use `/v1/embeddings`. UniVec conversion
uses `/v1/convert`, Cohere uses `/v2/embed` and Gemini uses its batch path.
The URL must be absolute `http` or `https` with a host. It must not carry a
query string, fragment, userinfo or trailing `/`.

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
- Bedrock's `InvokeModel` sends one text per request, so `max_batch` is
  1. On a Titan-backed column, lower `postvec.batch_size` rather than
  raising the timeout.
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
migration re-embeds. Cohere, Gemini and UniVec use this distinction.

## UniVec converter entries

UniVec is the only connector that accepts `kind = "convert"`. An embed entry
uses the common schema above. A converter adds the source and target names in
the provider's vocabulary and in postvec's vocabulary:

```toml
provider = "univec"
api_key_file = "/etc/postvec/keys/univec.key"

[[models]]
name               = "univec-convert-snowflake-to-bge-m3"
kind               = "convert"
provider_source_id = "snowflake-arctic-embed-l-v2.0"
provider_model_id  = "baai-bge-m3"
source_model       = "snowflake-arctic-embed-l-v2.0"
target_model       = "baai-bge-m3"
source_dim         = 1024
dim                = 1024
max_batch          = 96
```

`provider_source_id` and `provider_model_id` go on the UniVec request.
`source_model` and `target_model` are matched against a postvec column and
the requested migration target. `source_dim` validates every input vector;
`dim` validates every output vector.

Converter entries refuse `max_tokens`. They also refuse a missing name or
dimension, identical source and target names, or a connector type other than
`univec`. One invalid entry prevents the whole file from loading.

Hosted converters are direct routes for `migrate()` and `convert()`.
Embed-bridge resolution uses local models. To search a retired space
without migrating, see [search a retired space](/docs/guides/bridge).
[UniVec hosted models](/docs/models/univec) has the hosted conversion
sequence.

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
- The schema is per connector. Unknown fields for that connector type
  are refused.
- `enabled = false` parses and serves nothing.
- A public name may appear once. Two entries in one file: the file is
  refused. Two files claiming the same name: neither serves until one
  drops it.
- A loaded local model keeps a colliding name, in `/config` and on the
  embed or convert path.
- The directory is bounded as a whole: at most 32 connector files, 512
  provider models and 256 total `max_concurrent` across every file. A
  single file holds at most 256 `[[models]]` entries (one whole UniVec
  catalogue with headroom), is at most 256 KiB, and a referenced secret at
  most 16 KiB. Breaking a directory-wide ceiling fails the whole scan.

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
text or stored vectors. The serving host refuses those cases. `provider add`
also refuses to write there, and `doctor` fails `provider.directory`.
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

The CLI uses a curated public name for a model in its built-in catalogue.
These entries are available:

| Provider | Model id | Route name | Space | Dim |
|---|---|---|---|---:|
| openai | `text-embedding-3-small` | `openai-text-embedding-3-small` | same | 1536 |
| openai | `text-embedding-3-large` | `openai-text-embedding-3-large` | same | 3072 |
| openai | `text-embedding-ada-002` | `openai-text-embedding-ada-002` | same | 1536 |
| google | `gemini-embedding-001` | `gemini-embedding-001` | same | 3072 |
| cohere | `embed-v4.0` | `cohere-embed-v4-0` | same | 1536 |
| cohere | `embed-english-v3.0` | `cohere-embed-english-v3-0` | same | 1024 |
| cohere | `embed-multilingual-v3.0` | `cohere-embed-multilingual-v3-0` | same | 1024 |
| aws | `amazon.titan-embed-text-v2:0` | `aws-titan-embed-text-v2-0` | same | 1024 |
| aws | `amazon.titan-embed-text-v1` | `aws-titan-embed-text-v1` | same | 1536 |
| mistral | `mistral-embed` | `mistral-mistral-embed` | same | 1024 |
| openrouter | `google/gemini-embedding-001` | `openrouter-google-gemini-embedding-001` | `gemini-embedding-001` | 3072 |
| openrouter | `openai/text-embedding-3-small` | `openrouter-openai-text-embedding-3-small` | `openai-text-embedding-3-small` | 1536 |

Any other id works too. The probe measures the dimension, or `--dim`
states it. OpenRouter ids are namespaced, so
`--model openai/text-embedding-3-large` becomes route
`openrouter-openai-text-embedding-3-large` in space
`openai-text-embedding-3-large`. Two embed entries in one file that
claim the same space at different dimensions refuse the file.

UniVec ids are not in the CLI's built-in catalogue. `provider add univec`
measures an embed model's dimension. For example, `--model baai-bge-m3`
becomes `univec-baai-bge-m3`.

For an unlisted id, the name is derived mechanically: lowercase, and
every character outside `[a-z0-9._-]` becomes `-`. `provider_model_id`
keeps the id exactly as the API expects it.

`gemini` is accepted as an alias for `google`, and `amazon` for `aws`.
The file always records the canonical name. An alias never changes
what you type in SQL.

## Commands

| Command | Result | Network |
|---|---|---|
| `provider add TYPE --model ID...` | Write or extend the file, verify, reload the host, refresh `postvec.models` | One embed per verified model, unless `--no-verify` |
| `provider add univec --convert-source ID ...` | Write or extend the file with a converter, verify, reload and refresh | One vector conversion, unless `--no-verify` |
| `provider ls` | Providers, key sources, models, dims and whether the host serves them now | Loopback |
| `provider test NAME [--model ID]` | Verify embed or converter entries on demand | One embed or vector conversion per selected entry |
| `provider rm NAME [--model ID]` | Drop one model entry or the whole file, then reload | Loopback |

Useful options on `add`:

| Option | Effect |
|---|---|
| `--name STEM` | Write `STEM.toml` instead of the type's name. Two OpenAI-compatible endpoints, two files |
| `--api-key-file P` / `--api-key-env VAR` / `--key-stdin` | Choose the key source. Without any of them and with a TTY, a hidden prompt asks |
| `--base-url URL` | Gateways, Azure-shaped fronts or a mock server. Not accepted for `aws` |
| `--region` | Required for `aws`, and only valid there |
| `--dim N` | For a single unlisted model, together with `--no-verify` |
| `--convert-source ID --convert-target ID` | UniVec provider ids for a hosted converter |
| `--source-model NAME --target-model NAME --source-dim N` | Postvec route names and the input dimension for a hosted converter |
| `--converter-name NAME` | Override the derived `univec-convert-<source>-to-<target>` name |
| `--no-verify` | Skip the live probe |
| `--dry-run` | Print the plan and change nothing |

The plan is confirmed before the probe. Declining it, or failing the
in-use acknowledgement, makes no API call.

`add` and `test` use an embedding probe for embed entries and a single-vector
conversion probe for converter entries. A connector change, such as a new key
source or `base_url`, rechecks every entry in that file.

Target resolution:

| Target | Behaviour |
|---|---|
| `--path DIR` | Filesystem management of `DIR/providers.d`, or of `DIR` itself when it already is one. `DIR` must already exist. New files inherit its owner. An embed entry requires `--acknowledge-in-use`; a new converter skips that flag |
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

## `doctor` checks

`postvec doctor` has a `provider.*` family, all read-only:

| Check | Reports |
|---|---|
| `provider.directory` | **Fails** when the directory is group- or world-writable, or when ownership or ancestors fail the write-safety rules. **Warns** on mere read/execute bits. "Does not exist" is a **pass** |
| `provider.file` | A file the serving host would refuse: mode, unknown fields, two sources for one secret, an unknown type, no credential, a bad `dim` / `region` / `base_url`, no `[[models]]`. **Warns** on a plaintext `base_url` to a non-loopback host |
| `provider.key-source` | A referenced key file that is missing, a symlink or too permissive, or a named variable that is absent. Every source, including both halves of an AWS SigV4 pair |
| `provider.served` | A configured model missing from the running host. Skipped for `enabled = false` files |

A complaint about an environment variable can be a false alarm.
`doctor` observes its own environment, and the postmaster's is what
matters. The check says so.

`provider add` / `rm` write the files first and then ask the host to
reload. When no host answers on a cluster target, the result is
partial (exit 3): the files are correct and a restart applies them. On
a `--path` target the same situation is a note, not a partial result.

## Limits

| Topic | What the product does |
|---|---|
| Credentials | Keys live in `providers.d` on the inference host. PostgreSQL holds a path |
| Spend control | `max_concurrent` bounds calls in flight. Set quotas on the provider |
| AWS auth | Static SigV4 pair or a Bedrock bearer token |
| Azure OpenAI | `base_url` fronts the `/openai/v1` API |
| Gemini | `gemini-embedding-001` only (the documented contract) |
| Hosted converters | Direct routes for `migrate()` and `convert()`. Embed-bridge uses local models |
| Provider TLS | rustls with the bundled Mozilla root set |
| Inference transport | Loopback gRPC (embedded) and postvec-server gRPC (remote) are plaintext. Restrict the gRPC port. Use provider-side quotas as the spend control |

- [External providers](/docs/models/providers)
- [OpenAI](/docs/models/openai) · [Cohere](/docs/models/cohere) ·
  [Amazon Bedrock](/docs/models/aws) · [Gemini](/docs/models/gemini) ·
  [Mistral](/docs/models/mistral) · [OpenRouter](/docs/models/openrouter)
- [UniVec hosted models](/docs/models/univec)
- [postvec-server](/docs/server/)
- [CLI](/docs/reference/cli)
- [GUCs](/docs/reference/gucs)
- [Docker](/docs/install/docker#external-providers)
