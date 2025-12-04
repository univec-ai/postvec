---
title: Pull, upgrade, remove
description: postvec model pull / upgrade / rm / activate / ls / show.
---

# Pull, upgrade, remove

These commands change an **engine root**. On an embedded cluster they
also hot-load and refresh `postvec.models` in every configured database.
No PostgreSQL restart.

```bash
postvec model ls --available
sudo postvec model pull baai-bge-m3 --dry-run
sudo postvec model pull baai-bge-m3 --yes
sudo postvec model show baai-bge-m3 --verify
sudo postvec --cluster 18/main doctor --database app --deep
```

::: tip Expected
The plan lists the requested model, engine dependencies, download size,
peak disk, and licences. After `--yes`, `model ls` shows it loaded.
`SELECT vector_dims(postvec.embed('…', 'baai-bge-m3')::vector)` returns
**1024** for BGE-M3. No restart.
:::

Dependencies are automatic. There is no `--with-deps`. The whole archive
is SHA-256 verified **before** anything is extracted.

## `pull` will not replace

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

Identity must stay compatible (type, backend, dimensions, source/target).
Downgrades are refused. Package/manual models are never replaced.

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

- `ls` — name, type/dimension, size, owner, revision, enabled, load
  state. `?` means unknown, never "current".
- `show --verify` hashes every file against the receipt. Works offline.
- `rm` only deletes CLI-owned receipts. Refuses an explicitly preloaded
  model until you change the `--model` allow-list. Refuses breaking a
  dependant unless `--force`. Unloads before deleting.
- `activate` takes **no names**. Scan-load vs allow-list is whatever
  `setup --model` configured.

`rm` never touches stored database vectors.

## Terms

| Policy | What you do |
|---|---|
| `none` | Display only |
| `notice` | Confirm interactively, or `--accept-license ID@VERSION` |
| `organization` | Cannot be accepted locally |

`--yes` confirms the mutation. It never accepts terms.

## Remote mode

```bash
sudo postvec --cluster 18/main model ls          # advertised by nodes
sudo postvec --cluster 18/main model pull baai-bge-m3 --dry-run
```

::: tip Expected
The second command **refuses**. Files on the database host would not
change the remote fleet. Administer ninference, or use `--path` on a
standalone root.
:::

## Don't

::: danger Don't `sudo model pull` after a non-root `login`
Credentials are per effective user. Run `sudo postvec login` too, or
pass `--api-key-file`. A failed credential never falls back to public.
:::
