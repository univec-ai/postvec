-- SPDX-License-Identifier: PostgreSQL
CREATE OR REPLACE FUNCTION postvec.worker_kick() RETURNS void LANGUAGE sql
SET search_path = pg_catalog, pg_temp AS $$ SELECT pg_catalog.pg_notify('postvec_kick', '') $$;
CREATE OR REPLACE FUNCTION postvec.refresh_models() RETURNS void LANGUAGE sql
SET search_path = pg_catalog, pg_temp AS $$ SELECT pg_catalog.pg_notify('postvec_kick', 'refresh_models') $$;
REVOKE ALL ON FUNCTION postvec.refresh_models() FROM PUBLIC;

-- Served by the postvec-server proxy, which rewrites these calls before the
-- database sees them; reaching the function means the call did not go
-- through it.
DROP FUNCTION IF EXISTS postvec.search(text,text,text,integer,real,integer,integer,jsonb);
DROP FUNCTION IF EXISTS postvec.search_with_vector(text,text,real[],text,integer,real,integer,integer,jsonb);
CREATE OR REPLACE FUNCTION postvec.search(relation text, column_name text, query text, limit_n integer DEFAULT 10,
    semantic_weight real DEFAULT 0.5, rrf_k integer DEFAULT 60, candidates integer DEFAULT NULL, filter jsonb DEFAULT NULL)
RETURNS TABLE(pk_value text, rrf_score double precision, semantic_rank bigint, fts_rank bigint,
              semantic_distance double precision, fts_score double precision,
              chunk_seq integer, chunk_start bigint, chunk_end bigint, chunk_text text)
LANGUAGE plpgsql SET search_path = pg_catalog, pg_temp AS $$
BEGIN
    RAISE EXCEPTION 'postvec: search(text) needs the postvec-server proxy port with literal relation and column arguments and a literal or $n query text'
        USING ERRCODE = 'feature_not_supported',
              HINT = 'Connect through the proxy, or embed the query with the server API and call postvec.search_with_vector().';
END $$;
CREATE OR REPLACE FUNCTION postvec._proxy_error(message text) RETURNS void
LANGUAGE plpgsql SET search_path = pg_catalog, pg_temp AS $$
BEGIN RAISE EXCEPTION '%', message USING ERRCODE = 'feature_not_supported'; END $$;
CREATE OR REPLACE FUNCTION postvec.embed(input text, model text) RETURNS real[]
LANGUAGE plpgsql SET search_path = pg_catalog, pg_temp AS $$
BEGIN
    RAISE EXCEPTION 'postvec: embed() needs the postvec-server proxy port with a literal model and a literal or $n text'
        USING ERRCODE = 'feature_not_supported',
              HINT = 'Connect through the proxy, or use the server''s /api/openai/embeddings endpoint.';
END $$;

CREATE OR REPLACE FUNCTION postvec.retry_dead(relation regclass, column_name text, dead_ids bigint[] DEFAULT NULL)
RETURNS bigint LANGUAGE plpgsql SET search_path = pg_catalog, pg_temp AS $$
DECLARE r postvec.registry; picked bigint[]; consumed bigint; q text;
BEGIN
    IF NOT EXISTS (SELECT FROM pg_class c WHERE c.oid = relation AND c.relkind IN ('r','p')
                   AND pg_has_role(current_user, c.relowner, 'USAGE')) THEN
        RAISE EXCEPTION 'postvec: must own the source table to retry dead jobs';
    END IF;
    EXECUTE format('LOCK TABLE %s IN ACCESS SHARE MODE', relation);
    SELECT reg.* INTO STRICT r FROM postvec.registry reg JOIN pg_namespace n ON n.nspname = reg.table_schema
      JOIN pg_class c ON c.relnamespace = n.oid AND c.relname = reg.table_name
     WHERE c.oid = relation AND reg.source_column = column_name;
    IF r.chunking = 'recursive' THEN
        EXECUTE format('LOCK TABLE %I.%I IN ACCESS SHARE MODE', r.destination_schema, r.destination_table);
    END IF;
    SELECT reg.* INTO STRICT r FROM postvec.registry reg WHERE reg.id = r.id FOR UPDATE;
    IF r.state <> 'active' THEN RAISE EXCEPTION 'postvec: only active entries can retry dead jobs'; END IF;
    IF NOT EXISTS (SELECT FROM pg_trigger WHERE tgrelid = relation AND tgname = 'postvec_trunc_' || r.id) THEN
        RAISE EXCEPTION 'postvec: source identity trigger is missing';
    END IF;
    IF dead_ids IS NOT NULL THEN
        IF cardinality(dead_ids) = 0 OR array_position(dead_ids, NULL) IS NOT NULL THEN
            RAISE EXCEPTION 'postvec: dead_ids must be nonempty and contain no NULLs';
        END IF;
        SELECT array_agg(DISTINCT x) INTO picked FROM unnest(dead_ids) x;
        IF cardinality(picked) > 100000 OR (SELECT count(*) FROM postvec.jobs_dead
            WHERE registry_id = r.id AND dead_id = ANY(picked)) <> cardinality(picked) THEN
            RAISE EXCEPTION 'postvec: dead ids missing, belong to another entry, or exceed 100000';
        END IF;
    END IF;
    q := 'WITH pick AS (
        SELECT dead_id FROM postvec.jobs_dead WHERE registry_id = $1
          AND ($2::bigint[] IS NULL OR dead_id = ANY($2)) ORDER BY dead_id LIMIT 100000
    ), del AS (
        DELETE FROM postvec.jobs_dead d USING pick p WHERE d.dead_id = p.dead_id
        RETURNING d.pk_value, d.op, d.chunk_id
    ), ins AS (
        INSERT INTO postvec.jobs (registry_id, pk_value, op, chunk_id)
        SELECT DISTINCT $1, d.pk_value, d.op, d.chunk_id FROM del d WHERE ';
    IF r.chunking = 'recursive' THEN
        q := q || format('d.op = ''refresh'' OR EXISTS (SELECT FROM %I.%I c
            WHERE c.postvec_chunk_id = d.chunk_id AND c.postvec_source_pk = d.pk_value::%s)',
            r.destination_schema, r.destination_table, r.pk_types[1]);
    ELSE
        q := q || 'true';
    END IF;
    q := q || ' ON CONFLICT (registry_id, op, pk_value, chunk_id) WHERE claimed_at IS NULL DO NOTHING
        ) SELECT count(*) FROM del';
    EXECUTE q INTO consumed USING r.id, picked;
    IF picked IS NOT NULL AND consumed <> cardinality(picked) THEN
        RAISE EXCEPTION 'postvec: dead ids consumed concurrently; nothing retried';
    END IF;
    IF consumed = 100000 THEN RAISE NOTICE 'postvec: full retry batch consumed; call again for remaining jobs'; END IF;
    IF consumed > 0 THEN PERFORM postvec.worker_kick(); END IF;
    RETURN consumed;
END $$;
REVOKE ALL ON FUNCTION postvec.retry_dead(regclass, text, bigint[]) FROM PUBLIC;

CREATE OR REPLACE FUNCTION postvec.search_with_vector(
    relation text, column_name text, query_vector real[], query_text text DEFAULT '',
    limit_n integer DEFAULT 10, semantic_weight real DEFAULT 0.5, rrf_k integer DEFAULT 60,
    candidates integer DEFAULT NULL, filter jsonb DEFAULT NULL)
RETURNS TABLE(pk_value text, rrf_score double precision, semantic_rank bigint, fts_rank bigint,
              semantic_distance double precision, fts_score double precision,
              chunk_seq integer, chunk_start bigint, chunk_end bigint, chunk_text text)
LANGUAGE plpgsql SET hnsw.iterative_scan = 'relaxed_order' AS $$
DECLARE r postvec.registry; rel regclass; dest text; src text; pk text; cid text; fields text;
    vector_schema text; vec text; qvec text; op text; lex text; cfg text; pred text := ''; col text; cond jsonb;
    term jsonb; key text; typ text; category "char"; expr text; rhs text; items text[];
    values_json jsonb := '[]'; n integer := 0; cand integer; sql text; join_sql text;
BEGIN
    rel := pg_catalog.to_regclass(relation);
    IF rel IS NULL THEN RAISE EXCEPTION 'postvec: relation does not exist'; END IF;
    EXECUTE pg_catalog.format('LOCK TABLE %s IN ACCESS SHARE MODE', rel);
    SELECT reg.* INTO STRICT r FROM postvec.registry reg
      JOIN pg_catalog.pg_namespace ns ON ns.nspname = reg.table_schema
      JOIN pg_catalog.pg_class c ON c.relnamespace = ns.oid AND c.relname = reg.table_name
     WHERE c.oid = rel AND reg.source_column = column_name AND reg.state IN ('active','migrating');
    IF r.trigger_mode = 'none' AND NOT EXISTS (SELECT FROM pg_catalog.pg_trigger
        WHERE tgrelid = rel AND tgname = 'postvec_trunc_' || r.id) THEN
        RAISE EXCEPTION 'postvec: source identity trigger is missing';
    END IF;
    IF query_vector IS NULL OR cardinality(query_vector) <> r.dim OR array_ndims(query_vector) <> 1
       OR EXISTS (SELECT FROM unnest(query_vector) v WHERE v IS NULL OR v::text IN ('NaN','Infinity','-Infinity')) THEN
        RAISE EXCEPTION 'postvec: query vector must contain % finite components', r.dim;
    END IF;
    IF limit_n IS NULL OR limit_n NOT BETWEEN 1 AND 1000 THEN RAISE EXCEPTION 'postvec: limit_n must be between 1 and 1000'; END IF;
    cand := COALESCE(candidates, greatest(limit_n * CASE WHEN r.chunking = 'recursive' THEN 16 ELSE 4 END,
                                         CASE WHEN r.chunking = 'recursive' THEN 200 ELSE 50 END));
    IF limit_n IS NULL OR limit_n < 1 OR limit_n > 1000 OR cand < 1 OR cand > 100000
       OR rrf_k IS NULL OR rrf_k < 1 OR semantic_weight IS NULL OR NOT (semantic_weight BETWEEN 0 AND 1)
       OR query_text IS NULL OR octet_length(query_text) > 16777216 THEN
        RAISE EXCEPTION 'postvec: invalid search arguments';
    END IF;
    IF filter IS NOT NULL AND (pg_catalog.jsonb_typeof(filter) <> 'object' OR octet_length(filter::text) > 65536) THEN
        RAISE EXCEPTION 'postvec: filter must be an object of at most 65536 bytes';
    END IF;
    IF (SELECT count(*) FROM pg_catalog.jsonb_object_keys(filter)) > 32 THEN RAISE EXCEPTION 'postvec: filter exceeds 32 columns'; END IF;
    FOR col, cond IN SELECT * FROM pg_catalog.jsonb_each(filter) LOOP
        WITH RECURSIVE att AS (
            SELECT a.atttypid, pg_catalog.format_type(a.atttypid,a.atttypmod) AS declared
              FROM pg_catalog.pg_attribute a WHERE a.attrelid=rel AND a.attname=col AND a.attnum>0 AND NOT a.attisdropped
        ), walk AS (
            SELECT t.oid,t.typbasetype,t.typcategory FROM pg_catalog.pg_type t JOIN att ON t.oid=att.atttypid
            UNION ALL SELECT t.oid,t.typbasetype,t.typcategory FROM pg_catalog.pg_type t JOIN walk w ON t.oid=w.typbasetype
        ) SELECT att.declared,w.typcategory INTO typ,category FROM att,walk w WHERE w.typbasetype=0;
        IF NOT FOUND THEN RAISE EXCEPTION 'postvec: unknown filter column %', col; END IF;
        expr := pg_catalog.format('d.%I', col);
        IF pg_catalog.jsonb_typeof(cond) = 'array' THEN cond := pg_catalog.jsonb_build_object('in', cond);
        ELSIF pg_catalog.jsonb_typeof(cond) <> 'object' THEN cond := pg_catalog.jsonb_build_object('eq', cond);
        ELSIF cond ? 'eq' THEN RAISE EXCEPTION 'postvec: use a scalar for equality'; END IF;
        IF cond = '{}'::jsonb THEN RAISE EXCEPTION 'postvec: empty filter condition'; END IF;
        FOR key, term IN SELECT * FROM pg_catalog.jsonb_each(cond) LOOP
            IF key = 'eq' AND term = 'null'::jsonb THEN pred := pred || ' AND ' || expr || ' IS NULL'; CONTINUE; END IF;
            IF key = 'is_not' THEN
                IF term <> 'null'::jsonb THEN RAISE EXCEPTION 'postvec: is_not accepts only null'; END IF;
                pred := pred || ' AND ' || expr || ' IS NOT NULL'; CONTINUE;
            END IF;
            IF key = 'in' THEN
                IF pg_catalog.jsonb_typeof(term) <> 'array' OR pg_catalog.jsonb_array_length(term) = 0 OR pg_catalog.jsonb_array_length(term) > 256 THEN
                    RAISE EXCEPTION 'postvec: in requires 1..256 scalar values';
                END IF;
            ELSIF key NOT IN ('eq','neq','gt','gte','lt','lte','like','ilike') THEN
                RAISE EXCEPTION 'postvec: unknown filter operator %', key;
            ELSE term := pg_catalog.jsonb_build_array(term);
            END IF;
            items := ARRAY[]::text[];
            FOR cond IN SELECT value FROM pg_catalog.jsonb_array_elements(term) LOOP
                IF pg_catalog.jsonb_typeof(cond) NOT IN ('string','number','boolean') THEN
                    RAISE EXCEPTION 'postvec: filter values must be non-NULL scalars';
                END IF;
                IF key IN ('like','ilike') AND (category <> 'S' OR pg_catalog.jsonb_typeof(cond) <> 'string') THEN
                    RAISE EXCEPTION 'postvec: like/ilike requires a string column and pattern';
                END IF;
                IF NOT pg_catalog.pg_input_is_valid(cond #>> '{}', typ) THEN
                    RAISE EXCEPTION 'postvec: invalid filter value for column % (%)', col, typ;
                END IF;
                rhs := pg_catalog.format('($3->>%s)::%s', n, CASE WHEN key IN ('like','ilike') THEN 'text' ELSE typ END);
                values_json := values_json || pg_catalog.jsonb_build_array(cond); n := n + 1;
                items := array_append(items, rhs);
            END LOOP;
            pred := pred || ' AND ' || expr || CASE key WHEN 'in' THEN ' IN (' || array_to_string(items, ',') || ')'
              ELSE ' ' || CASE key WHEN 'eq' THEN '=' WHEN 'neq' THEN '<>' WHEN 'gt' THEN '>' WHEN 'gte' THEN '>='
              WHEN 'lt' THEN '<' WHEN 'lte' THEN '<=' WHEN 'like' THEN 'LIKE' ELSE 'ILIKE' END || ' ' || items[1] END;
        END LOOP;
    END LOOP;
    src := pg_catalog.format('%I.%I', r.table_schema, r.table_name);
    cfg := pg_catalog.format('%L::regconfig', r.fts_config);
    IF r.chunking = 'recursive' THEN
        dest := pg_catalog.format('%I.%I', r.destination_schema, r.destination_table);
        join_sql := pg_catalog.format('%s c JOIN %s d ON d.%I = c.postvec_source_pk', dest, src, r.pk_columns[1]);
        fields := 'c.postvec_chunk_id AS cid, c.postvec_source_pk::text AS pk,
                   c.postvec_chunk_seq AS seq, c.postvec_char_start AS cs, c.postvec_char_end AS ce';
        vec := pg_catalog.format('c.%I', r.vector_column); lex := 'c.chunk_text';
    ELSE
        SELECT CASE WHEN cardinality(r.pk_columns) = 1 THEN pg_catalog.format('d.%I::text', r.pk_columns[1])
               ELSE 'ROW(' || string_agg(pg_catalog.format('d.%I', x), ',' ORDER BY ord) || ')::text' END
          INTO pk FROM unnest(r.pk_columns) WITH ORDINALITY u(x, ord);
        fields := pk || ' AS cid, ' || pk || ' AS pk, NULL::integer AS seq, NULL::bigint AS cs, NULL::bigint AS ce';
        join_sql := src || ' d'; vec := pg_catalog.format('d.%I', r.vector_column);
        lex := pg_catalog.format('d.%I::text', r.source_column);
    END IF;
    SELECT n.nspname INTO STRICT vector_schema FROM pg_catalog.pg_extension e
      JOIN pg_catalog.pg_namespace n ON n.oid=e.extnamespace WHERE e.extname='vector';
    qvec := pg_catalog.format('$1::%I.vector', vector_schema);
    IF r.dim > 2000 THEN
        vec := pg_catalog.format('(%s::%I.halfvec(%s))', vec, vector_schema, r.dim);
        qvec := pg_catalog.format('$1::%I.halfvec(%s)', vector_schema, r.dim);
    END IF;
    op := pg_catalog.format('OPERATOR(%I.%s)', vector_schema, CASE r.distance WHEN 'l2' THEN '<->' WHEN 'ip' THEN '<#>' ELSE '<=>' END);
    sql := pg_catalog.format('WITH stats AS (
        SELECT * FROM postvec._lexical_terms(%s, %s, $2)
    ), semantic AS (
        SELECT %s, (%s %s %s)::float8 AS dist, row_number() OVER (ORDER BY %s %s %s) AS rank_sem FROM %s
         WHERE %s IS NOT NULL %s ORDER BY %s %s %s LIMIT %s
    ), fts AS (
        SELECT *, row_number() OVER (ORDER BY score_fts DESC, cid) AS rank_fts
          FROM (
            SELECT %s, postvec.lexical_score(pg_catalog.to_tsvector(%s, %s), pg_catalog.websearch_to_tsquery(%s, $2),
                                             st.terms, st.dfs, st.n, st.avgdl) AS score_fts
              FROM %s, stats st
             WHERE pg_catalog.to_tsvector(%s, %s) @@ pg_catalog.websearch_to_tsquery(%s, $2) %s
             ORDER BY score_fts DESC, cid LIMIT %s
          ) scored
    ), fused AS (
        SELECT coalesce(s.cid,f.cid) AS cid, coalesce(s.pk,f.pk) AS pk,
               coalesce(s.seq,f.seq) AS seq, coalesce(s.cs,f.cs) AS cs, coalesce(s.ce,f.ce) AS ce,
               coalesce(%s::float8/(%s+s.rank_sem),0) + coalesce((1-%s::float8)/(%s+f.rank_fts),0) AS score,
               s.rank_sem, f.rank_fts, s.dist, f.score_fts FROM semantic s FULL JOIN fts f USING(cid)
    ), ranked AS (
        SELECT *, row_number() OVER (PARTITION BY pk ORDER BY score DESC,
            coalesce(rank_sem,9223372036854775807), coalesce(rank_fts,9223372036854775807), seq, cid) AS rn FROM fused
    ), winners AS (
        SELECT * FROM ranked WHERE rn=1 ORDER BY score DESC NULLS LAST, pk LIMIT %s
    ) SELECT w.pk,w.score,w.rank_sem,w.rank_fts,w.dist,w.score_fts,w.seq,w.cs,w.ce,',
        r.id, cfg,
        fields,vec,op,qvec,vec,op,qvec,join_sql,vec,pred,vec,op,qvec,cand,
        fields,cfg,lex,cfg,join_sql,cfg,lex,cfg,pred,cand,
        semantic_weight,rrf_k,semantic_weight,rrf_k,limit_n);
    IF r.chunking = 'recursive' THEN
        sql := sql || pg_catalog.format('CASE WHEN sum(octet_length(c.chunk_text)) OVER (
            ORDER BY w.score DESC NULLS LAST, w.pk ROWS UNBOUNDED PRECEDING) <= 67108864 THEN c.chunk_text END
            FROM winners w LEFT JOIN %s c ON c.postvec_chunk_id = w.cid::bigint ORDER BY w.score DESC NULLS LAST, w.pk', dest);
    ELSE sql := sql || 'NULL::text FROM winners w ORDER BY w.score DESC NULLS LAST, w.pk';
    END IF;
    RETURN QUERY EXECUTE sql USING '[' || array_to_string(query_vector, ',') || ']', query_text, values_json;
END $$;

CREATE OR REPLACE FUNCTION postvec._has_vector_index(target regclass, col text) RETURNS boolean
LANGUAGE sql STABLE SET search_path = pg_catalog, pg_temp AS $$
    SELECT EXISTS (
        SELECT FROM pg_index i JOIN pg_class ic ON ic.oid = i.indexrelid JOIN pg_am am ON am.oid = ic.relam
         WHERE i.indrelid = target AND am.amname IN ('hnsw', 'ivfflat')
           AND i.indisvalid AND i.indisready AND i.indislive
           AND (EXISTS (SELECT FROM pg_attribute a WHERE a.attrelid = i.indrelid AND a.attnum = ANY(i.indkey) AND a.attname = col)
                OR EXISTS (SELECT FROM pg_depend d JOIN pg_attribute a ON a.attrelid = i.indrelid AND a.attnum = d.refobjsubid
                            WHERE d.classid = 'pg_class'::regclass AND d.objid = i.indexrelid
                              AND d.refclassid = 'pg_class'::regclass AND d.refobjid = i.indrelid AND a.attname = col)))
$$;

CREATE OR REPLACE FUNCTION postvec.convert(embedding real[], source_model text, target_model text) RETURNS real[]
LANGUAGE plpgsql SET search_path = pg_catalog, pg_temp AS $$
BEGIN
    RAISE EXCEPTION 'postvec: convert() is not available on managed PostgreSQL'
        USING ERRCODE = 'feature_not_supported',
              HINT = 'POST {"source_model","target_model","embeddings"} to the postvec-server /api/convert endpoint.';
END $$;

DROP FUNCTION IF EXISTS postvec.status();
CREATE OR REPLACE FUNCTION postvec.status() RETURNS TABLE(worker_alive boolean, registry_id bigint, relation text, source_column text, model text, dim integer, state text, distance text, backfill_mode text, pending_jobs bigint, dead_jobs bigint, oldest_pending_seconds double precision, has_vector_index boolean, model_last_seen text, last_error text, worker_pid integer, worker_last_beat text, index_mode text, index_error text, chunking text, chunk_size integer, chunk_overlap integer, destination text, destination_view text, pending_refresh_jobs bigint, pending_embed_jobs bigint, lexical_docs bigint, lexical_stats_age_seconds double precision, lexical_error text, space text, route text, route_execution text)
LANGUAGE sql SET search_path = pg_catalog, pg_temp AS $$
SELECT COALESCE(hb.last_beat > now() - interval '30 seconds', false), r.id,
                        r.table_schema || '.' || r.table_name AS relation,
                        r.source_column, r.model, r.dim, r.state, r.distance,
                        r.backfill_mode,
                        COALESCE(j.pending, 0)::bigint,
                        COALESCE(jd.dead, 0)::bigint,
                        EXTRACT(EPOCH FROM (now() - j.oldest))::float8,
                        postvec._has_vector_index(to_regclass(format('%I.%I', COALESCE(r.destination_schema,r.table_schema), COALESCE(r.destination_table,r.table_name))), r.vector_column) AS has_vector_index,
                        mm.last_seen::text,
                        je.last_error,
                        hb.pid, hb.last_beat::text,
                        r.index_mode, r.index_error,
                        r.chunking, r.chunk_size, r.chunk_overlap,
                        CASE WHEN r.destination_table IS NOT NULL
                             THEN r.destination_schema || '.' || r.destination_table
                        END AS destination,
                        CASE WHEN r.destination_view IS NOT NULL
                             THEN r.destination_schema || '.' || r.destination_view
                        END AS destination_view,
                        COALESCE(j.pending_refresh, 0)::bigint,
                        COALESCE(j.pending_embed, 0)::bigint,
                        COALESCE(ls.n, 0)::bigint,
                        EXTRACT(EPOCH FROM (now() - ls.refreshed_at))::float8,
                        ls.error,
                        COALESCE(mm.space, r.space),
                        mm.route,
                        mm.route_execution
                   FROM postvec.registry r
                   LEFT JOIN postvec.lexical_stats ls ON ls.registry_id = r.id
                   LEFT JOIN (
                        SELECT registry_id, count(*) AS pending, min(created_at) AS oldest,
                               count(*) FILTER (WHERE op = 'refresh') AS pending_refresh,
                               count(*) FILTER (WHERE op = 'embed') AS pending_embed
                          FROM postvec.jobs GROUP BY registry_id
                   ) j ON j.registry_id = r.id
                   LEFT JOIN LATERAL (
                        -- Most recent error via a top-1 ordered fetch.
                        -- array_agg(... ORDER BY) would materialize and sort
                        -- every pending error string per group just to take
                        -- element [1], which spikes memory on a large backlog.
                        SELECT last_error FROM postvec.jobs
                         WHERE registry_id = r.id AND last_error IS NOT NULL
                         ORDER BY not_before DESC
                         LIMIT 1
                   ) je ON true
                   LEFT JOIN (
                        SELECT registry_id, count(*) AS dead
                          FROM postvec.jobs_dead GROUP BY registry_id
                   ) jd ON jd.registry_id = r.id
                   LEFT JOIN LATERAL (
                        -- The served embed route, else the converter that
                        -- targets the entry's space (an embed-bridge entry).
                        SELECT last_seen, space, route, route_execution FROM (
                            SELECT 0 AS tier, last_seen, space, route, execution AS route_execution
                              FROM postvec._route(r.model, r.space)
                            UNION ALL
                            SELECT 1, last_seen, target_model, name, 'bridge'
                              FROM postvec.models
                             WHERE model_type = 'convert' AND target_model IN (r.model, r.space)
                        ) x ORDER BY tier, route LIMIT 1
                   ) mm ON true
                   LEFT JOIN (SELECT pid, last_beat FROM postvec.worker_heartbeat LIMIT 1) hb ON true
                  ORDER BY r.id
$$;

CREATE OR REPLACE FUNCTION postvec.stats() RETURNS TABLE(worker_pid integer, worker_started_at text, worker_last_beat text, jobs_embedded bigint, jobs_nulled bigint, jobs_retried bigint, jobs_dead_lettered bigint, migration_rows_converted bigint, migration_rows_skipped bigint, model_refreshes bigint, worker_errors bigint, worker_last_error text, queue_pending bigint, queue_claimed bigint, queue_dead bigint, migrations_running bigint, documents_chunked bigint, chunks_created bigint)
LANGUAGE sql SET search_path = pg_catalog, pg_temp AS $$
SELECT hb.pid, hb.started_at::text, hb.last_beat::text,
                        COALESCE(hb.jobs_embedded, 0), COALESCE(hb.jobs_nulled, 0),
                        COALESCE(hb.jobs_retried, 0), COALESCE(hb.jobs_dead, 0),
                        COALESCE(hb.rows_converted, 0), COALESCE(hb.rows_skipped, 0),
                        COALESCE(hb.model_refreshes, 0), COALESCE(hb.errors, 0),
                        hb.last_error,
                        (SELECT count(*) FROM postvec.jobs WHERE claimed_at IS NULL),
                        (SELECT count(*) FROM postvec.jobs WHERE claimed_at IS NOT NULL),
                        (SELECT count(*) FROM postvec.jobs_dead),
                        (SELECT count(*) FROM postvec.migrations WHERE state = 'running'),
                        COALESCE(hb.documents_chunked, 0), COALESCE(hb.chunks_created, 0)
                   FROM (SELECT 1) one
                   LEFT JOIN (SELECT * FROM postvec.worker_heartbeat LIMIT 1) hb ON true
$$;

CREATE OR REPLACE FUNCTION postvec.migration_status(migration_id bigint DEFAULT NULL) RETURNS TABLE(migration_id bigint, registry_id bigint, relation text, source_column text, old_model text, new_model text, strategy text, resolved_via text, state text, rows_total bigint, rows_done bigint, rows_skipped bigint, progress_pct double precision, error text, started_at text, finished_at text, suggested_index_sql text)
LANGUAGE sql SET search_path = pg_catalog, pg_temp AS $$
SELECT m.id, m.registry_id,
                        r.table_schema || '.' || r.table_name,
                        r.source_column,
                        m.old_model, m.new_model, m.strategy,
                        m.resolved_via::text, m.state,
                        m.rows_total, m.rows_done, m.rows_skipped,
                        CASE WHEN m.rows_total > 0
                             -- capped: fresh writes routed to the new column
                             -- shrink the driver's share of rows_total
                             THEN LEAST(100.0, round(100.0 * (m.rows_done + m.rows_skipped)
                                        / m.rows_total, 1))::float8
                             ELSE NULL END,
                        m.error, m.started_at::text, m.finished_at::text,
                        CASE WHEN m.state = 'awaiting_index' THEN format(
                            'CREATE INDEX CONCURRENTLY ON %I.%I USING hnsw (%s %s);',
                            COALESCE(r.destination_schema,r.table_schema), COALESCE(r.destination_table,r.table_name),
                            CASE WHEN r.dim > 2000 THEN format('(%I::%I.halfvec(%s))',r.vector_column,vns.nspname,r.dim)
                                 ELSE quote_ident(r.vector_column) END,
                            quote_ident(vns.nspname) || '.' || CASE WHEN r.dim > 2000 THEN 'halfvec_' ELSE 'vector_' END ||
                            CASE r.distance WHEN 'l2' THEN 'l2_ops' WHEN 'ip' THEN 'ip_ops' ELSE 'cosine_ops' END)
                        END
                   FROM postvec.migrations m
                   JOIN postvec.registry r ON r.id = m.registry_id
                   JOIN pg_catalog.pg_extension ve ON ve.extname='vector'
                   JOIN pg_catalog.pg_namespace vns ON vns.oid=ve.extnamespace
                  WHERE $1::bigint IS NULL OR m.id = $1
                  ORDER BY m.id
$$;
