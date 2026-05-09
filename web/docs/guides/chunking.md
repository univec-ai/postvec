---
title: Chunk long documents
description: Recursive 1:N chunking into a managed destination table.
---

# Chunk long documents

`chunking => 'recursive'` turns one source row into many searchable
chunk vectors in a table postvec manages. Search still returns **one
row per document**, plus the winning chunk.

Use this when documents are longer than the model's preferred input.
Matches are scored at passage level and reported at document level.

## 1. Enable with a destination

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

`destination` is required, unqualified and created in the source schema.
Defaults: `chunk_size` 2000, `chunk_overlap` 200. Bounds: size
64-100000, overlap 0-size-1.

:::: tip Expected
`status()` shows `chunking = recursive`, a `destination` and
`pending_refresh_jobs` then `pending_embed_jobs` draining. Each article
produces one refresh job; the worker splits it and fans out one embed
job per chunk.
::::

## 2. Inspect chunks

The destination and `{destination}_view` are **owner-only**. Grant
explicitly:

```sql
GRANT SELECT ON public.articles_body_chunks,
               public.articles_body_chunks_view TO app_role;
```

The view contains chunk fields but no vector or source columns. Source
fields require a join to the source table.

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

## Update and delete

An `UPDATE`/`DELETE` deletes that document's chunk rows (and queued
work) **inside the writer's transaction**. Bulk updates multiply the
cost. Splitting and re-embedding stay asynchronous.

Until refresh and embed jobs drain, the document is **missing from
search**. The gap is a temporary false negative, never stale chunk
text.

```sql
UPDATE public.articles SET body = 'a completely new short body' WHERE id = 1;
SELECT count(*) FROM public.articles_body_chunks WHERE postvec_source_pk = 1;
```

:::: tip Expected
Count is 0 immediately after the update. `pending_refresh_jobs` is 1.
::::

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

Storage grows because overlap duplicates text and every chunk is an
index row. Active destinations may need lower autovacuum scale factors
and periodic `REINDEX INDEX CONCURRENTLY`.

## Migration counts chunks

`migrate()` converts or re-embeds **chunk rows**. `rows_total` /
`rows_done` are chunk counts. Conversion sends vectors. Fresh writes
during a migration embed with the new model directly.

## Teardown

```sql
SELECT postvec.disable('public.articles', 'body');  -- keeps chunks
SELECT postvec.disable('public.articles', 'body',
                       drop_destination => true);   -- needs ownership markers
```

Manual edits to destination rows are invisible to postvec until the
related source row changes again.

## Limits and refusals

- Single-column PK only. No composite PK. No `adopt()` of a chunked entry.
- Deterministic splitter, <= 10,000 non-blank chunks, <= 32 MiB UTF-8
  input, <= 4x output amplification per document. Overlap above 75% of
  `chunk_size` can exceed the amplification limit on long documents;
  `enable()` emits a warning.
- Live splitter reconfiguration is refused. Disable, drop and re-enable
  to change splitter settings.

:::: info Chunk destinations are owner-only by default
Application access requires an explicit grant on the destination and
its view.
::::

:::: danger Destination tables must remain under postvec management
Ownership is stored as a marker comment on the destination. Teardown
requires that marker. Recreating a same-named table leaves ownership
unset.
::::
