---
title: Quick start remote
description: postvec-server and a remote-mode PostgreSQL container on one Docker network. Pick PostgreSQL 16, 17 or 18.
---

# Quick start remote

Two containers on one Docker network:

- PostgreSQL with the postvec extension in [remote mode](/docs/concepts/modes)
- [postvec-server](/docs/server/) for inference, with
  [MiniLM](https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2)
  already loaded

SQL is the same as [quick start local](/docs/quickstart). The server
container generates a self-signed certificate at start; the extension
accepts it. The hostname `postvec-server` is both the container name and
a SAN on that certificate.

## 1. Start both containers

<PgSnippet id="docker-quickstart-remote" />

Wait until both are `healthy`. Start the server first: PostgreSQL
reads models at boot, and MiniLM still loading leaves `postvec.models`
empty until the next refresh (`postvec.model_refresh_interval_ms`, 60 s).

<div v-pre>

```bash
docker inspect --format '{{.State.Health.Status}}' postvec-server
docker inspect --format '{{.State.Health.Status}}' postvec
```

</div>

::::: tip Expected
`postvec-server` is `healthy` once MiniLM can answer (`GET /ready`).
`postvec` is `healthy` once PostgreSQL is up and the worker heartbeat
advances. The remote image creates the extension in `POSTGRES_DB` on
first initialization only.
:::::

:::: info Optional
```bash
docker exec postvec-server postvec-server status
docker exec postvec postvec-healthcheck
curl -sk https://127.0.0.1:22222/ready
```

`status` lists the loaded model. `/ready` is `200` once it can serve.
The dashboard is `https://127.0.0.1:22222` (self-signed).
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

Vectors fill **asynchronously** on postvec-server. After INSERT
commits, wait until `pending_jobs = 0` and every `body_semantic` is
non-NULL:

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
`filled = total`, `pending_jobs = 0`, `dead_jobs = 0`. MiniLM on the
server is usually a second or two.
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
`status().has_vector_index` is true. `postvec.models` lists MiniLM from
the server's `/config`.
::::

## 4. Remove the containers

```bash
docker rm -f postvec postvec-server
docker network rm postvec
```

A lasting install uses [Docker](/docs/server/docker) or
[packages](/docs/server/packages) for postvec-server, then
[connect PostgreSQL](/docs/server/connect). RDS, Aurora, Cloud SQL,
Azure, Supabase and Neon use [managed PostgreSQL](/docs/server/managed).

- [Quick start local](/docs/quickstart)
- [SQL functions](/docs/guides/)
- [Dashboard](/docs/server/dashboard)
- [When to use postvec-server](/docs/server/usage)
