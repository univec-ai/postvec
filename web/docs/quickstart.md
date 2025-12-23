---
title: Quick start
description: Hybrid search in a disposable postvec container. Pick PostgreSQL 16, 17 or 18.
---

# Quick start

A single disposable container is enough for this walkthrough. The host
PostgreSQL cluster is left alone. No API key is involved. One example
table is created.

The tabs on each command pick the PostgreSQL major. The same choice is
remembered on the install pages.

:::: info Release status
Commands use the planned `0.1.0-1` image. Publication status is listed
with the [release artifacts](/download). An unpublished tag requires a
local image build or an existing development package.
::::

## 1. Run the embedded image

<PgSnippet id="docker-quickstart" />

Wait until it is healthy:

<div v-pre>

```bash
docker inspect --format '{{.State.Health.Status}}' postvec
docker exec postvec postvec-healthcheck
```

</div>

:::: tip Expected
Health becomes `healthy` after PostgreSQL starts and the bundled MiniLM
model loads. `postvec-healthcheck` exits 0.
::::

The image creates the extension in `POSTGRES_DB` on first initialization
only.

## 2. Verify inference

```bash
docker exec -i postvec psql -U app -d app <<'SQL'
SELECT name, model_type, target_dim
  FROM postvec.models
 ORDER BY name;

SELECT vector_dims(postvec.embed(
  'the isolated image performs inference',
  'sentence-transformers-all-minilm-l6-v2'
)::vector);
SQL
```

:::: tip Expected
The bundled model is listed. `vector_dims` returns **384**.
::::

`embed()` is an administrative helper. Application roles require an
explicit `GRANT`. Search is the public query surface.

## 3. Enable a column and insert

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

Vectors fill **asynchronously**. A committed write arms an at-commit
latch, so the worker normally starts within inference time of the
commit. Repeat until `pending_jobs = 0` and every `body_semantic` is
non-NULL:

```sql
SELECT count(*) FILTER (WHERE body_semantic IS NOT NULL) AS filled,
       count(*) AS total
  FROM docs;
```

:::: tip Expected
`filled = total`, `pending_jobs = 0`, `dead_jobs = 0`. On this image
that is usually a second or two. `postvec.poll_interval_ms` (default
5000) is only the backstop if the worker restarted between the latch
being armed and the commit.
::::

## 4. Index and search

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

`search()` returns the primary key as **text**, which joins back to the
source table without a dynamic record type.

## 5. Remove the container

```bash
docker rm -f postvec
```

That removes the disposable container. No host files or PostgreSQL
cluster configuration were created.

## Next

- [Install on a real cluster](/docs/install/)
- [Choose the SQL call](/docs/guides/starting) - enable, adopt, bridge or migrate
- [How the worker fills vectors](/docs/concepts/consistency)
