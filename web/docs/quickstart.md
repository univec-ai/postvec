---
title: Quick start
description: Get hybrid search working in a disposable postvec container.
---

# Quick start

This path never touches a host PostgreSQL cluster. It is the right first
contact: one container, no API key, one table, one search.

## 1. Run the embedded image

```bash
docker run -d --name postvec \
  -e POSTGRES_PASSWORD=demo \
  -e POSTGRES_USER=app \
  -e POSTGRES_DB=app \
  -p 127.0.0.1:5433:5432 \
  ghcr.io/univec-ai/postvec:0.1.0-1-pg18-embedded
```

Wait until it is healthy:

<div v-pre>

```bash
docker inspect --format '{{.State.Health.Status}}' postvec
docker exec postvec postvec-healthcheck
```

</div>

::: tip Expected
Health becomes `healthy` after PostgreSQL starts and the bundled MiniLM
model loads. `postvec-healthcheck` exits 0.
:::

The image creates the extension in `POSTGRES_DB` on first initialization only.

## 2. Prove inference

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

::: tip Expected
The bundled model is listed. `vector_dims` returns **384**.
:::

`embed()` is an administrative helper. Application roles do not have it
until you `GRANT` it. Search is the public query surface.

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

Vectors fill **asynchronously**. Repeat until `pending_jobs = 0` and every
`body_semantic` is non-NULL:

```sql
SELECT count(*) FILTER (WHERE body_semantic IS NOT NULL) AS filled,
       count(*) AS total
  FROM docs;
```

::: tip Expected
`filled = total`, `pending_jobs = 0`, `dead_jobs = 0`. This usually takes
one poll interval plus inference time (about a second here).
:::

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

::: tip Expected
The engineering row ranks highly despite almost no keyword overlap.
`status().has_vector_index` is true.
:::

`search()` returns the primary key as **text**. You join back. That is
intentional — it avoids dynamic record types.

## 5. Throw it away

```bash
docker rm -f postvec
```

No host files, no cluster configuration, no leftover database.

## Next

- [Install on a real cluster](/docs/install/)
- [How the worker actually fills vectors](/docs/concepts/consistency)
- [Usage guides](/docs/guides/) — filters, templates, chunking, migrate, adopt
