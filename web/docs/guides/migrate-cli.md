---
title: Change the stored model (CLI)
description: Install a local or hosted converter before postvec.migrate().
---

# Change the stored model (CLI)

SQL: [`migrate()`](/docs/guides/migrate). The CLI installs the converter
`migrate(strategy => 'convert')` uses.

## Local converter

On an embedded host:

```bash
postvec model ls --available
sudo postvec model pull "$converter_name" --dry-run
sudo postvec model pull "$converter_name" --yes
sudo postvec model activate "$converter_name" --yes
sudo postvec doctor --database app --deep
```

On [postvec-server](/docs/server/models):

```bash
postvec model pull "$converter_name"
postvec model activate "$converter_name"
postvec-server load "$converter_name"
postvec-server status --fleet
```

Then run [`refresh_models()`](/docs/guides/helpers) in SQL if the cache
is stale. See [pull, activate, upgrade, remove](/docs/models/pull) for
the catalogue, login and receipts.

## Hosted UniVec converter

```bash
sudo postvec provider add univec --convert-to baai-bge-m3
postvec provider ls
```

`--convert-to` adds every catalogue route into that target.
`--convert SRC:DST` adds one pair. Walkthrough:
[UniVec hosted models](/docs/models/univec).

The migration sends stored vectors to UniVec. `migration_status().resolved_via`
names the converter after [`migrate()`](/docs/guides/migrate) starts.

::::: tip Expected
`provider ls` lists the convert entries as `served`. A local converter
wins over a hosted one with the same source and target.
:::::
