---
title: One-shot helpers (CLI)
description: model ls and doctor after embed, convert or refresh_models.
---

# One-shot helpers (CLI)

SQL: [`embed()` / `convert()` / `refresh_models()`](/docs/guides/helpers).
The CLI lists inventory and checks the host after those calls.

```bash
postvec model ls
sudo postvec doctor --database app --deep
```

::::: tip Expected
`model ls` shows the model `embed()` named as `loaded` (embedded) or
advertised by postvec-server (remote). `doctor` exits 0.
:::::

On remote mode, inventory is on the server:
[models on postvec-server](/docs/server/models). Pull and activate:
[pull](/docs/models/pull).
