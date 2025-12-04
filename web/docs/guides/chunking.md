---
title: Chunk long documents
description: Recursive 1:N chunking into a managed destination table.
---

# Chunk long documents

`chunking => 'recursive'` turns one source row into many searchable chunk
vectors in a table postvec manages. Search still returns **one row per
document**, plus the winning chunk.

Use this when a document is larger than the model likes, or when you want
passage-level hits with document-level results.

```sql
CREATE TABLE public.articles (
    id    bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    title text,
    body  text
);

SELECT postvec.enable(
  'public.articles', 'body',
  model         => 'sentence-transformers-all-minilm-l6-v2',
  chunking      => 'recursive',
  chunk_size    => 400,
  chunk_overlap => 40,
  destination   => 'articles_body_chunks',
  create_fts_index => true,
  format        => E'$title\n\n$chunk'
);
```

`destination` is required, unqualified, and created in the source schema.
Defaults: `chunk_size` 2000, `chunk_overlap` 200. Bounds: size 64–100000,
overlap 0–size-1.

::: tip Expected
`status()` shows `chunking = recursive`, a `destination`, and
`pending_refresh_jobs` then `pending_embed_jobs` draining. Each article
produces one refresh job; the worker splits it and fans out one embed
job per chunk.
:::

## Look at chunks

The destination and `{destination}_view` are **owner-only**. Grant
explicitly:

```sql
GRANT SELECT ON public.articles_body_chunks,
               public.articles_body_chunks_view TO app_role;
```

The view is chunk fields only — no vector, no source columns. Join back.

```sql
SELECT pk_value, chunk_seq, chunk_start, chunk_end, left(chunk_text, 40)
  FROM public.articles_body_chunks_view
 WHERE pk_value = 1
 ORDER BY chunk_seq
 LIMIT 3;

SELECT a.id, a.title, s.chunk_seq, left(s.chunk_text, 40) AS winning_chunk,
       round(s.rrf_score::numeric, 4) AS score
  FROM postvec.search('public.articles', 'body', 'sentence 42') AS s
  JOIN public.articles a ON a.id = s.pk_value::bigint;
```

Source-table RLS constrains chunk visibility through the view.

## Updates invalidate in the writer

An `UPDATE`/`DELETE` deletes that document's chunk rows (and queued work)
**inside the writer's transaction**. Bulk updates multiply the cost.
Splitting and re-embedding stay asynchronous.

Until refresh + embeds drain, the document is **missing from search**.
False negatives, never stale text.

```sql
UPDATE public.articles SET body = 'a completely new short body' WHERE id = 1;
SELECT count(*) FROM public.articles_body_chunks WHERE postvec_source_pk = 1;
```

::: tip Expected
Count is 0 immediately after the update. `pending_refresh_jobs` is 1.
:::

## Indexes target the destination

`index_mode => 'immediate'` is **refused** for chunked entries. After
backfill:

```sql
CREATE INDEX CONCURRENTLY articles_chunks_hnsw
  ON public.articles_body_chunks
  USING hnsw (body_semantic vector_cosine_ops);
```

or `index_mode => 'auto'` on a small table, or
`postvec.create_vector_index('public.articles', 'body')`.

Storage amplifies: overlap duplicates text, every chunk is an index row.
Lower autovacuum scale factors on a hot destination; consider periodic
`REINDEX INDEX CONCURRENTLY`.

## Migration counts chunks

`migrate()` converts or re-embeds **chunk rows**. `rows_total` /
`rows_done` are chunk counts. Convert sends vectors, never chunk text.
Fresh writes during a migration embed with the new model directly.

## Teardown keeps the destination

```sql
SELECT postvec.disable('public.articles', 'body');  -- keeps chunks
SELECT postvec.disable('public.articles', 'body',
                       drop_destination => true);   -- needs ownership markers
```

Rows you edit in the destination yourself are invisible to postvec until
that document's source row changes again.

## Limits and refusals

- Single-column PK only. No composite PK. No `adopt()` of a chunked entry.
- Deterministic splitter, ≤ 10,000 non-blank chunks, ≤ 32 MiB UTF-8 input,
  ≤ 4× output amplification per document. Overlap above 75% of
  `chunk_size` trips the budget on long documents; `enable()` warns.
- Live splitter reconfiguration is refused — disable, drop, re-enable.

## Don't

::: danger Don't `GRANT` nothing and wonder why the app cannot see chunks
Owner-only is the default. Grant the destination and the view.
:::

::: danger Don't `DROP TABLE` the destination out from under postvec
Ownership is a marker comment, not an OID. Teardown without that marker
refuses. Recreating a same-named table does not make it postvec's.
:::
