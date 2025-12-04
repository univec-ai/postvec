---
title: Templates
description: Format strings that add row context to the text that gets embedded.
---

# Templates

A template is what gets **embedded**. The source column remains the
lifecycle anchor and the FTS input.

```sql
SELECT postvec.set_format(
  'public.docs', 'body',
  E'$title\n\n$body'
);
```

Or pass `format => E'$title\n\n$body'` to `enable()` / `adopt()`.

::: tip Expected
`set_format()` returns void, then `status().pending_jobs` jumps — every
row is re-enqueued, including NULL-source rows (those clear a stale
vector). Until the queue drains, search sees a mix of old- and
new-template vectors.
:::

## Grammar

| Token | Meaning |
|---|---|
| `$name` | ASCII identifier `[A-Za-z_][A-Za-z0-9_]*` |
| `${exact column name}` | Quoted / awkward names. `}}` inside braces is one `}` |
| `$$` | A literal `$` |
| anything else | Literal |

There is **no backslash processing**. `\n` is two characters. Newlines
come from SQL string syntax (`E'\n'` or dollar-quoting). This is the
first thing people trip on.

Limits: non-empty, ≤ 16 KiB, ≤ 64 distinct columns. Must reference the
source column. Must **not** reference the vector column (that is a
feedback loop). A chunked template must reference `$chunk` and must not
reference the source column.

NULL context columns render as `''`. A NULL **source** renders SQL NULL,
costs no inference, and converges the vector to NULL.

Triggers fire on a change to **any** referenced column. Text is rendered
at claim time, never stored on the job, so a re-driven job uses the
current template.

## `set_format()` is bulk maintenance

It takes `SHARE ROW EXCLUSIVE` (blocks writes) while it replaces triggers
and enqueues every row. Schedule it. Comparison is **bytewise**:
`$body` → `${body}` is a real change.

Clear a template with `set_format(..., NULL)`.

`migrate(strategy => 'reembed')` renders the template.
`strategy => 'convert'` does not — it translates existing vectors.

On `adopt(format => …)` the template is also a provenance assertion about
the vectors already stored. Use `backfill => 'all'` if that is not known
to be true.

## Don't

::: danger Don't write `format => '$title\n\n$body'`
In a plain SQL string that is a backslash and an `n`. Use
`E'$title\n\n$body'`.
:::

::: danger Don't reference the vector column
Refused. It would re-enqueue forever.
:::
