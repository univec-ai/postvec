---
title: Quick start
description: Hybrid search in a postvec container. Pick PostgreSQL 16, 17 or 18.
---

# Quick start

A container with PostgreSQL, postvec and the bundled MiniLM model.

Start the image, enable a column, wait for vectors, index, search, then
remove the container.

:::: info Release status
Commands use the planned `0.1.0-1` image. Publication status is listed
with the [release artifacts](/download). An unpublished tag requires a
local image build or an existing development package.
::::

## 1. Run the local image

<PgSnippet id="docker-quickstart" />

Wait until health is `healthy` (PostgreSQL is up and MiniLM is loaded):

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

Vectors fill **asynchronously**. After the INSERT commits, wait until
`pending_jobs = 0` and every `body_semantic` is non-NULL:

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

```sql
SELECT postvec.create_vector_index('public.docs', 'body');

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

- [Install](/docs/install/)
- [SQL functions](/docs/guides/starting)
- [Eventual consistency](/docs/concepts/consistency)
