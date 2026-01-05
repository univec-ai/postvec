---
title: Pull, activate, upgrade, remove
description: postvec model pull / activate / deactivate / upgrade / rm / ls / show.
---

# Pull, activate, upgrade, remove

These commands change an **engine root**. On an embedded cluster they also
refresh `postvec.models` in every configured database.

<PgSnippet id="model-pull" />

:::: warning Installing is not activating
`pull` puts the files in place and stops. The installed descriptor is written
`enabled: false`, so the model is **not** loaded now and **not** loaded at the
next PostgreSQL restart. `model activate` is the only command that turns it on.
::::

:::: tip Expected
The plan lists the requested model, engine dependencies, download size, peak
disk and licences, and ends by naming the `model activate` command. After
`--yes`, `model ls` shows STATE `deactivated`; after `model activate`, `loaded`.
`SELECT vector_dims(postvec.embed('...', 'baai-bge-m3')::vector)` returns
**1024** for BGE-M3 once the model is active.
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

An upgrade **preserves activation state**: a serving model is unloaded, swapped
and reloaded; a deactivated model is swapped on disk and stays deactivated,
with no engine involvement. Replacing a model's bytes is not a decision about
whether it should serve.

On a live embedded target the swap is recoverable: a crash leaves enough
state for the next model command to finish or roll forward. During the
brief unload/load window, work for that model retries; synchronous
`embed`/`search` can fail.

## Activate and deactivate

```bash
sudo postvec model activate baai-bge-m3 --yes      # on, and it stays on
sudo postvec model activate --yes                  # every eligible model
sudo postvec model deactivate baai-bge-m3 --yes    # off, and it stays off
```

Both rewrite the `enabled` field of the installed `ninference.hub.json`, which
the engine reads at **every** start — so neither is undone by a PostgreSQL
restart. No GUC and no catalogue is involved.

- `activate NAME` enables the model's **deactivated dependency closure** with
  it. The engine refuses to load a closure containing a deactivated model, and
  a restart would not make one resident either, so enabling only the named root
  would activate nothing usable.
- A bare `activate` (or `--all`) means every eligible CLI-installed model.
- `deactivate` has **no `--all`**: turning every model off in one flag is how
  search goes down by accident.
- `deactivate` unloads first, then flips — so "deactivate returned an error"
  means "still on". `activate` flips first, then loads.
- `--path DIR` flips descriptors only: no engine, no database. That is the
  air-gapped staging form.
- Both refuse package-owned and manual directories, and both refuse a model
  named in an explicit `postvec.embedded_models` (a listed model that is
  disabled fails engine **startup**, so change the list first).
- `deactivate` and `rm` refuse a model another **enabled** model depends on,
  unless `--force`. Deactivating does **not** cascade to a model's own
  dependencies — they stay active, and turning them off too is your call.

### Columns that would lose their embedding route

Deactivating or removing a model that managed columns still depend on is
**allowed, never silent**:

```text
- WARNING: convert-bge-to-ada is the embedding route for these columns:
      app: public.docs.body (active) — declared on openai-text-embedding-ada-002,
           served through it
    search(), embed() and the worker will fail for those entries until
    convert-bge-to-ada is activated again, or the column is migrated to another
    model or disabled. Stored vectors are not touched.
```

The question is **not** "does a column name this model" but "can this column
still be embedded afterwards". A column on `openai-text-embedding-ada-002` is
served by a *converter* into that space plus `embed-bridge` — neither carries
the target's name — so a name match alone would miss the migration story
postvec exists for. The check mirrors the extension's two-tier resolution
against the inventory with the models removed: a direct embed model named for
the space, or a converter into it whose own source space is embeddable, through
`embed-bridge`. Deactivating any leg of that route trips the acknowledgement;
none of them does when a second route into the same space survives.

Interactively, type the model names back exactly as the prompt prints them
(separators and quotes are normalized). Non-interactively, pass
`--acknowledge-in-use` **together with** `--yes`. `--yes` confirms the change
you asked for; `--force` covers breaking other *models*; neither stands in for
the other. A configured database that cannot be inspected is reported as
**unknown**, and takes the same acknowledgement — a silent "looked clean" is
what this gate exists to prevent.

## `ls`, `show`, `rm`

```bash
postvec model ls
postvec model ls --available
postvec model show baai-bge-m3 --verify
sudo postvec model rm baai-bge-m3 --dry-run
```

- `ls` - name, type/dimension, size, owner, revision, and STATE. STATE
  reconciles the persistent switch against what the engine holds:
  `loaded`, `deactivated`, `not loaded` (activated but absent — `doctor` warns),
  or `loaded, deactivate pending` (an unload did not finish — `doctor` fails).
  `?` means unknown, not current.
- `show --verify` hashes every file against the receipt. Works offline, and
  passes on a deactivated model: the receipt covers the descriptor that was
  actually installed.
- `rm` only deletes CLI-owned receipts. Refuses an explicitly preloaded
  model until the `--model` allow-list changes. Removal is refused when
  it would break an enabled dependant unless `--force` is supplied, and takes
  the same in-use acknowledgement as `deactivate`. The model unloads before
  deletion.

`rm` does not modify stored database vectors, and neither does `deactivate`.

## Terms

| Policy | Required action |
|---|---|
| `none` | Display only |
| `notice` | Confirm interactively, or `--accept-license ID@VERSION` |
| `organization` | Cannot be accepted locally |

`--yes` confirms the mutation but does not accept licence terms.

## Remote mode

```bash
postvec --cluster 18/main model ls          # advertised by nodes
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
