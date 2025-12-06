---
title: Login and the private catalogue
description: Public subset for everyone; organisation accounts see every model and the better converters.
---

# Login and the private catalogue

Two channels, compiled into the CLI. There is no flag to point it at
another index.

| Channel | Who | Contents |
|---|---|---|
| Public | No credential | A **subset** of open-weight embedding models and a **subset** of conversion pairs |
| Private | Organisation account, a `uv_` API key | **All** embedders, 100+ conversion pairs, higher-fidelity converters |

Public entries keep their public download URLs even when you are logged
in. Signing in does not move ordinary models onto the private path.

The private catalogue is the organisation product, alongside
[ninference](/docs/concepts/modes) (distributed CPU/GPU inference) and
support. Get an account at [univec.ai](https://univec.ai).

The catalogue is currently in publication preview; these commands document the
client contract and become useful when the registry channels are live.

The authenticated route is **identity-only**: account active/verified,
key not expired. It does **not** check balance or spending limit. Create
a dedicated key with a **$0 spending limit** — the same `uv_` key would
otherwise also authorize billable UniVec API calls.

## Commands

```bash
sudo postvec login
sudo postvec whoami
sudo postvec model ls --available
sudo postvec logout
```

`login`, `whoami`, `ls --available`, `pull`, and `upgrade` accept
`--api-key-file FILE`. There is no `--api-key` value flag: arguments
show up in `/proc`.

Credential order:

1. `POSTVEC_API_KEY` (CI / ephemeral)
2. `--api-key-file`
3. The effective user's store — `/var/lib/postvec/auth.json` for root,
   otherwise `$XDG_CONFIG_HOME/postvec/auth.json`
4. None → public catalogue

A bad credential **fails**. It never silently becomes anonymous. `logout`
is how you choose public on purpose.

## Don't

::: danger Don't reuse a production spending key for `model pull`
A dedicated zero-limit key is the whole point of identity-only auth.
:::
