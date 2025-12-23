---
title: Login and the private catalogue
description: Authentication and access rules for the public and private model catalogues.
---

# Login and the private catalogue

The CLI uses two compiled-in catalogue channels.

| Channel | Who | Contents |
|---|---|---|
| Public | No credential | A **subset** of open-weight embedding models and a **subset** of conversion pairs |
| Private | A verified UniVec account and a `uv_` API key | The full embedding suite, nearly 100 conversion pairs and additional converter variants |

Public entries retain their public download URLs after authentication.
Authentication does not move public models to the private channel.

The private catalogue is the same inventory a ninference fleet can
serve. Organisation accounts and support around that fleet are
documented at [univec.ai](https://univec.ai). The authenticated route
itself is **identity-only**: the account must be active and verified,
and the key must not be expired. Balance and spending limit are not
checked.

The catalogue is currently in publication preview. These commands
document the client contract and become useful when the registry
channels are live.

Create a dedicated key with a **$0 spending limit**. The same `uv_` key
would otherwise also authorize billable UniVec API calls.

## Commands

```bash
sudo postvec login
sudo postvec whoami
sudo postvec model ls --available
sudo postvec logout
```

`login`, `whoami`, `ls --available`, `pull` and `upgrade` accept
`--api-key-file FILE`. Pass the secret that way, or via
`POSTVEC_API_KEY`, so it stays out of `/proc`.

Credential order:

1. `POSTVEC_API_KEY` (CI / ephemeral)
2. `--api-key-file`
3. The effective user's store - `/var/lib/postvec/auth.json` for root,
   otherwise `$XDG_CONFIG_HOME/postvec/auth.json`
4. None -> public catalogue

An invalid credential returns an error and does not fall back to
anonymous access. `logout` restores public-only catalogue resolution.

## Credential isolation

:::: danger Production spending keys are unsuitable for model operations
Model catalogue access should use a dedicated key with a zero spending
limit.
::::
