---
title: Templates
description: Format strings for adding row context to embedded text.
---

# Templates

A template defines the text that is **embedded**. The source column is
still what full-text search uses.

Use this when a short body needs title or other row context in the
vector.

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

`\n` is two characters unless the SQL string itself interprets it.
Newlines come from SQL string syntax (`E'\n'` or dollar-quoting).

Limits: non-empty, <= 16 KiB, <= 64 distinct columns. Must reference
the source column. Referencing the vector column is a feedback loop and
is refused. A chunked template must reference `$chunk` and omit the
source column.

NULL context columns render as `''`. A NULL **source** becomes a NULL
vector, with no inference call.

Triggers fire on a change to **any** referenced column. The template is
applied when the worker embeds the row, so a retry uses the current
template.

## `set_format()` is bulk maintenance

The call takes `SHARE ROW EXCLUSIVE` (blocks writes) while it replaces
triggers and enqueues every row. Treat it as a maintenance window.
`$body` and `${body}` are different strings.

Clear a template with `set_format(..., NULL)`.

`migrate(strategy => 'reembed')` renders the template.
`strategy => 'convert'` translates existing vectors.

On `adopt(format => ...)` the template describes how existing vectors
were produced. Use `backfill => 'all'` if that is uncertain.

## Refused forms

:::: danger Newlines come from SQL string syntax
Use `E'$title\n\n$body'` or dollar-quoting. In a plain SQL string, `\n`
is a backslash and an `n`.
::::

:::: danger The template cannot name the vector column
That would create a continuous re-enqueue loop.
::::
