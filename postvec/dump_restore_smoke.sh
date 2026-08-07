#!/usr/bin/env bash
# Dump/restore smoke test for recursive (chunked) entries.
#
# Proves that a database with a chunked entry survives pg_dump/pg_restore
# with ready, pending, and dead chunk work intact:
#
#   1. the destination table, its identity sequence POSITION, the join view,
#      the ownership-token comments, and the RLS policy restore;
#   2. the generated trigger attachments survive and resolve to the shared
#      extension-installed trg_chunk_* functions;
#   3. registry/jobs/jobs_dead rows (a ready document, a pending refresh, a
#      pending chunk-keyed child, a dead child) restore through the extension
#      config-dump registration;
#   4. post-restore: search, source DML invalidation, retry_dead, migrate/
#      abort, and the proven destination teardown all work.
#
# Self-contained: spins a scratch cluster from the pgrx-managed install (the
# extension must already be installed there — `cargo pgrx test`/`install` does
# that; ci.sh runs before this in CI). No background worker is needed: the
# seeded state stands in for completed refresh/embed work.
set -euo pipefail
cd "$(dirname "$0")"

PG="${POSTVEC_PG:-pg18}"
PGVER="${PG#pg}"
BIN="$(echo "$HOME"/.pgrx/"${PGVER}".*/pgrx-install/bin)"
if [ ! -x "$BIN/pg_ctl" ]; then
    echo "no pgrx-managed PostgreSQL $PGVER found under ~/.pgrx" >&2
    exit 1
fi
if [ ! -f "$(dirname "$BIN")/share/postgresql/extension/postvec.control" ]; then
    echo "postvec is not installed into the pgrx cluster; run ci.sh or cargo pgrx install first" >&2
    exit 1
fi

WORK="$(mktemp -d)"
PORT=28911
export PGHOST="$WORK" PGPORT="$PORT"
cleanup() {
    "$BIN/pg_ctl" -D "$WORK/data" -m immediate stop >/dev/null 2>&1 || true
    rm -rf "$WORK"
}
trap cleanup EXIT

"$BIN/initdb" -D "$WORK/data" -A trust >/dev/null
"$BIN/pg_ctl" -D "$WORK/data" -l "$WORK/pg.log" \
    -o "-p $PORT -c unix_socket_directories='$WORK' -c listen_addresses=''" start >/dev/null

PSQL=("$BIN/psql" -v ON_ERROR_STOP=1 -q)

"$BIN/createdb" p5src
"${PSQL[@]}" -d p5src <<'SQL'
CREATE EXTENSION vector;
CREATE EXTENSION postvec;

-- Seed the model cache (it is deliberately NOT dumped; discovery refreshes
-- it in production).
INSERT INTO postvec.models (name, model_type, target_model, target_dim, raw)
VALUES ('m','embed','m',3,'{}'::jsonb),
       ('m2','embed','m2',4,'{}'::jsonb),
       ('conv-m-m2','convert',NULL,NULL,'{}'::jsonb);
UPDATE postvec.models SET source_model='m', target_model='m2', source_dim=3, target_dim=4
 WHERE name='conv-m-m2';

CREATE TABLE docs (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY,
                   title text, body text);
SELECT postvec.enable('public.docs','body','m',
                      chunking => 'recursive', destination => 'docs_chunks',
                      chunk_size => 64, chunk_overlap => 0);

-- Doc 1: "ready" (materialized + embedded chunks, one still-pending child,
-- one dead child). Doc 2: pending refresh straight from the trigger.
INSERT INTO docs (title, body) VALUES ('t1', 'ready document body'),
                                      ('t2', 'pending document body');
DELETE FROM postvec.jobs WHERE pk_value = '1';   -- doc 1's refresh: "done"
INSERT INTO docs_chunks (postvec_source_pk, postvec_chunk_seq, postvec_char_start,
                         postvec_char_end, chunk_text, body_semantic)
VALUES (1, 0, 0, 5,  'ready',      '[1,2,3]'),
       (1, 1, 5, 13, 'document',   '[4,5,6]'),
       (1, 2, 13, 19, 'body',      NULL);
INSERT INTO postvec.jobs (registry_id, pk_value, op, chunk_id)
SELECT r.id, '1', 'embed', c.postvec_chunk_id
  FROM postvec.registry r, docs_chunks c
 WHERE c.postvec_chunk_seq = 2;
INSERT INTO postvec.jobs_dead (job_id, registry_id, pk_value, op, chunk_id, last_error)
SELECT 0, r.id, '1', 'embed', c.postvec_chunk_id, 'seeded dead child'
  FROM postvec.registry r, docs_chunks c
 WHERE c.postvec_chunk_seq = 1;
SQL

"$BIN/pg_dump" -d p5src -f "$WORK/dump.sql"
"$BIN/createdb" p5dst
"${PSQL[@]}" -d p5dst -f "$WORK/dump.sql" >/dev/null

"${PSQL[@]}" -d p5dst <<'SQL'
DO $check$
DECLARE
    r record;
    n bigint;
    tok text;
BEGIN
    -- 1. Registry shape + token restored.
    SELECT * INTO STRICT r FROM postvec.registry;
    IF r.chunking <> 'recursive' OR r.destination_table <> 'docs_chunks'
       OR r.destination_token IS NULL THEN
        RAISE EXCEPTION 'registry row did not restore: %', r;
    END IF;
    tok := r.destination_token;

    -- 2. Chunk rows with their exact identities.
    SELECT count(*) INTO n FROM docs_chunks;
    IF n <> 3 THEN RAISE EXCEPTION 'expected 3 chunks, got %', n; END IF;
    SELECT count(*) INTO n FROM docs_chunks WHERE body_semantic IS NOT NULL;
    IF n <> 2 THEN RAISE EXCEPTION 'expected 2 embedded chunks, got %', n; END IF;

    -- 3. Identity sequence position: a fresh insert must NOT reuse an id.
    IF (SELECT last_value FROM pg_sequences s, pg_class c
         WHERE s.schemaname = 'public' AND c.relname = 'docs_chunks'
           AND s.sequencename = pg_get_serial_sequence('docs_chunks','postvec_chunk_id')::regclass::name)
       IS DISTINCT FROM (SELECT max(postvec_chunk_id) FROM docs_chunks) THEN
        RAISE EXCEPTION 'identity sequence position was not restored';
    END IF;

    -- 4. Queue state: one pending refresh (doc 2), one pending chunk-keyed
    --    child (doc 1), one dead child.
    SELECT count(*) INTO n FROM postvec.jobs WHERE op = 'refresh' AND pk_value = '2';
    IF n <> 1 THEN RAISE EXCEPTION 'pending refresh did not restore'; END IF;
    SELECT count(*) INTO n FROM postvec.jobs WHERE op = 'embed' AND chunk_id IS NOT NULL;
    IF n <> 1 THEN RAISE EXCEPTION 'pending child job did not restore'; END IF;
    SELECT count(*) INTO n FROM postvec.jobs_dead WHERE chunk_id IS NOT NULL;
    IF n <> 1 THEN RAISE EXCEPTION 'dead child did not restore'; END IF;

    -- 5. The five trigger attachments survive and resolve to the SHARED
    --    extension-installed functions.
    SELECT count(*) INTO n FROM pg_trigger t JOIN pg_proc p ON p.oid = t.tgfoid
                        JOIN pg_namespace pn ON pn.oid = p.pronamespace
     WHERE t.tgrelid = 'docs'::regclass AND pn.nspname = 'postvec'
       AND p.proname IN ('trg_chunk_ins','trg_chunk_upd','trg_chunk_pk',
                         'trg_chunk_del','trg_truncate');
    IF n <> 5 THEN
        RAISE EXCEPTION 'expected 5 shared-function trigger attachments, got %', n;
    END IF;

    -- 6. View + markers + RLS.
    IF to_regclass('docs_chunks_view') IS NULL THEN
        RAISE EXCEPTION 'the join view did not restore';
    END IF;
    IF position(tok in coalesce(obj_description('docs_chunks'::regclass,'pg_class'),'')) = 0
       OR position(tok in coalesce(obj_description('docs_chunks_view'::regclass,'pg_class'),'')) = 0 THEN
        RAISE EXCEPTION 'ownership-token comments did not restore';
    END IF;
    IF NOT (SELECT relrowsecurity AND relforcerowsecurity FROM pg_class
             WHERE oid = 'docs_chunks'::regclass) THEN
        RAISE EXCEPTION 'RLS enable/force did not restore';
    END IF;
    SELECT count(*) INTO n FROM pg_policy WHERE polrelid = 'docs_chunks'::regclass;
    IF n <> 1 THEN RAISE EXCEPTION 'the RLS policy did not restore'; END IF;
END $check$;

-- 7. Post-restore behavior. Search first (chunks intact):
DO $s$
DECLARE n bigint;
BEGIN
    SELECT count(*) INTO n
      FROM postvec.search_with_vector('docs','body', ARRAY[1,2,3]::real[],
                                      query_text => 'ready');
    IF n < 1 THEN RAISE EXCEPTION 'post-restore search returned nothing'; END IF;
END $s$;

-- The model cache is NOT dumped; refresh it (here: reseed) as documented.
INSERT INTO postvec.models (name, model_type, source_model, target_model,
                            source_dim, target_dim, raw)
VALUES ('m','embed',NULL,'m',NULL,3,'{}'::jsonb),
       ('m2','embed',NULL,'m2',NULL,4,'{}'::jsonb),
       ('conv-m-m2','convert','m','m2',3,4,'{}'::jsonb);

-- retry_dead preserves the restored dead child's identity.
DO $rd$
DECLARE n bigint;
BEGIN
    n := postvec.retry_dead('docs'::regclass, 'body');
    IF n <> 1 THEN RAISE EXCEPTION 'retry_dead consumed % dead rows', n; END IF;
END $rd$;

-- Source DML invalidates through the restored triggers.
UPDATE docs SET body = 'edited after restore' WHERE id = 1;
DO $t$
DECLARE n bigint;
BEGIN
    SELECT count(*) INTO n FROM docs_chunks WHERE postvec_source_pk = 1;
    IF n <> 0 THEN RAISE EXCEPTION 'restored triggers did not purge chunks'; END IF;
    SELECT count(*) INTO n FROM postvec.jobs WHERE op = 'refresh' AND pk_value = '1';
    IF n <> 1 THEN RAISE EXCEPTION 'restored triggers did not enqueue a refresh'; END IF;
END $t$;

-- Migration lifecycle reaches the destination; abort cleans up.
DO $m$
DECLARE mid bigint; n bigint;
BEGIN
    mid := postvec.migrate('public.docs','body','m2');
    SELECT count(*) INTO n FROM pg_attribute
     WHERE attrelid = 'docs_chunks'::regclass
       AND attname = 'body_semantic_new' AND NOT attisdropped;
    IF n <> 1 THEN RAISE EXCEPTION 'migration scratch column not on the destination'; END IF;
    PERFORM postvec.migration_abort(mid);
END $m$;

-- Teardown: the restored comments are the ownership proof.
SELECT postvec.disable('public.docs','body', drop_destination => true);
DO $d$
BEGIN
    IF to_regclass('docs_chunks') IS NOT NULL THEN
        RAISE EXCEPTION 'proven teardown did not drop the restored destination';
    END IF;
END $d$;
SQL

echo "== postvec dump/restore smoke passed =="

if [[ "${POSTVEC_TEST_MANAGED:-0}" == 1 ]]; then
    POSTVEC_MANAGED_TEST_DSN="postgresql://$(id -un)@localhost/postgres?host=${WORK}&port=${PORT}" \
    POSTVEC_MANAGED_TEST_EXTENSION=1 \
        cargo test --manifest-path ../Cargo.toml -p postvec-server --test managed --test managed_worker -- --ignored
fi
