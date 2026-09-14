---
title: Search a retired space (CLI)
description: Pull and activate the local converter chain so search can embed-bridge into a stored space.
---

# Search a retired space (CLI)

SQL: [adopt and search](/docs/guides/bridge). The CLI installs the
local embed model, converter and `embed-bridge` executor that
`search()` uses to convert each query into the stored space.

## Embedded host

```bash
postvec model ls --available
converter_name='replace-with-catalogue-name'
sudo postvec model pull "$converter_name" --dry-run
sudo postvec model pull "$converter_name" --yes
sudo postvec model activate "$converter_name" --yes
sudo postvec doctor --database app --deep
```

`pull` installs the closure deactivated. `activate` on the converter
enables its deactivated dependencies with it (the embed model and the
`embed-bridge` executor). The engine loads a chain only when every
member is enabled.

::::: tip Expected
The dry-run plan names the converter, its embed dependency and the
`embed-bridge` executor. After `activate`, `model ls` shows them
`loaded`. `doctor` exits 0.
:::::

The public catalogue holds a subset of the source/target pairs; the
private catalogue holds the broader conversion inventory
([login](/docs/models/login)).

## Remote (postvec-server)

Models are administered on each [postvec-server](/docs/server/models):
run the model commands on the node itself, because a database host in
remote mode refuses a model mutation that would change only local files.
At least one server must host the complete embed model, converter and
bridge chain. Pieces discovered on different nodes stay separate.

```bash
postvec model pull "$converter_name"
postvec model activate "$converter_name"
postvec-server load "$converter_name"
postvec-server status --fleet
```

Then refresh the SQL cache from a database session:
[`refresh_models()`](/docs/guides/helpers).

See [pull, activate, upgrade, remove](/docs/models/pull) for pull
details and [search a retired space (SQL)](/docs/guides/bridge) for the
SQL side.
