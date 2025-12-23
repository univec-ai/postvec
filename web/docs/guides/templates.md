---
title: Templates
description: Format strings for adding row context to embedded text.
---

# Templates

A template defines the text that is **embedded**. The source column remains the lifecycle anchor and FTS input.

```sql
SELECT postvec.set_format(
  'public.docs', 'body',
  E'$title\n\n$body'
);
```

Or pass `format => E'$title\n\n$body'` to `enable()` / `adopt()`.

:::: tip Expected
`set_format()` returns void, then `status().pending_jobs` jumps. Every
row is re-enqueued, including NULL-source rows (those clear a stale
vector). Until the queue drains, search sees a mix of old-template and
new-template vectors.
::::

## Grammar

| Token | Meaning |
|---|---|
| `$name` | ASCII identifier `[A-Za-z_][A-Za-z0-9_]*` |
| `${exact column name}` | Quoted / awkward names. `}}` inside braces is one `}` |
| `$$` | A literal `$` |
| anything else | Literal |

There is **no backslash processing**. `\n` is two characters. Newlines come from SQL string syntax (`E'\n'` or dollar-quoting).

Limits: non-empty, <= 16 KiB, <= 64 distinct columns. Must reference the source column. Must **not** reference the vector column (that is a feedback loop). A chunked template must reference `$chunk` and must not reference the source column.

NULL context columns render as `''`. A NULL **source** renders SQL NULL, costs no inference and converges the vector to NULL.

Triggers fire on a change to **any** referenced column. Text is rendered at claim time and is not stored on the job, so a re-driven job uses the current template.

## `set_format()` is bulk maintenance

The call takes `SHARE ROW EXCLUSIVE` (blocks writes) while it replaces triggers and enqueues every row. A maintenance window is appropriate. Comparison is **bytewise**: `$body` -> `${body}` is a real change.

Clear a template with `set_format(..., NULL)`.

`migrate(strategy => 'reembed')` renders the template. `strategy => 'convert'` translates existing vectors.

On `adopt(format => ...)` the template is also a provenance assertion about the vectors already stored. Use `backfill => 'all'` if that assertion is uncertain.

## Invalid forms

:::: danger Newlines come from SQL string syntax
Use `E'$title\n\n$body'` or dollar-quoting. In a plain SQL string, `\n`
is a backslash and an `n`.
::::

:::: danger Templates cannot reference the vector column
The function rejects this reference because it would create a continuous
re-enqueue loop.
::::
