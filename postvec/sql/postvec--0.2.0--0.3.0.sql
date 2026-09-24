-- postvec 0.2.0 -> 0.3.0. Applied by ALTER EXTENSION postvec UPDATE; see README.md.
-- Schema changes go below the lock. upgrade_test.sh compares the result with a fresh install.
SELECT pg_advisory_xact_lock(hashtext('postvec_schema'));

-- The shared SECURITY DEFINER trigger functions pinned search_path to
-- pg_catalog alone, which leaves pg_temp searched first for type names: a
-- temporary type could run code as their owner. pg_temp now goes last.
ALTER FUNCTION postvec.trg_chunk_ins() SET search_path = pg_catalog, pg_temp;
ALTER FUNCTION postvec.trg_chunk_upd() SET search_path = pg_catalog, pg_temp;
ALTER FUNCTION postvec.trg_chunk_pk() SET search_path = pg_catalog, pg_temp;
ALTER FUNCTION postvec.trg_chunk_del() SET search_path = pg_catalog, pg_temp;
ALTER FUNCTION postvec.trg_truncate() SET search_path = pg_catalog, pg_temp;

-- Primary keys are queued as text by the writing session and parsed by the
-- worker: both now use one format (core's KEY_SETTINGS), so a DMY or
-- non-UTC session no longer queues a key the worker reads as another row.
ALTER FUNCTION postvec.trg_chunk_ins() SET DateStyle = 'ISO, MDY' SET TimeZone = 'UTC' SET IntervalStyle = 'postgres';
ALTER FUNCTION postvec.trg_chunk_upd() SET DateStyle = 'ISO, MDY' SET TimeZone = 'UTC' SET IntervalStyle = 'postgres';
ALTER FUNCTION postvec.trg_chunk_pk() SET DateStyle = 'ISO, MDY' SET TimeZone = 'UTC' SET IntervalStyle = 'postgres';
ALTER FUNCTION postvec.trg_chunk_del() SET DateStyle = 'ISO, MDY' SET TimeZone = 'UTC' SET IntervalStyle = 'postgres';
DO $$
DECLARE f regprocedure;
BEGIN
    FOR f IN SELECT p.oid::regprocedure FROM pg_catalog.pg_proc p
              WHERE p.pronamespace = 'postvec'::regnamespace AND p.proname ~ '^trg_(ins|upd|pk)_[0-9]+$' LOOP
        EXECUTE format('ALTER FUNCTION %s SET DateStyle = %L SET TimeZone = %L SET IntervalStyle = %L',
                       f, 'ISO, MDY', 'UTC', 'postgres');
    END LOOP;
END $$;

-- State written before the pin: entries whose key includes a date, time or
-- interval (domains, arrays and ranges resolved to their base type).
-- Checkpoints are reset rather than converted, since the key order changed
-- with the format; a reset only rescans (backfills revisit rows without
-- vectors, migrations rows whose new column is still NULL). A queued or
-- dead-lettered key is rewritten only when it reads the same under every
-- DateStyle field order, time zone and IntervalStyle a writer could have
-- used. An ambiguous one (a DMY/MDY date) cannot be trusted: queued ones move
-- to jobs_dead as evidence, and the entry is queued again in full, which
-- re-embeds stale vectors and rebuilds chunk sets.
CREATE FUNCTION postvec._format_sensitive(t text) RETURNS boolean LANGUAGE sql AS $f$
    WITH RECURSIVE b(oid) AS (
        SELECT to_regtype(t)::oid
        UNION ALL
        SELECT coalesce(nullif(p.typbasetype, 0), nullif(p.typelem, 0),
                        (SELECT rngsubtype FROM pg_catalog.pg_range WHERE rngtypid = p.oid),
                        (SELECT rngsubtype FROM pg_catalog.pg_range WHERE rngmultitypid = p.oid))
          FROM b JOIN pg_catalog.pg_type p ON p.oid = b.oid)
    SELECT coalesce(bool_or(p.typcategory IN ('D', 'T')), false)
      FROM b JOIN pg_catalog.pg_type p ON p.oid = b.oid
$f$;
CREATE FUNCTION postvec._canonical_key(v anyelement) RETURNS text LANGUAGE sql
    SET DateStyle = 'ISO, MDY' SET TimeZone = 'UTC' SET IntervalStyle = 'postgres'
    AS 'SELECT v::text';
-- The key in the new format if every reading agrees, else NULL. The SET
-- clauses only restore the caller's settings on exit.
CREATE FUNCTION postvec._legacy_key(k text, t text, zone text) RETURNS text LANGUAGE plpgsql
    SET DateStyle = 'ISO, MDY' SET TimeZone = 'UTC' SET IntervalStyle = 'postgres' AS $f$
DECLARE o text; z text; i text; v text; seen text;
BEGIN
    FOREACH o IN ARRAY ARRAY['DMY', 'MDY', 'YMD'] LOOP
        FOREACH z IN ARRAY ARRAY[zone, 'UTC'] LOOP
            FOREACH i IN ARRAY ARRAY['postgres', 'sql_standard'] LOOP
                PERFORM set_config('DateStyle', 'ISO, ' || o, true), set_config('TimeZone', z, true),
                        set_config('IntervalStyle', i, true);
                CONTINUE WHEN NOT pg_input_is_valid(k, t);
                EXECUTE format('SELECT postvec._canonical_key($1::%s)', t) INTO v USING k;
                IF seen IS NOT NULL AND v <> seen THEN RETURN NULL; END IF;
                seen := v;
            END LOOP;
        END LOOP;
    END LOOP;
    RETURN seen;
END $f$;
DO $$
DECLARE r record; t text; zone text; ambiguous bigint; queued bigint;
BEGIN
    -- The old worker read keys with the session defaults.
    SET LOCAL DateStyle TO DEFAULT; SET LOCAL TimeZone TO DEFAULT; SET LOCAL IntervalStyle TO DEFAULT;
    zone := current_setting('TimeZone');
    FOR r IN SELECT * FROM postvec.registry
              WHERE EXISTS (SELECT FROM unnest(pk_types) x WHERE postvec._format_sensitive(x)) LOOP
        UPDATE postvec.registry SET backfill_watermark = NULL WHERE id = r.id;
        UPDATE postvec.migrations SET last_pk = NULL WHERE registry_id = r.id AND r.chunking = 'none';
        t := r.pk_types[1];
        IF cardinality(r.pk_columns) > 1 THEN
            t := format('postvec._rekey_%s', r.id);
            EXECUTE format('CREATE TYPE %s AS (%s)', t, (SELECT string_agg(format('%I %s', c, y), ', ' ORDER BY o)
                FROM unnest(r.pk_columns, r.pk_types) WITH ORDINALITY u(c, y, o)));
        END IF;
        WITH k AS (SELECT id, postvec._legacy_key(pk_value, t, zone) AS c FROM postvec.jobs WHERE registry_id = r.id),
        moved AS (DELETE FROM postvec.jobs j USING k WHERE j.id = k.id AND k.c IS DISTINCT FROM j.pk_value
                  RETURNING j.*, k.c),
        kept AS (INSERT INTO postvec.jobs (registry_id, pk_value, op, chunk_id, attempts, not_before, claimed_at, last_error, created_at)
                 SELECT registry_id, c, op, chunk_id, attempts, not_before, claimed_at, last_error, created_at
                   FROM moved WHERE c IS NOT NULL
                 ON CONFLICT (registry_id, op, pk_value, chunk_id) WHERE claimed_at IS NULL DO NOTHING),
        dead AS (INSERT INTO postvec.jobs_dead (job_id, registry_id, pk_value, op, chunk_id, attempts, not_before, claimed_at, last_error, created_at)
                 SELECT id, registry_id, pk_value, op, chunk_id, attempts, not_before, claimed_at,
                        'queued before postvec 0.3.0 in an ambiguous key format; the entry was queued again in full', created_at
                   FROM moved WHERE c IS NULL RETURNING 1)
        SELECT count(*) INTO ambiguous FROM dead;
        WITH k AS (SELECT dead_id, postvec._legacy_key(pk_value, t, zone) AS c FROM postvec.jobs_dead
                    WHERE registry_id = r.id AND coalesce(last_error, '') NOT LIKE 'queued before postvec 0.3.0%'),
        rekeyed AS (UPDATE postvec.jobs_dead d SET pk_value = k.c FROM k
                     WHERE d.dead_id = k.dead_id AND k.c <> d.pk_value RETURNING 1)
        SELECT ambiguous + count(*) FILTER (WHERE c IS NULL) INTO ambiguous FROM k;
        IF ambiguous > 0 AND r.trigger_mode <> 'none' THEN
            PERFORM set_config('DateStyle', 'ISO, MDY', true), set_config('TimeZone', 'UTC', true),
                    set_config('IntervalStyle', 'postgres', true);
            EXECUTE format('INSERT INTO postvec.jobs (registry_id, pk_value, op) SELECT $1, (%s)::text, $2 FROM %I.%I WHERE %I IS NOT NULL
                            ON CONFLICT (registry_id, op, pk_value, chunk_id) WHERE claimed_at IS NULL DO NOTHING',
                           CASE WHEN cardinality(r.pk_columns) > 1
                                THEN 'ROW(' || (SELECT string_agg(quote_ident(c), ',') FROM unnest(r.pk_columns) c) || ')'
                                ELSE quote_ident(r.pk_columns[1]) END,
                           r.table_schema, r.table_name, r.source_column)
                USING r.id, CASE WHEN r.chunking = 'recursive' THEN 'refresh' ELSE 'embed' END;
            GET DIAGNOSTICS queued = ROW_COUNT;
            SET LOCAL DateStyle TO DEFAULT; SET LOCAL TimeZone TO DEFAULT; SET LOCAL IntervalStyle TO DEFAULT;
            RAISE NOTICE 'postvec: %.%.% had % keys in an ambiguous legacy format (see jobs_dead); % rows queued again',
                r.table_schema, r.table_name, r.source_column, ambiguous, queued;
        END IF;
    END LOOP;
END $$;
DROP FUNCTION postvec._legacy_key(text, text, text), postvec._canonical_key(anyelement), postvec._format_sensitive(text);
-- After the functions, whose cached plans may still name them.
DO $$
DECLARE t regtype;
BEGIN
    FOR t IN SELECT oid::regtype FROM pg_catalog.pg_type
              WHERE typnamespace = 'postvec'::regnamespace AND typname ~ '^_rekey_[0-9]+$' LOOP
        EXECUTE format('DROP TYPE %s', t);
    END LOOP;
END $$;
