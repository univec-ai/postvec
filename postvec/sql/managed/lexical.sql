-- SPDX-License-Identifier: PostgreSQL
-- Query-time BM25 over PostgreSQL text search. Loaded by CREATE EXTENSION
-- and by every managed install (tables IF NOT EXISTS, functions replaced).
-- Corpus stats are derived data: rebuilt by the worker when the table's
-- pg_stat tuple counters move, never dumped.

DROP FUNCTION IF EXISTS postvec._refresh_lexical_stats(bigint), postvec.refresh_lexical_stats(text, text),
    postvec._query_terms(regconfig, text), postvec._lexical_touch(bigint);
CREATE TABLE IF NOT EXISTS postvec.lexical_stats (
    registry_id  bigint PRIMARY KEY
                 REFERENCES postvec.registry(id) ON DELETE CASCADE,
    n            bigint NOT NULL DEFAULT 0,
    avgdl        float8 NOT NULL DEFAULT 1,
    mods         bigint,
    attempted_at timestamptz NOT NULL DEFAULT now(),
    refreshed_at timestamptz,
    error        text
);
CREATE TABLE IF NOT EXISTS postvec.lexical_df (
    registry_id bigint NOT NULL
                REFERENCES postvec.registry(id) ON DELETE CASCADE,
    term        text COLLATE "C" NOT NULL,
    df          integer NOT NULL,
    PRIMARY KEY (registry_id, term)
);
-- The role that called the outermost SQL, seen from inside SECURITY DEFINER.
CREATE OR REPLACE FUNCTION postvec._invoker() RETURNS name
LANGUAGE sql STABLE SET search_path = pg_catalog, pg_temp AS $$
    SELECT coalesce(nullif(current_setting('role', true), 'none'), session_user::text)::name
$$;

-- True when u sees every row of rel (RLS off, or u bypasses it).
CREATE OR REPLACE FUNCTION postvec._sees_full_corpus(rel regclass, u name)
RETURNS boolean
LANGUAGE sql STABLE SET search_path = pg_catalog, pg_temp AS $$
    SELECT NOT coalesce(c.relrowsecurity, true)
           OR coalesce(r.rolsuper OR r.rolbypassrls, false)
           OR (NOT c.relforcerowsecurity AND pg_has_role(u, c.relowner, 'USAGE'))
      FROM (SELECT rel AS oid) x
      LEFT JOIN pg_class c ON c.oid = x.oid
      LEFT JOIN pg_roles r ON r.rolname = u
$$;

GRANT SELECT ON postvec.lexical_stats TO PUBLIC;
ALTER TABLE postvec.lexical_stats ENABLE ROW LEVEL SECURITY;
DROP POLICY IF EXISTS lexical_stats_reader ON postvec.lexical_stats;
CREATE POLICY lexical_stats_reader ON postvec.lexical_stats FOR SELECT USING (
    EXISTS (SELECT FROM postvec.registry r JOIN pg_class c
                ON c.oid = to_regclass(format('%I.%I', r.table_schema, r.table_name))
             WHERE r.id = registry_id
               AND has_table_privilege(current_user, c.oid, 'SELECT')
               AND postvec._sees_full_corpus(c.oid, current_user))
);

-- Lucene IDF, k1 = 1.2, b = 0.75. dl counts token positions, as avgdl does.
CREATE OR REPLACE FUNCTION postvec.bm25_score(
    tsv tsvector, terms text[], dfs integer[], n bigint, avgdl float8)
RETURNS float8
LANGUAGE sql IMMUTABLE STRICT PARALLEL SAFE AS $$
    SELECT coalesce(pg_catalog.sum(
               pg_catalog.ln(1 + (n - least(df, n) + 0.5) / (least(df, n) + 0.5))
               * tf * 2.2 / (tf + 0.3 + 0.9 * dl / avgdl)), 0)
      FROM (SELECT lexeme,
                   coalesce(pg_catalog.array_length(positions, 1), 1)::float8 AS tf,
                   pg_catalog.sum(coalesce(pg_catalog.array_length(positions, 1), 1)) OVER () AS dl
              FROM pg_catalog.unnest(tsv)) d
      JOIN ROWS FROM (pg_catalog.unnest(terms), pg_catalog.unnest(dfs)) t(term, df) ON t.term = d.lexeme COLLATE "C"
$$;

CREATE OR REPLACE FUNCTION postvec.lexical_score(
    tsv tsvector, tsq tsquery, terms text[], dfs integer[], n bigint, avgdl float8)
RETURNS float8
LANGUAGE sql IMMUTABLE PARALLEL SAFE AS $$
    SELECT CASE WHEN n > 0 THEN postvec.bm25_score(tsv, terms, dfs, n, avgdl)
                ELSE pg_catalog.ts_rank_cd(tsv, tsq)::float8 END
$$;

-- Query lexemes with their df (1 when unknown) and the entry's corpus stats
-- (NULL before the first refresh: lexical_score then uses ts_rank_cd).
CREATE OR REPLACE FUNCTION postvec._lexical_terms(rid bigint, cfg regconfig, q text,
    OUT terms text[], OUT dfs integer[], OUT n bigint, OUT avgdl float8)
LANGUAGE plpgsql STABLE SECURITY DEFINER
SET search_path = pg_catalog, pg_temp AS $$
DECLARE rel regclass; token text; negate boolean := false; depth integer := 0;
BEGIN
    SELECT to_regclass(format('%I.%I', r.table_schema, r.table_name)) INTO rel
      FROM postvec.registry r WHERE r.id = rid;
    IF rel IS NULL OR NOT has_table_privilege(postvec._invoker(), rel, 'SELECT') THEN
        RAISE EXCEPTION 'postvec: permission denied for lexical stats of entry %', rid
            USING ERRCODE = 'insufficient_privilege';
    END IF;
    terms := '{}';
    FOR token IN SELECT m[1] FROM regexp_matches(
        websearch_to_tsquery(cfg, coalesce(q, ''))::text,
        $re$'(?:[^'\\]|\\.|'')*'|!|\(|\)$re$, 'g') m
    LOOP
        IF token = '!' THEN negate := true;
        ELSIF token = '(' THEN
            IF negate OR depth > 0 THEN depth := depth + 1; END IF;
            negate := false;
        ELSIF token = ')' THEN depth := greatest(depth - 1, 0);
        ELSE
            IF NOT negate AND depth = 0 THEN
                terms := terms || tsvector_to_array(token::tsvector);
            END IF;
            negate := false;
        END IF;
    END LOOP;
    SELECT coalesce(array_agg(DISTINCT t COLLATE "C" ORDER BY t COLLATE "C"), '{}')
      INTO terms FROM unnest(terms) t;
    -- Global frequencies reveal rows hidden by policies; use local ranking.
    IF NOT postvec._sees_full_corpus(rel, postvec._invoker()) THEN
        dfs := '{}'; RETURN;
    END IF;
    SELECT s.n, s.avgdl INTO n, avgdl
      FROM postvec.lexical_stats s WHERE s.registry_id = rid AND s.n > 0;
    SELECT coalesce(array_agg(coalesce(d.df, 1) ORDER BY u.ord), '{}') INTO dfs
      FROM unnest(terms) WITH ORDINALITY u(term, ord)
      LEFT JOIN postvec.lexical_df d ON d.registry_id = rid AND d.term = u.term;
END $$;

-- Inserted + updated + deleted tuples of a relation and its partitions.
CREATE OR REPLACE FUNCTION postvec._lexical_mods(rel regclass) RETURNS bigint
LANGUAGE sql STABLE STRICT SET search_path = pg_catalog, pg_temp AS $$
    WITH RECURSIVE t(oid) AS (
        SELECT rel UNION ALL SELECT i.inhrelid FROM pg_inherits i JOIN t ON i.inhparent = t.oid)
    SELECT sum(pg_stat_get_tuples_inserted(oid) + pg_stat_get_tuples_updated(oid)
               + pg_stat_get_tuples_deleted(oid))::bigint FROM t
$$;

-- The entry most overdue for a refresh: never refreshed, or changed since
-- and past its throttle (30 s, or ten times the last refresh's duration;
-- ten minutes after a failed attempt).
CREATE OR REPLACE FUNCTION postvec._lexical_stale() RETURNS bigint
LANGUAGE sql STABLE SET search_path = pg_catalog, pg_temp AS $$
    SELECT r.id FROM postvec.registry r
      LEFT JOIN postvec.lexical_stats s ON s.registry_id = r.id
     WHERE r.state <> 'disabled'
       AND (s.registry_id IS NULL
            OR (now() >= CASE WHEN s.error IS NULL
                     THEN s.refreshed_at + greatest(interval '30 seconds', 10 * (s.refreshed_at - s.attempted_at))
                     ELSE s.attempted_at + interval '10 minutes' END
                AND (s.error IS NOT NULL OR s.mods IS DISTINCT FROM postvec._lexical_mods(to_regclass(format('%I.%I',
                        coalesce(r.destination_schema, r.table_schema),
                        coalesce(r.destination_table, r.table_name)))))))
     ORDER BY s.refreshed_at NULLS FIRST, r.id LIMIT 1
$$;

-- One tokenization pass over the entry's text. Returns the error message
-- instead of raising, after recording it, so callers can back off.
CREATE OR REPLACE FUNCTION postvec._refresh_lexical_stats(rid bigint) RETURNS text
LANGUAGE plpgsql SECURITY DEFINER
SET search_path = pg_catalog, pg_temp SET row_security = off AS $$
DECLARE
    r postvec.registry; rel regclass; col text; mods bigint; docs bigint; tokens bigint;
    t0 timestamptz := clock_timestamp();
BEGIN
    -- Registry row before source table: disable() locks in that order, and
    -- the stats rows' FK check would otherwise wait on the row while holding
    -- the table.
    SELECT * INTO r FROM postvec.registry WHERE id = rid AND state <> 'disabled' FOR KEY SHARE;
    IF NOT FOUND THEN RETURN NULL; END IF;
    EXECUTE format('LOCK TABLE %I.%I IN ACCESS SHARE MODE', r.table_schema, r.table_name);
    SELECT * INTO r FROM postvec.registry WHERE id = rid AND state <> 'disabled';
    IF NOT FOUND THEN RETURN NULL; END IF;
    IF r.chunking = 'recursive' THEN
        rel := to_regclass(format('%I.%I', r.destination_schema, r.destination_table)); col := 'chunk_text';
    ELSE
        rel := to_regclass(format('%I.%I', r.table_schema, r.table_name)); col := r.source_column;
    END IF;
    IF rel IS NULL THEN RAISE EXCEPTION 'postvec: lexical relation is missing'; END IF;
    EXECUTE format('LOCK TABLE %s IN ACCESS SHARE MODE', rel);
    PERFORM pg_advisory_xact_lock(hashtextextended('postvec lexical ' || rid, 0));
    mods := postvec._lexical_mods(rel);
    DELETE FROM postvec.lexical_df WHERE registry_id = rid;
    EXECUTE format($sql$
        WITH s AS MATERIALIZED (
            SELECT lexeme COLLATE "C" AS word, count(*) AS ndoc,
                   sum(coalesce(array_length(positions, 1), 1)) AS nentry
              FROM %s CROSS JOIN LATERAL unnest(to_tsvector(%L::regconfig, %I::text))
             GROUP BY lexeme COLLATE "C"
        ), i AS (
            INSERT INTO postvec.lexical_df SELECT $1, word, ndoc FROM s WHERE ndoc > 1
        )
        SELECT (SELECT count(*) FROM %s WHERE %I IS NOT NULL),
               coalesce(sum(nentry), 0)::bigint FROM s
    $sql$, rel, r.fts_config, col, rel, col) INTO docs, tokens USING rid;
    INSERT INTO postvec.lexical_stats (registry_id, n, avgdl, mods, attempted_at, refreshed_at)
    VALUES (rid, docs, CASE WHEN docs > 0 AND tokens > 0 THEN tokens::float8 / docs ELSE 1 END,
            mods, t0, clock_timestamp())
    ON CONFLICT (registry_id) DO UPDATE
       SET n = excluded.n, avgdl = excluded.avgdl, mods = excluded.mods,
           attempted_at = excluded.attempted_at, refreshed_at = excluded.refreshed_at, error = NULL;
    RETURN NULL;
EXCEPTION WHEN OTHERS THEN
    INSERT INTO postvec.lexical_stats (registry_id, attempted_at, error)
    SELECT rid, clock_timestamp(), left(SQLERRM, 1024) FROM postvec.registry
     WHERE id = rid AND state <> 'disabled' FOR NO KEY UPDATE
    ON CONFLICT (registry_id) DO UPDATE SET attempted_at = excluded.attempted_at, error = excluded.error
        WHERE lexical_stats.attempted_at <= t0;
    RETURN SQLERRM;
END $$;
REVOKE ALL ON FUNCTION postvec._refresh_lexical_stats(bigint) FROM PUBLIC;

CREATE OR REPLACE FUNCTION postvec.refresh_lexical_stats(relation regclass, column_name text)
RETURNS void
LANGUAGE plpgsql SECURITY DEFINER
SET search_path = pg_catalog, pg_temp AS $$
DECLARE rid bigint; err text;
BEGIN
    IF NOT pg_has_role(postvec._invoker(), (SELECT relowner FROM pg_class WHERE oid = relation), 'USAGE') THEN
        RAISE EXCEPTION 'postvec: must own % to refresh its lexical stats', relation
            USING ERRCODE = 'insufficient_privilege';
    END IF;
    SELECT r.id INTO rid
      FROM postvec.registry r
      JOIN pg_class c ON c.oid = relation
      JOIN pg_namespace ns ON ns.oid = c.relnamespace
     WHERE r.table_schema = ns.nspname AND r.table_name = c.relname
       AND r.source_column = column_name AND r.state <> 'disabled';
    IF rid IS NULL THEN
        RAISE EXCEPTION 'postvec: %.% is not enabled', relation, column_name;
    END IF;
    err := postvec._refresh_lexical_stats(rid);
    IF err IS NOT NULL THEN RAISE EXCEPTION 'postvec: lexical stats refresh failed: %', err; END IF;
END $$;
