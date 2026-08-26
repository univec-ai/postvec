-- Manual hybrid search demo.
--
-- The RRF query shape that postvec.search() wraps: two LIMITed candidate
-- legs, fused on the document key. Handy as a standalone reference.
-- postvec.search() ranks the lexical leg with BM25 (ts_rank_cd until the
-- first refresh_lexical_stats / worker stats pass).
--
-- Prereqs:
--   * a running inference node with an embed model (set :model below)
--   * postvec.grpc_endpoints / _http_endpoints GUCs set
--   * CREATE EXTENSION postvec CASCADE; SELECT postvec.refresh_models();
--
-- Run:  psql -v model=snowflake-arctic-embed-l-v2.0 -f hybrid.sql

\set ON_ERROR_STOP on
\if :{?model}
\else
  \set model snowflake-arctic-embed-l-v2.0
\endif

BEGIN;

DROP TABLE IF EXISTS pv_demo_docs;
CREATE TABLE pv_demo_docs (
    id    bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
    title text NOT NULL,
    body  text NOT NULL
);

INSERT INTO pv_demo_docs (title, body) VALUES
  ('Q1 earnings',        'Quarterly revenue guidance was raised after strong subscription growth in the enterprise segment.'),
  ('Q2 earnings',        'Revenue missed expectations; management lowered full-year guidance citing weaker ad spend.'),
  ('Embedding lock-in',  'Migrating embedding models normally requires re-embedding all source text, which is slow and costly.'),
  ('Vector conversion',  'UniVec converts embeddings between incompatible vector spaces with high MRR retention.'),
  ('Postgres extension', 'postvec keeps shadow vector columns in sync via triggers, a queue table, and a background worker.'),
  ('Hybrid retrieval',   'Combining full-text search with vector KNN via reciprocal rank fusion improves recall on keyword-light queries.'),
  ('Database operations','Autovacuum tuning matters when wide vector rows are rewritten frequently under MVCC.'),
  ('Unrelated note',     'The office plants need watering twice a week, more often during summer.');

-- Shadow vector column, filled synchronously through postvec.embed()
-- (postvec.enable() automates exactly this via the trigger -> queue -> worker loop).
-- Dimension comes from the model cache.
SELECT target_dim AS dim FROM postvec.models WHERE name = :'model' \gset
ALTER TABLE pv_demo_docs ADD COLUMN body_semantic vector(:dim);

UPDATE pv_demo_docs
   SET body_semantic = postvec.embed(body, :'model')::vector;

-- FTS expression index (what enable(create_fts_index => true) will build)
CREATE INDEX pv_demo_docs_fts
    ON pv_demo_docs
 USING gin (to_tsvector('english', body));

COMMIT;

-- ---------------------------------------------------------------------------
-- The hybrid RRF query: two LIMITed candidate legs, FULL OUTER JOIN on
-- PK, weighted reciprocal-rank fusion w/(k+rank_sem) + (1-w)/(k+rank_fts).
-- ---------------------------------------------------------------------------
\set q 'revenue guidance for the quarter'
\set limit_n 5
\set candidates 20
\set rrf_k 60
\set w 0.5

WITH query_vec AS (
    SELECT postvec.embed(:'q', :'model')::vector AS v
),
semantic AS (
    SELECT d.id,
           row_number() OVER (ORDER BY d.body_semantic <=> (SELECT v FROM query_vec)) AS rank_sem
      FROM pv_demo_docs d
     WHERE d.body_semantic IS NOT NULL
     ORDER BY d.body_semantic <=> (SELECT v FROM query_vec)
     LIMIT :candidates
),
fts AS (
    SELECT d.id,
           row_number() OVER (
               ORDER BY ts_rank_cd(to_tsvector('english', d.body),
                                   websearch_to_tsquery('english', :'q')) DESC) AS rank_fts
      FROM pv_demo_docs d
     WHERE to_tsvector('english', d.body) @@ websearch_to_tsquery('english', :'q')
     ORDER BY ts_rank_cd(to_tsvector('english', d.body),
                         websearch_to_tsquery('english', :'q')) DESC
     LIMIT :candidates
),
fused AS (
    SELECT COALESCE(s.id, f.id) AS id,
           COALESCE(:w        / (:rrf_k + s.rank_sem), 0)
         + COALESCE((1 - :w)  / (:rrf_k + f.rank_fts), 0) AS rrf_score,
           s.rank_sem,
           f.rank_fts
      FROM semantic s
      FULL OUTER JOIN fts f USING (id)
)
SELECT d.id, d.title, round(fu.rrf_score::numeric, 5) AS rrf_score,
       fu.rank_sem, fu.rank_fts
  FROM fused fu
  JOIN pv_demo_docs d ON d.id = fu.id
 ORDER BY fu.rrf_score DESC
 LIMIT :limit_n;

-- A keyword-light query where the semantic leg must carry the result:
\set q 'switching to a different AI model without redoing the work'

WITH query_vec AS (
    SELECT postvec.embed(:'q', :'model')::vector AS v
),
semantic AS (
    SELECT d.id,
           row_number() OVER (ORDER BY d.body_semantic <=> (SELECT v FROM query_vec)) AS rank_sem
      FROM pv_demo_docs d
     WHERE d.body_semantic IS NOT NULL
     ORDER BY d.body_semantic <=> (SELECT v FROM query_vec)
     LIMIT :candidates
),
fts AS (
    SELECT d.id,
           row_number() OVER (
               ORDER BY ts_rank_cd(to_tsvector('english', d.body),
                                   websearch_to_tsquery('english', :'q')) DESC) AS rank_fts
      FROM pv_demo_docs d
     WHERE to_tsvector('english', d.body) @@ websearch_to_tsquery('english', :'q')
     ORDER BY ts_rank_cd(to_tsvector('english', d.body),
                         websearch_to_tsquery('english', :'q')) DESC
     LIMIT :candidates
),
fused AS (
    SELECT COALESCE(s.id, f.id) AS id,
           COALESCE(:w        / (:rrf_k + s.rank_sem), 0)
         + COALESCE((1 - :w)  / (:rrf_k + f.rank_fts), 0) AS rrf_score,
           s.rank_sem,
           f.rank_fts
      FROM semantic s
      FULL OUTER JOIN fts f USING (id)
)
SELECT d.id, d.title, round(fu.rrf_score::numeric, 5) AS rrf_score,
       fu.rank_sem, fu.rank_fts
  FROM fused fu
  JOIN pv_demo_docs d ON d.id = fu.id
 ORDER BY fu.rrf_score DESC
 LIMIT :limit_n;
