---
title: Pull, upgrade, remove
description: postvec model pull / upgrade / rm / activate / ls / show.
---

# Pull, upgrade, remove

These commands change an **engine root**. On an embedded cluster they
also hot-load and refresh `postvec.models` in every configured database.
The engine hot-loads the model.

<PgSnippet id="model-pull" />

:::: tip Expected
The plan lists the requested model, engine dependencies, download size,
peak disk and licences. After `--yes`, `model ls` shows it loaded.
`SELECT vector_dims(postvec.embed('...', 'baai-bge-m3')::vector)` returns
**1024** for BGE-M3. The engine hot-loads the model.
::::

Dependencies are pulled automatically. The complete
archive is SHA-256 verified **before** anything is extracted.

## Replacement constraints

If the name is already installed, `pull` prints the `model upgrade`
command and stops. It also refuses:

- the same name under another backend (lookup would be ambiguous)
- a package-owned or manually copied directory
- the same revision with a different digest
- an unmet `min_postvec_version`
- insufficient peak disk

## Upgrade

```bash
sudo postvec model upgrade baai-bge-m3 --dry-run
sudo postvec model upgrade baai-bge-m3 --yes
sudo postvec model upgrade --all --yes
```

Identity must stay compatible (type, backend, dimensions,
source/target). Downgrades are refused. Package-owned and manually
installed models are not replaced.

On a live embedded target the swap is recoverable: a crash leaves enough
state for the next model command to finish or roll forward. During the
brief unload/load window, work for that model retries; synchronous
`embed`/`search` can fail.

## `ls`, `show`, `rm`, `activate`

```bash
postvec model ls
postvec model ls --available
postvec model show baai-bge-m3 --verify
sudo postvec model rm baai-bge-m3 --dry-run
sudo postvec model activate --yes
```

- `ls` - name, type/dimension, size, owner, revision, enabled, load
  state. `?` means unknown, not current.
- `show --verify` hashes every file against the receipt. Works offline.
- `rm` only deletes CLI-owned receipts. Refuses an explicitly preloaded
  model until the `--model` allow-list changes. Removal is refused when
  it would break a dependant unless `--force` is supplied. The model
  unloads before deletion.
- `activate` takes **no names**. Scan-load or allow-list behavior
  follows the existing `setup --model` configuration.

`rm` does not modify stored database vectors.

## Terms

| Policy | Required action |
|---|---|
| `none` | Display only |
| `notice` | Confirm interactively, or `--accept-license ID@VERSION` |
| `organization` | Cannot be accepted locally |

`--yes` confirms the mutation but does not accept licence terms.

## Remote mode

```bash
sudo postvec --cluster 18/main model ls          # advertised by nodes
sudo postvec --cluster 18/main model pull baai-bge-m3 --dry-run
```

:::: tip Expected
The second command returns an error because files on the database host
would not change the remote fleet. Model changes must occur through
ninference or through `--path` on a standalone root.
::::

## Credential scope

:::: danger Credentials are scoped to the effective user
After a non-root `login`, a command run through `sudo` requires a
separate `sudo postvec login` or `--api-key-file`. Invalid credentials
do not fall back to public access.
::::
