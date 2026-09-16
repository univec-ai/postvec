---
title: Login and the private catalogue
description: Authentication and access rules for the public and private model catalogues.
---

# Login and the private catalogue

The CLI uses two compiled-in catalogue channels. The public channel needs no
credential; the private channel needs a UniVec account. The bundled MiniLM
model is already on disk after a complete install.

| Channel | Who | Contents |
|---|---|---|
| Public | No credential | A **subset** of open-weight embedding models and a **subset** of conversion pairs |
| Private | A verified UniVec account and a `uv_` API key | The full embedding suite, nearly 100 conversion pairs and additional converter variants |

Public entries retain their public download URLs after authentication.

Organization accounts and support are documented at
[univec.ai](https://univec.ai). The authenticated route is **identity-only**:
the account must be active and verified, and the key must be unexpired.

The catalogue is currently in publication preview. On a
[postvec-server](/docs/server/) node the same credential comes from
`POSTVEC_API_KEY`, a `postvec login` as the service account, or the
[dashboard](/docs/server/dashboard) **Use key** box.

Create a dedicated key with a **€0 spending limit**. The same `uv_` key
would otherwise also authorize billable UniVec API calls.

## Commands

```bash
postvec login
postvec whoami
postvec model ls --available
postvec logout
```

:::: tip Expected
`whoami` prints the account after `login`, or reports anonymous before
it. `ls --available` lists the public catalogue without a credential
and the private superset after a valid `login`. `logout` returns you
to public-only.
::::

Each command uses the credential of the user that runs it, so a later
`sudo model pull` needs `sudo postvec login` or `--api-key-file`.
Credentials are stored per effective user.

`login`, `whoami`, `ls --available`, `pull` and `upgrade` accept
`--api-key-file FILE`. Pass the secret that way, or via
`POSTVEC_API_KEY`, so it stays out of `/proc`.

Credential order:

1. `POSTVEC_API_KEY` (CI / ephemeral)
2. `--api-key-file`
3. The effective user's store - `/var/lib/postvec/auth.json` for root,
   otherwise `$XDG_CONFIG_HOME/postvec/auth.json`
4. None -> public catalogue

An invalid credential returns an error. `logout` restores public-only
catalogue resolution.

## Credential isolation

:::: danger Production spending keys are unsuitable for model operations
Model catalogue access should use a dedicated key with a zero spending
limit.
::::
