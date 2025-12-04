---
title: Retry dead jobs
description: Inspect jobs_dead and re-drive with retry_dead() after fixing the cause.
---

# Retry dead jobs

Permanent failures and exhausted retries land in `postvec.jobs_dead`.
They do not retry on their own.

```sql
SELECT dead_id, pk_value, last_error
  FROM postvec.jobs_dead;
```

Fix whatever `last_error` names (missing model, oversized input, a
converter that is not loaded, …). Then:

```sql
-- every dead row for this entry
SELECT postvec.retry_dead('public.docs', 'body');

-- a specific set
SELECT postvec.retry_dead(
  'public.docs', 'body', ARRAY[17, 23]::bigint[]
);
```

::: tip Expected
The return value is **dead rows consumed**, not queue rows inserted.
Several dead rows can share a PK; an already-pending job coalesces.
Those rows leave `jobs_dead`.
:::

`retry_dead()` takes `regclass` (the one exception) so *your* search_path
resolves the table before the definer body runs. `'public.docs'` is
fine.

## What it will not do

- Touch another entry's dead rows. A mixed `dead_ids` list is proven
  first; nothing is locked if any id is wrong.
- Run on a missing, disabled, or migrating entry.
- Copy error / attempt state onto the new job. The new job is clean.

Authorization uses the invoking role (`SET ROLE` counts). `jobs_dead` is
PUBLIC-SELECT; the re-drive function is the gated write.

Concurrent calls serialize: one consumes, the other reports them gone.

## Don't

::: danger Don't `INSERT INTO postvec.jobs SELECT … FROM postvec.jobs_dead`
That recipe is gone from the docs on purpose. It skips validation,
dedup, and the ownership check. Use `retry_dead()`.
:::
