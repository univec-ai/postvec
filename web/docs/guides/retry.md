---
title: Retry dead jobs
description: Dead-letter inspection and re-drive with retry_dead().
---

# Retry dead jobs

Permanent failures and exhausted retries land in `postvec.jobs_dead`.
They do not retry on their own.

```sql
SELECT dead_id, pk_value, last_error
  FROM postvec.jobs_dead;
```

Resolve the condition named by `last_error`, such as a missing model,
oversized input, or unloaded converter. Then run:

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

`retry_dead()` takes `regclass` (the one exception), so the invoking role's
`search_path` resolves the table before the definer body runs.
`'public.docs'` is unambiguous.

## Limitations

- Touch another entry's dead rows. A mixed `dead_ids` list is validated
  atomically before locking; no locks are acquired if any identifier is
  invalid.
- Run on a missing, disabled, or migrating entry.
- Copy error / attempt state onto the new job. The new job is clean.

Authorization uses the invoking role (`SET ROLE` counts). `jobs_dead` is
PUBLIC-SELECT; the re-drive function is the gated write.

Concurrent calls serialize: one consumes the rows, and the other reports that
they are no longer present.

## Direct queue writes

::: danger Direct inserts into `postvec.jobs` are unsupported
Copying rows from `postvec.jobs_dead` skips validation, deduplication, and the
ownership check. `retry_dead()` provides the supported path.
:::
