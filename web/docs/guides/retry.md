---
title: Retry dead jobs
description: Dead-letter inspection and re-drive with retry_dead().
---

# Retry dead jobs

Permanent failures and exhausted retries land in `postvec.jobs_dead`. They do not retry on their own.

<figure class="pvd">
<svg viewBox="0 0 656 300" role="img" aria-labelledby="pvd-jobs-title pvd-jobs-desc">
<title id="pvd-jobs-title">Job lifecycle, from trigger to dead letter</title>
<desc id="pvd-jobs-desc">A trigger creates a pending job; the worker claims it and either writes the vector, returns it to pending after a transient failure and backoff, or dead-letters it once the failure is permanent or the retries are exhausted. Dead jobs leave that state only when retry_dead is called.</desc>
<defs>
<marker id="pvd-jl-head" markerWidth="8" markerHeight="6" refX="7" refY="3" orient="auto">
<polygon class="head" points="0 0, 8 3, 0 6"/>
</marker>
<marker id="pvd-jl-head-accent" markerWidth="8" markerHeight="6" refX="7" refY="3" orient="auto">
<polygon class="head-accent" points="0 0, 8 3, 0 6"/>
</marker>
</defs>

<rect class="s-mask" width="656" height="300"/>

<path class="c" d="M 48,96 H 80" marker-end="url(#pvd-jl-head)"/>
<path class="c" d="M 216,96 H 272" marker-end="url(#pvd-jl-head)"/>
<path class="c" d="M 408,96 H 576" marker-end="url(#pvd-jl-head)"/>
<path class="c" d="M 340,64 V 40 Q 340,32 332,32 H 156 Q 148,32 148,40 V 64" marker-end="url(#pvd-jl-head)"/>
<path class="c" d="M 340,128 V 200" marker-end="url(#pvd-jl-head)"/>
<path class="c c-accent" d="M 272,232 H 156 Q 148,232 148,224 V 128" marker-end="url(#pvd-jl-head-accent)"/>

<rect class="s-mask" x="28" y="72" width="44" height="12"/>
<text class="t-arrow" x="50" y="81" text-anchor="middle">TRIGGER</text>
<rect class="s-mask" x="216" y="72" width="56" height="12"/>
<text class="t-arrow" x="244" y="81" text-anchor="middle">CLAIM</text>
<rect class="s-mask" x="440" y="72" width="72" height="12"/>
<text class="t-arrow" x="476" y="81" text-anchor="middle">VECTOR SET</text>
<rect class="s-mask" x="188" y="12" width="112" height="12"/>
<text class="t-arrow" x="244" y="21" text-anchor="middle">TRANSIENT · BACKOFF</text>
<rect class="s-mask" x="348" y="152" width="112" height="12"/>
<text class="t-arrow" x="404" y="161" text-anchor="middle">PERMANENT / SPENT</text>
<rect class="s-mask" x="188" y="240" width="88" height="12"/>
<text class="t-arrow t-accent" x="232" y="249" text-anchor="middle">retry_dead()</text>

<circle class="head" cx="40" cy="96" r="6"/>

<rect class="s-mask" x="80" y="64" width="136" height="64" rx="8"/>
<rect class="s-node" x="80" y="64" width="136" height="64" rx="8"/>
<text class="t-name" x="148" y="92" text-anchor="middle">pending</text>
<text class="t-sub" x="148" y="106" text-anchor="middle">postvec.jobs</text>

<rect class="s-mask" x="272" y="64" width="136" height="64" rx="8"/>
<rect class="s-node" x="272" y="64" width="136" height="64" rx="8"/>
<text class="t-name" x="340" y="92" text-anchor="middle">claimed</text>
<text class="t-sub" x="340" y="106" text-anchor="middle">worker holds it</text>

<rect class="s-mask" x="272" y="200" width="136" height="64" rx="8"/>
<rect class="s-focal" x="272" y="200" width="136" height="64" rx="8"/>
<text class="t-name" x="340" y="228" text-anchor="middle">jobs_dead</text>
<text class="t-sub" x="340" y="242" text-anchor="middle">never retried alone</text>

<circle class="s-node" cx="592" cy="96" r="8"/>
<circle class="head" cx="592" cy="96" r="5"/>
<text class="t-sub" x="592" y="124" text-anchor="middle">written</text>
</svg>
</figure>

```sql
SELECT dead_id, pk_value, last_error
  FROM postvec.jobs_dead;
```

Resolve the condition named by `last_error`, such as a missing model, oversized input or unloaded converter. Then run:

```sql
-- every dead row for this entry
SELECT postvec.retry_dead('public.docs', 'body');

-- a specific set
SELECT postvec.retry_dead(
  'public.docs', 'body', ARRAY[17, 23]::bigint[]
);
```

:::: tip Expected
The return value is **dead rows consumed**, not queue rows inserted.
Several dead rows can share a PK; an already-pending job coalesces.
Those rows leave `jobs_dead`.
::::

`retry_dead()` takes `regclass` (the one exception), so the invoking role's `search_path` resolves the table before the definer body runs. `'public.docs'` is unambiguous.

## Limitations

- Touch another entry's dead rows. A mixed `dead_ids` list is validated atomically before locking; no locks are acquired if any identifier is invalid.
- Run on a missing, disabled or migrating entry.
- Copy error / attempt state onto the new job. The new job is clean.

Authorization uses the invoking role (`SET ROLE` counts). `jobs_dead` is PUBLIC-SELECT; the re-drive function is the gated write.

Concurrent calls serialize: one consumes the rows, and the other reports that they are no longer present.

## Direct queue writes

:::: danger Direct inserts into `postvec.jobs` are unsupported
Copying rows from `postvec.jobs_dead` skips validation, deduplication and the
ownership check. `retry_dead()` provides the supported path.
::::
