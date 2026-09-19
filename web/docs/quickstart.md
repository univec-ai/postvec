---
title: Quick start local
description: Hybrid search in one postvec container. Pick PostgreSQL 16, 17 or 18.
---

# Quick start local

A container with PostgreSQL, postvec and one local model
([MiniLM](https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2)).
Inference runs inside that container.

Run the image, enable a column, wait for `pending_jobs = 0`, then index and
search.

The same SQL against [postvec-server](/docs/server/) is
[quick start remote](/docs/quickstart-remote).

## 1. Run the local image

<PgSnippet id="docker-quickstart" />

Wait until container status is `healthy` (PostgreSQL is up and MiniLM is
loaded):

<div v-pre>

```bash
docker inspect --format '{{.State.Health.Status}}' postvec
```

</div>

The image creates the extension in `POSTGRES_DB` on first initialization
only.

:::: info Optional
`postvec-healthcheck` exits 0 when the worker and engine are ready.
`postvec.embed()` returning 384-d also proves inference:

```bash
docker exec postvec postvec-healthcheck
docker exec -i postvec psql -U app -d app -c \
  "SELECT vector_dims(postvec.embed(
     'the isolated image performs inference',
     'sentence-transformers-all-minilm-l6-v2'
   )::vector);"
```
::::

## 2. Enable a column and insert

```bash
docker exec -i postvec psql -U app -d app <<'SQL'
CREATE TABLE docs (
  id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
  body text,
  category text
);

SELECT postvec.enable(
  'public.docs', 'body',
  model => 'sentence-transformers-all-minilm-l6-v2',
  create_fts_index => true
);

INSERT INTO docs (body, category) VALUES
  ('quarterly revenue guidance was raised after strong subscription growth',
   'finance'),
  ('migrating embedding models normally requires re-embedding source text',
   'engineering'),
  ('the office plants need watering twice a week',
   'office');

SELECT relation, pending_jobs, dead_jobs
  FROM postvec.status();
SQL
```

Vectors fill **asynchronously**. After INSERT commits, wait until
`pending_jobs = 0` and every `body_semantic` is non-NULL:

```bash
docker exec -it postvec psql -U app -d app
```

then (in psql):

```sql
SELECT count(*) FILTER (WHERE body_semantic IS NOT NULL) AS filled,
       count(*) AS total
  FROM docs;
```

:::: tip Expected
`filled = total`, `pending_jobs = 0`, `dead_jobs = 0`. On this image
that is usually a second or two.
::::

## 3. Index and search

Index (one-time):

```sql
SELECT postvec.create_vector_index('public.docs', 'body');
```

Search:

```sql
SELECT d.id, d.body,
       round(s.rrf_score::numeric, 5) AS score,
       s.semantic_rank, s.fts_rank
  FROM postvec.search(
         'public.docs', 'body',
         'switching AI models without redoing the work'
       ) AS s
  JOIN public.docs AS d ON d.id = s.pk_value::bigint
 ORDER BY s.rrf_score DESC;
```

:::: tip Expected
The engineering row ranks highly despite almost no keyword overlap.
`status().has_vector_index` is true.
::::

## 4. Remove the container

```bash
docker rm -f postvec
```

An existing cluster uses [packages](/docs/install/packages) then
[configure](/docs/install/setup). RDS, Aurora, Cloud SQL, Azure,
Supabase and Neon use [managed PostgreSQL](/docs/server/managed).

A populated vector column uses [`adopt()`](/docs/guides/adopt). For a
retired or provider-only space, [search that space](/docs/guides/bridge)
first; [`migrate()`](/docs/guides/migrate) is optional afterwards.

- [SQL functions](/docs/guides/)
- [BM25](/docs/guides/bm25)
- [Eventual consistency](/docs/concepts/consistency)
- [postvec-server](/docs/server/)

:::: info Release status
Commands use the current release. Every published release and its
artifacts are listed on the [release artifacts](/download) page.
::::
