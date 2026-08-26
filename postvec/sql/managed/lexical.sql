-- SPDX-License-Identifier: PostgreSQL
-- Query-time BM25 over PostgreSQL text search. Loaded by CREATE EXTENSION
-- and by every managed install (tables IF NOT EXISTS, functions replaced).

CREATE TABLE IF NOT EXISTS postvec.lexical_stats (
    registry_id  bigint PRIMARY KEY
                 REFERENCES postvec.registry(id) ON DELETE CASCADE,
    n            bigint NOT NULL DEFAULT 0,
    avgdl        float8 NOT NULL DEFAULT 1,
    refreshed_at timestamptz,
    dirty_at     timestamptz
);
CREATE TABLE IF NOT EXISTS postvec.lexical_df (
    registry_id bigint NOT NULL
                REFERENCES postvec.registry(id) ON DELETE CASCADE,
    term        text NOT NULL,
    df          integer NOT NULL CHECK (df > 0),
    PRIMARY KEY (registry_id, term)
);
GRANT SELECT ON postvec.lexical_stats, postvec.lexical_df TO PUBLIC;

CREATE OR REPLACE FUNCTION postvec._query_terms(cfg regconfig, q text)
RETURNS text[]
LANGUAGE sql STABLE
SET search_path = pg_catalog, pg_temp AS $$
    SELECT COALESCE(ARRAY(
        SELECT replace(m[1], chr(39)||chr(39), chr(39))
          FROM regexp_matches(
                 strip(to_tsvector(cfg, COALESCE(q, '')))::text,
                 '''((?:[^'']|'''')+)''',
                 'g') AS m), ARRAY[]::text[])
$$;

CREATE OR REPLACE FUNCTION postvec.bm25_score(
    tsv tsvector, terms text[], dfs integer[], n bigint, avgdl float8)
RETURNS float8
LANGUAGE plpgsql IMMUTABLE
SET search_path = pg_catalog, pg_temp AS $$
DECLARE
    t text; i int; tf float8; dl float8; score float8 := 0; idf float8; df int;
    repr text; esc text; pos int; rest text; j int; poslist text;
    k1 float8 := 1.2; b float8 := 0.75;
BEGIN
    IF tsv IS NULL OR terms IS NULL OR n IS NULL OR n <= 0
       OR avgdl IS NULL OR avgdl <= 0 THEN
        RETURN 0;
    END IF;
    dl := length(tsv);
    IF dl <= 0 THEN RETURN 0; END IF;
    repr := tsv::text;
    FOR i IN 1..COALESCE(cardinality(terms), 0) LOOP
        t := terms[i];
        IF t IS NULL OR t = '' THEN CONTINUE; END IF;
        esc := replace(t, chr(39), chr(39)||chr(39));
        pos := position(chr(39)||esc||chr(39)||':' in repr);
        IF pos > 0 THEN
            rest := substr(repr, pos + length(esc) + 3);
            j := 1;
            WHILE j <= length(rest) AND substr(rest, j, 1) ~ '[0-9A-D,]' LOOP
                j := j + 1;
            END LOOP;
            poslist := substr(rest, 1, j - 1);
            IF poslist = '' THEN tf := 1;
            ELSE tf := length(poslist) - length(replace(poslist, ',', '')) + 1;
            END IF;
        ELSIF position(chr(39)||esc||chr(39) in repr) > 0 THEN
            tf := 1;
        ELSE
            tf := 0;
        END IF;
        IF tf = 0 THEN CONTINUE; END IF;
        df := COALESCE(dfs[i], 0);
        idf := ln(1.0 + (n - df + 0.5) / (df + 0.5));
        score := score + idf * tf * (k1 + 1) / (tf + k1 * (1 - b + b * dl / avgdl));
    END LOOP;
    RETURN score;
END $$;

CREATE OR REPLACE FUNCTION postvec.lexical_score(
    tsv tsvector, tsq tsquery, terms text[], dfs integer[], n bigint, avgdl float8)
RETURNS float8
LANGUAGE sql STABLE
SET search_path = pg_catalog, pg_temp AS $$
    SELECT CASE
        WHEN tsv IS NULL OR tsq IS NULL THEN 0::float8
        WHEN n IS NULL OR n = 0 THEN ts_rank_cd(tsv, tsq)::float8
        ELSE postvec.bm25_score(tsv, terms, dfs, n, avgdl)
    END
$$;

CREATE OR REPLACE FUNCTION postvec._lexical_touch(rid bigint)
RETURNS void
LANGUAGE sql
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp AS $$
    UPDATE postvec.lexical_stats SET dirty_at = now() WHERE registry_id = rid
$$;

CREATE OR REPLACE FUNCTION postvec._refresh_lexical_stats(rid bigint)
RETURNS void
LANGUAGE plpgsql
SECURITY DEFINER
SET search_path = pg_catalog, pg_temp AS $$
DECLARE
    r postvec.registry;
    tsv_sql text;
    n_docs bigint;
    avg_dl float8;
BEGIN
    PERFORM set_config('statement_timeout', '300000', true);
    SELECT * INTO r FROM postvec.registry WHERE id = rid AND state <> 'disabled';
    IF NOT FOUND THEN RETURN; END IF;
    PERFORM 1 FROM postvec.lexical_stats WHERE registry_id = rid FOR UPDATE SKIP LOCKED;
    IF NOT FOUND THEN RETURN; END IF;
    IF r.chunking = 'recursive' THEN
        tsv_sql := format(
            'SELECT to_tsvector(%L::regconfig, chunk_text) FROM %I.%I',
            r.fts_config::text, r.destination_schema, r.destination_table);
    ELSE
        tsv_sql := format(
            'SELECT to_tsvector(%L::regconfig, %I::text) FROM %I.%I',
            r.fts_config::text, r.source_column, r.table_schema, r.table_name);
    END IF;
    DELETE FROM postvec.lexical_df WHERE registry_id = rid;
    INSERT INTO postvec.lexical_df (registry_id, term, df)
    SELECT rid, word, ndoc FROM ts_stat(tsv_sql) WHERE ndoc > 0;
    EXECUTE format(
        'SELECT count(*) FILTER (WHERE tsv IS NOT NULL AND tsv != ''''::tsvector),
                coalesce(avg(length(tsv)) FILTER (WHERE tsv IS NOT NULL AND tsv != ''''::tsvector), 1)
           FROM (%s) d(tsv)', tsv_sql) INTO n_docs, avg_dl;
    UPDATE postvec.lexical_stats
       SET n = n_docs, avgdl = avg_dl, refreshed_at = now(), dirty_at = NULL
     WHERE registry_id = rid;
END $$;
REVOKE ALL ON FUNCTION postvec._refresh_lexical_stats(bigint) FROM PUBLIC;

CREATE OR REPLACE FUNCTION postvec.refresh_lexical_stats(relation text, column_name text)
RETURNS void
LANGUAGE plpgsql
SECURITY DEFINER AS $$
DECLARE
    rel regclass;
    r postvec.registry;
BEGIN
    rel := to_regclass(relation);
    IF rel IS NULL THEN
        RAISE EXCEPTION 'postvec: relation % does not exist', relation;
    END IF;
    IF NOT pg_has_role(session_user, (SELECT relowner FROM pg_class WHERE oid = rel), 'USAGE') THEN
        RAISE EXCEPTION 'postvec: must own the source table to refresh lexical stats';
    END IF;
    SELECT reg.* INTO r
      FROM postvec.registry reg
      JOIN pg_class c ON c.oid = rel
      JOIN pg_namespace ns ON ns.oid = c.relnamespace
     WHERE reg.table_schema = ns.nspname
       AND reg.table_name = c.relname
       AND reg.source_column = column_name
       AND reg.state <> 'disabled';
    IF NOT FOUND THEN
        RAISE EXCEPTION 'postvec: %.% is not enabled', relation, column_name;
    END IF;
    PERFORM postvec._refresh_lexical_stats(r.id);
END $$;
