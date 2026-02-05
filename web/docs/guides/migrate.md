---
title: Migrate models
description: In-place model migration via convert (or re-embed), finalize and abort.
---

# Migrate models

Stored vectors move to a new model in place. The default strategy is
`convert`: UniVec translates the stored vectors. Source text is not
sent through the old embedding provider.

When the corpus should stay put, [bridge search](/docs/guides/bridge)
converts queries into the existing space instead.

A migration does not finish on its own. It stops and waits at the two
accented states below.

<figure class="pvd">
<svg viewBox="0 0 576 456" role="img" aria-labelledby="pvd-mig-title pvd-mig-desc">
<title id="pvd-mig-title">Migration lifecycle and its two manual finalize steps</title>
<desc id="pvd-mig-desc">A migration converts rows, then waits in awaiting_finalize until migration_finalize is called, which performs the column swap. An entry that had an ANN index then waits again in awaiting_index for the rebuilt index and a second finalize; an entry without one goes straight to done. migration_abort is available only before the swap.</desc>
<defs>
<marker id="pvd-mg-head" markerWidth="8" markerHeight="6" refX="7" refY="3" orient="auto">
<polygon class="head" points="0 0, 8 3, 0 6"/>
</marker>
</defs>

<rect class="s-mask" width="576" height="456"/>

<path class="c" d="M 244,48 V 72" marker-end="url(#pvd-mg-head)"/>
<path class="c" d="M 244,136 V 184" marker-end="url(#pvd-mg-head)"/>
<path class="c" d="M 244,248 V 296" marker-end="url(#pvd-mg-head)"/>
<path class="c" d="M 244,360 V 396" marker-end="url(#pvd-mg-head)"/>
<path class="c" d="M 144,216 H 96 Q 88,216 88,224 V 396 Q 88,404 96,404 H 228" marker-end="url(#pvd-mg-head)"/>
<path class="c c-dash" d="M 344,216 H 400" marker-end="url(#pvd-mg-head)"/>

<rect class="s-mask" x="252" y="46" width="68" height="12"/>
<text class="t-arrow" x="286" y="55" text-anchor="middle">migrate()</text>
<rect class="s-mask" x="252" y="154" width="92" height="12"/>
<text class="t-arrow" x="298" y="163" text-anchor="middle">ROWS CONVERTED</text>
<rect class="s-mask" x="252" y="266" width="100" height="12"/>
<text class="t-arrow" x="302" y="275" text-anchor="middle">FINALIZE · SWAP</text>
<rect class="s-mask" x="252" y="372" width="56" height="12"/>
<text class="t-arrow" x="280" y="381" text-anchor="middle">FINALIZE</text>
<rect class="s-mask" x="96" y="298" width="48" height="12"/>
<text class="t-arrow" x="120" y="307" text-anchor="middle">NO INDEX</text>
<rect class="s-mask" x="352" y="196" width="40" height="12"/>
<text class="t-arrow" x="372" y="205" text-anchor="middle">ABORT</text>

<circle class="head" cx="244" cy="40" r="6"/>

<rect class="s-mask" x="144" y="72" width="200" height="64" rx="8"/>
<rect class="s-node" x="144" y="72" width="200" height="64" rx="8"/>
<text class="t-name" x="244" y="100" text-anchor="middle">running</text>
<text class="t-sub" x="244" y="114" text-anchor="middle">convert, or re-embed</text>

<rect class="s-mask" x="144" y="184" width="200" height="64" rx="8"/>
<rect class="s-focal" x="144" y="184" width="200" height="64" rx="8"/>
<text class="t-name" x="244" y="212" text-anchor="middle">awaiting_finalize</text>
<text class="t-sub" x="244" y="226" text-anchor="middle">waits for you</text>

<rect class="s-mask" x="144" y="296" width="200" height="64" rx="8"/>
<rect class="s-focal" x="144" y="296" width="200" height="64" rx="8"/>
<text class="t-name" x="244" y="324" text-anchor="middle">awaiting_index</text>
<text class="t-sub" x="244" y="338" text-anchor="middle">swap done, rebuild ANN</text>

<rect class="s-mask" x="400" y="184" width="152" height="64" rx="8"/>
<rect class="s-node" x="400" y="184" width="152" height="64" rx="8"/>
<text class="t-name" x="476" y="212" text-anchor="middle">aborted</text>
<text class="t-sub" x="476" y="226" text-anchor="middle">only before the swap</text>

<circle class="s-node" cx="244" cy="404" r="8"/>
<circle class="head" cx="244" cy="404" r="5"/>
<text class="t-sub" x="244" y="432" text-anchor="middle">done</text>
</svg>
</figure>

```sql
SELECT postvec.migrate(
  'public.docs', 'body',
  new_model => 'baai-bge-m3'
) AS migration_id \gset

SELECT migration_id, state, rows_done, rows_total, progress_pct, error
  FROM postvec.migration_status(:migration_id);
```

Repeat the status query until `state = 'awaiting_finalize'`, then:

```sql
SELECT postvec.migration_finalize(:migration_id);
```

:::: tip Expected
After the first finalize, an entry that had an ANN index normally enters
`awaiting_index`. The column swap is already done; the entry is live on
the new model. Run the suggested concurrent index, then finalize again.
::::

```sql
SELECT suggested_index_sql
  FROM postvec.migration_status(:migration_id);
-- run that CREATE INDEX CONCURRENTLY in autocommit, then:
SELECT postvec.migration_finalize(:migration_id);
SELECT state FROM postvec.migration_status(:migration_id);
```

In psql, `\gexec` executes `suggested_index_sql`.

## Strategies

| `strategy` | What it does |
|---|---|
| `convert` (default) | Translate existing vectors. Needs a converter (or bridge) to the target. |
| `reembed` | Embed current source text (renders the template) with the new model. |
| `auto` | Convert when a route exists, otherwise re-embed. |

:::: code-group

```sql [convert]
SELECT postvec.migrate(
  'public.docs', 'body',
  new_model => 'baai-bge-m3',
  strategy => 'convert'
);
```

```sql [reembed]
SELECT postvec.migrate(
  'public.docs', 'body',
  new_model => 'baai-bge-m3',
  strategy => 'reembed'
);
```

```sql [auto]
SELECT postvec.migrate(
  'public.docs', 'body',
  new_model => 'baai-bge-m3',
  strategy => 'auto'
);
```

::::

`reindex`: `manual` (default) or `blocking`. Manual is recommended for tables serving application traffic.

On an embedded host, pull a converter with [`postvec model pull`](/docs/models/pull). On a remote host, install it on the `postvec-server` node.

## Abort

```sql
SELECT postvec.migration_abort(:migration_id);
```

`migration_abort()` is available **before** the swap. The original column remains unchanged.

## Observed entries

An `adopt(sync => false)` entry has no write path. `migrate()` refuses it unless `observed_writes_quiesced => true`, with writes remaining stopped through finalization. Otherwise the watermark can miss updates.

## Chunked entries

Counts refer to **chunks**. Conversion sends vectors. New writes during the migration embed with the new model into the scratch column.

## Finalization constraints

`migrate()` already refuses lossy column metadata (defaults, `NOT NULL`, constraints, comments, ACLs, stats/storage) before creating the `_new` scratch column. `finalize` takes `ACCESS EXCLUSIVE` and re-inspects. If dependent views or indexes appeared, it returns a retryable error without using `CASCADE`. The inspection walks `pg_partition_tree`.

## Validation constraints

:::: danger Conversion requires correct source-model provenance
An incorrect model assertion at `adopt()` produces invalid converted vectors.
Provenance must be confirmed first; otherwise use `strategy => 'reembed'`.
::::

:::: info `awaiting_index` means the column is already live
The data is on the new model. Build the index, then finalize again.
::::

:::: danger Only one migration may be active per entry
`migration_status()` reports the active migration. It can be aborted before
the column swap when a different target or strategy is required.
::::
