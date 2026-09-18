#!/usr/bin/env bash
# Extension upgrade test: the previous release -> this tree, on the
# pgrx-managed PostgreSQL that ci.sh uses.
#
#   1. Catalog parity. A database created at the previous version and taken
#      forward with ALTER EXTENSION postvec UPDATE is catalog-identical to a
#      fresh install of this version (members, relations, columns in order,
#      defaults, indexes, constraints, policies, triggers, views, functions,
#      types, sequences, ACLs, comments, dumpable config tables). A
#      hand-written script that misses a change fails here.
#   2. Data. A populated previous-version database (plain entry, chunked
#      entry, queued jobs, a dead-lettered job, user rows) keeps every row
#      through the upgrade. The comparison runs inside the ALTER EXTENSION
#      transaction. Generated triggers still enqueue afterwards.
#   3. Version gate, in the order a package upgrade produces it: new library
#      loaded, schema still old. The worker logs that it is parked, claims
#      nothing, writes nothing (heartbeat included), then after ALTER
#      EXTENSION resumes and drains a pending chunk refresh against the
#      upgraded schema. Chunk refresh needs no inference engine.
#
#   upgrade_test.sh             # previous = newest older postvec-v* tag
#   upgrade_test.sh <git-ref>   # previous = any commit
#
# With no argument and no older release tag it skips (first release). It
# finishes with this tree installed; the previous release's install script
# stays in share/extension.
set -euo pipefail
cd "$(dirname "$0")"
REPO="$(git rev-parse --show-toplevel)"

PG="${POSTVEC_PG:-pg18}"
PGVER="${PG#pg}"
BIN="$(echo "$HOME"/.pgrx/"${PGVER}".*/pgrx-install/bin)"
[ -x "$BIN/pg_ctl" ] || { echo "no pgrx-managed PostgreSQL $PGVER under ~/.pgrx (cargo pgrx init --$PG)" >&2; exit 1; }
SHARE="$(dirname "$BIN")/share/postgresql/extension"
[ -f "$SHARE/vector.control" ] || { echo "pgvector is not installed into the pgrx PostgreSQL $PGVER" >&2; exit 1; }

version_of() { sed -nE 's/^version = "([^"]+)"$/\1/p' | head -1; }
NEW_VERSION="$(version_of < Cargo.toml)"

# ------------------------------------------------------------ the previous ref
PREV_REF="${1:-}"
if [ -z "$PREV_REF" ]; then
    # Newest tag of an older product version. A 0.1.1 hotfix in a repo that
    # already has 0.2.0 tagged upgrades from 0.1.0.
    PREV_REF="$("$REPO/packaging/postvec/scripts/previous-release.sh")"
    if [ -z "$PREV_REF" ]; then
        echo "skip  upgrade test: no earlier postvec-v* release tag (first release)."
        echo "      Before the first tag exists, name the previous commit: upgrade_test.sh <ref>"
        exit 0
    fi
fi
git rev-parse --verify --quiet "${PREV_REF}^{commit}" >/dev/null \
    || { echo "unknown ref '$PREV_REF' (git fetch --tags?)" >&2; exit 1; }
PREV_VERSION="$(git show "${PREV_REF}:postvec/Cargo.toml" | version_of)"
if [ "$PREV_VERSION" = "$NEW_VERSION" ]; then
    echo "skip  upgrade test: $PREV_REF is also $NEW_VERSION (a packaging-only release has no upgrade)."
    exit 0
fi

# cargo-pgrx must match each tree's pgrx dependency. One binary can build both
# only while the pin is unchanged; fail here if it has moved.
pin() { sed -nE "s/^PGRX_VERSION=//p"; }
NEW_PGRX="$(pin < "$REPO/packaging/postvec/versions.env")"
PREV_PGRX="$(git show "${PREV_REF}:packaging/postvec/versions.env" | pin)"
if [ "$NEW_PGRX" != "$PREV_PGRX" ]; then
    echo "the previous release pins cargo-pgrx $PREV_PGRX and this tree pins $NEW_PGRX." >&2
    echo "Build the previous side with a matching cargo-pgrx (PREV_CARGO_PGRX=/path/to/cargo-pgrx)." >&2
    [ -n "${PREV_CARGO_PGRX:-}" ] || exit 1
fi

ls sql/postvec--"${PREV_VERSION}"--*.sql >/dev/null 2>&1 \
    || { echo "no upgrade script starts at $PREV_VERSION: add sql/postvec--${PREV_VERSION}--${NEW_VERSION}.sql" >&2; exit 1; }

echo "==> upgrade test: postvec $PREV_VERSION ($PREV_REF) -> $NEW_VERSION ($PG)"

WORK="$(mktemp -d)"
PORT=28921
export PGHOST="$WORK" PGPORT="$PORT"
cleanup() {
    "$BIN/pg_ctl" -D "$WORK/data" -m immediate stop >/dev/null 2>&1 || true
    git -C "$REPO" worktree remove --force "$WORK/prev" >/dev/null 2>&1 || true
    rm -rf "$WORK"
}
trap cleanup EXIT
fail() {
    echo "FAIL  $*" >&2
    if [ -f "$WORK/pg.log" ]; then echo "--- server log (tail)" >&2; tail -40 "$WORK/pg.log" >&2; fi
    exit 1
}

# Each tree gets its own target directory. Sharing one looks like a cheap way
# to reuse compiled dependencies, but both trees emit target/debug/libpostvec.so
# and cargo does not re-copy that file when it considers a build up to date —
# so the second install can ship the *other* version's library. The previous
# side's directory lives under the extension's target/, so it is gitignored and
# cached from run to run. `loaded_version` below proves which library is live.
install_tree() {  # <dir> <label> <target-dir> [cargo-pgrx binary]
    local dir="$1" label="$2" target="$3" pgrx="cargo pgrx"
    # A cargo-pgrx binary run directly takes the subcommand name first.
    [ -n "${4:-}" ] && pgrx="$4 pgrx"
    echo "    building and installing $label"
    # cargo re-links a library whose output file is missing, but leaves an
    # existing one alone when it thinks the build is up to date — even if that
    # file came from another tree. A cdylib's name carries no hash, so the copy
    # in deps/ is as exposed as the one cargo lifts out of it; both go.
    # Deleting them makes the installed library always the one this tree
    # compiles, whatever an earlier run left behind.
    rm -f "$target"/{debug,release}/libpostvec.so "$target"/{debug,release}/deps/libpostvec.so
    (cd "$dir" && CARGO_TARGET_DIR="$target" \
        $pgrx install --pg-config "$BIN/pg_config" --no-default-features --features "$PG") \
        >"$WORK/build-$label.log" 2>&1 \
        || { tail -30 "$WORK/build-$label.log" >&2; fail "cargo pgrx install ($label)"; }
}
# The version compiled into the library the server actually loaded.
loaded_version() { "${PSQL[@]}" -d "$1" -Atc "SELECT postvec.build_info() ->> 'version'"; }

git -C "$REPO" worktree add --detach --quiet "$WORK/prev" "$PREV_REF"
install_tree "$WORK/prev/postvec" "previous" "$REPO/postvec/target/upgrade-previous" "${PREV_CARGO_PGRX:-}"
[ -f "$SHARE/postvec--${PREV_VERSION}.sql" ] || fail "previous install script postvec--${PREV_VERSION}.sql was not installed"

PSQL=("$BIN/psql" -v ON_ERROR_STOP=1 -X -q)
start() { "$BIN/pg_ctl" -D "$WORK/data" -l "$WORK/pg.log" -w \
              -o "-p $PORT -c unix_socket_directories='$WORK' -c listen_addresses='' $*" start >/dev/null; }
stop()  { "$BIN/pg_ctl" -D "$WORK/data" -m fast -w stop >/dev/null; }

"$BIN/initdb" -D "$WORK/data" -A trust >/dev/null
start

# --------------------------------------------- previous version: two databases
# upg_empty: the extension alone, for catalog parity.
# upg:       a populated database, for data survival and the worker.
"$BIN/createdb" upg_empty
"${PSQL[@]}" -d upg_empty -c "CREATE EXTENSION vector" -c "CREATE EXTENSION postvec VERSION '${PREV_VERSION}'"
[ "$(loaded_version upg_empty)" = "$PREV_VERSION" ] \
    || fail "the previous side loaded postvec $(loaded_version upg_empty), not $PREV_VERSION: a stale library was installed"

"$BIN/createdb" upg
"${PSQL[@]}" -d upg -v prev="$PREV_VERSION" >/dev/null <<'SQL'
CREATE EXTENSION vector;
CREATE EXTENSION postvec VERSION :'prev';

-- The model cache is not dumped and not upgraded; discovery refreshes it in
-- production. Seed what the entries below resolve against.
INSERT INTO postvec.models (name, model_type, target_model, target_dim, raw)
VALUES ('m', 'embed', 'm', 3, '{}'::jsonb);

-- A plain entry: its trigger enqueues one embed job per row.
CREATE TABLE notes (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, body text);
SELECT postvec.enable('public.notes', 'body', 'm');
INSERT INTO notes (body) VALUES ('first note'), ('second note'), ('third note');

-- A chunked entry: doc 1 already materialized (its refresh is done), doc 2
-- left with a pending refresh, which the worker drains after the upgrade.
CREATE TABLE docs (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, body text);
SELECT postvec.enable('public.docs', 'body', 'm',
                      chunking => 'recursive', destination => 'docs_chunks',
                      chunk_size => 64, chunk_overlap => 0);
INSERT INTO docs (body) VALUES ('ready document body'), ('pending document body');
DELETE FROM postvec.jobs WHERE op = 'refresh' AND pk_value = '1';
INSERT INTO docs_chunks (postvec_source_pk, postvec_chunk_seq, postvec_char_start,
                         postvec_char_end, chunk_text, body_semantic)
VALUES (1, 0, 0, 5, 'ready', '[1,2,3]'), (1, 1, 5, 13, 'document', NULL);

-- A dead-lettered job.
INSERT INTO postvec.jobs_dead (job_id, registry_id, pk_value, op, last_error)
SELECT 0, r.id, '2', 'embed', 'seeded dead job'
  FROM postvec.registry r WHERE r.table_name = 'notes';
SQL

# What has to stay identical across the upgrade: every row of the user tables,
# the identities of queued and dead jobs, and the registry.
DIGEST_SQL="SELECT concat_ws(' ',
  (SELECT count(*) FROM postvec.registry),
  (SELECT md5(coalesce(string_agg(id::text, ',' ORDER BY id), '')) FROM postvec.registry),
  (SELECT count(*) FROM postvec.jobs),
  (SELECT md5(coalesce(string_agg(concat_ws(':', id, registry_id, pk_value, op), ',' ORDER BY id), '')) FROM postvec.jobs),
  (SELECT count(*) FROM postvec.jobs_dead),
  (SELECT md5(coalesce(string_agg(concat_ws(':', registry_id, pk_value, op), ',' ORDER BY registry_id, pk_value), '')) FROM postvec.jobs_dead),
  (SELECT md5(coalesce(string_agg(t::text, ',' ORDER BY id), '')) FROM notes t),
  (SELECT md5(coalesce(string_agg(t::text, ',' ORDER BY id), '')) FROM docs t),
  (SELECT md5(coalesce(string_agg(t::text, ',' ORDER BY postvec_chunk_id), '')) FROM docs_chunks t))"
digest() { "${PSQL[@]}" -d upg -Atc "$DIGEST_SQL"; }
BEFORE="$(digest)"
PENDING_REFRESH="$("${PSQL[@]}" -d upg -Atc "SELECT count(*) FROM postvec.jobs WHERE op = 'refresh' AND pk_value = '2'")"
[ "$PENDING_REFRESH" = 1 ] || fail "fixture: expected one pending refresh, found $PENDING_REFRESH"
stop

# ---------------------------------------------- the package-upgrade sequence
# Files first (what apt/dnf does), restart with the worker, schema last.
install_tree "$REPO/postvec" "new" "$REPO/postvec/target"
[ -f "$SHARE/postvec--${NEW_VERSION}.sql" ] || fail "install script postvec--${NEW_VERSION}.sql was not installed"
for s in sql/postvec--*--*.sql; do
    [ -f "$SHARE/$(basename "$s")" ] || fail "upgrade script $(basename "$s") was not installed into $SHARE"
done

start "-c shared_preload_libraries=postvec -c postvec.database=upg -c postvec.mode=grpc"
[ "$(loaded_version upg)" = "$NEW_VERSION" ] \
    || fail "after the upgrade the server loaded postvec $(loaded_version upg), not $NEW_VERSION: a stale library was installed"
echo "    ok  libraries: $PREV_VERSION loaded before, $NEW_VERSION after"

# 3a. New library, old schema: the worker parks and touches nothing.
PARKED="worker parked for db=upg: the installed extension is version ${PREV_VERSION} but this postvec.so is ${NEW_VERSION}"
for _ in $(seq 1 150); do grep -qF "$PARKED" "$WORK/pg.log" && break; sleep 0.2; done
grep -qF "$PARKED" "$WORK/pg.log" || fail "the worker did not report the version skew within 30s"
sleep 2   # give a misbehaving worker the chance to touch something
[ "$(digest)" = "$BEFORE" ] || fail "a parked worker changed postvec data before ALTER EXTENSION"
HB="$("${PSQL[@]}" -d upg -Atc "SELECT count(*) FROM postvec.worker_heartbeat")"
[ "$HB" = 0 ] || fail "a parked worker wrote its heartbeat ($HB rows)"
echo "    ok  parked across the version skew: no claims, no writes, no heartbeat"

# 2. The upgrade itself. The data check runs inside the ALTER EXTENSION
# transaction: the script holds the schema lock exclusively, so the worker
# cannot act on the new schema before these rows have been compared.
DURING="$("${PSQL[@]}" -d upg -At <<SQL | tail -1
BEGIN;
ALTER EXTENSION postvec UPDATE;
$DIGEST_SQL;
COMMIT;
SQL
)"
[ "$DURING" = "$BEFORE" ] || fail "rows changed during the upgrade
  before: $BEFORE
  after:  $DURING"
"${PSQL[@]}" -d upg_empty -c "ALTER EXTENSION postvec UPDATE"
for db in upg upg_empty; do
    v="$("${PSQL[@]}" -d "$db" -Atc "SELECT extversion FROM pg_extension WHERE extname = 'postvec'")"
    [ "$v" = "$NEW_VERSION" ] || fail "$db is at $v after ALTER EXTENSION, expected $NEW_VERSION"
done
echo "    ok  ALTER EXTENSION postvec UPDATE: $PREV_VERSION -> $NEW_VERSION, every row intact"

# 3b. Versions agree: the worker resumes, heartbeats, and drains the backlog.
for _ in $(seq 1 150); do
    [ "$("${PSQL[@]}" -d upg -Atc "SELECT count(*) FROM postvec.worker_heartbeat")" != 0 ] \
        && [ "$("${PSQL[@]}" -d upg -Atc "SELECT count(*) FROM postvec.jobs WHERE op = 'refresh' AND pk_value = '2'")" = 0 ] \
        && break
    sleep 0.2
done
[ "$("${PSQL[@]}" -d upg -Atc "SELECT count(*) FROM postvec.worker_heartbeat")" != 0 ] \
    || fail "the worker did not resume (no heartbeat) within 30s of ALTER EXTENSION"
[ "$("${PSQL[@]}" -d upg -Atc "SELECT count(*) FROM postvec.jobs WHERE op = 'refresh' AND pk_value = '2'")" = 0 ] \
    || fail "the resumed worker did not drain the pending chunk refresh"
[ "$("${PSQL[@]}" -d upg -Atc "SELECT count(*) FROM docs_chunks WHERE postvec_source_pk = 2")" -gt 0 ] \
    || fail "the drained refresh produced no chunks for doc 2"
echo "    ok  worker resumed on the upgraded schema and drained a pending refresh"

# The generated triggers still enqueue after the upgrade.
JOBS="$("${PSQL[@]}" -d upg -Atc "SELECT count(*) FROM postvec.jobs WHERE pk_value = '4'")"
"${PSQL[@]}" -d upg -c "INSERT INTO notes (body) VALUES ('written after the upgrade')"
[ "$("${PSQL[@]}" -d upg -Atc "SELECT count(*) FROM postvec.jobs WHERE pk_value = '4'")" -gt "$JOBS" ] \
    || fail "an insert after the upgrade enqueued no job: the generated trigger did not survive"
echo "    ok  generated triggers still enqueue"

# 1. Catalog parity: fresh install vs upgraded install.
"$BIN/createdb" fresh
"${PSQL[@]}" -d fresh -c "CREATE EXTENSION vector" -c "CREATE EXTENSION postvec"
catalog() { "${PSQL[@]}" -d "$1" -At -f - <<'SQL'
WITH ext AS (SELECT oid FROM pg_extension WHERE extname = 'postvec'),
     ns  AS (SELECT oid FROM pg_namespace WHERE nspname = 'postvec'),
     rel AS (SELECT c.oid, c.relname FROM pg_class c WHERE c.relnamespace = (SELECT oid FROM ns))
SELECT line FROM (
  SELECT 'member ' || pg_describe_object(classid, objid, objsubid)
    FROM pg_depend WHERE refclassid = 'pg_extension'::regclass
     AND refobjid = (SELECT oid FROM ext) AND deptype = 'e'
  UNION ALL
  SELECT format('relation %s kind=%s persistence=%s rls=%s/%s', c.relname, c.relkind,
                c.relpersistence, c.relrowsecurity, c.relforcerowsecurity)
    FROM pg_class c WHERE c.relnamespace = (SELECT oid FROM ns)
     AND c.relkind IN ('r','p','v','m','S','f','c')
  UNION ALL
  -- Columns by logical position, so a dropped column's gap in an upgraded
  -- table does not register while a changed order still does: code that
  -- reads rows positionally depends on it.
  SELECT format('column %s #%s %s %s notnull=%s default=%s identity=%s generated=%s collation=%s',
                r.relname, row_number() OVER (PARTITION BY r.oid ORDER BY a.attnum), a.attname,
                format_type(a.atttypid, a.atttypmod), a.attnotnull,
                pg_get_expr(d.adbin, d.adrelid), a.attidentity, a.attgenerated,
                (SELECT collname FROM pg_collation WHERE oid = a.attcollation))
    FROM rel r JOIN pg_attribute a ON a.attrelid = r.oid AND a.attnum > 0 AND NOT a.attisdropped
    LEFT JOIN pg_attrdef d ON d.adrelid = r.oid AND d.adnum = a.attnum
  UNION ALL
  SELECT 'index ' || pg_get_indexdef(i.indexrelid)
    FROM pg_index i JOIN rel r ON r.oid = i.indrelid
  UNION ALL
  SELECT format('constraint %s on %s: %s', con.conname, r.relname, pg_get_constraintdef(con.oid))
    FROM pg_constraint con JOIN rel r ON r.oid = con.conrelid
  UNION ALL
  SELECT format('policy %s on %s cmd=%s permissive=%s roles=%s using=%s check=%s', p.polname,
                r.relname, p.polcmd, p.polpermissive, p.polroles::text,
                pg_get_expr(p.polqual, p.polrelid), pg_get_expr(p.polwithcheck, p.polrelid))
    FROM pg_policy p JOIN rel r ON r.oid = p.polrelid
  UNION ALL
  SELECT 'trigger ' || pg_get_triggerdef(t.oid)
    FROM pg_trigger t JOIN rel r ON r.oid = t.tgrelid WHERE NOT t.tgisinternal
  UNION ALL
  SELECT format('view %s: %s', r.relname, pg_get_viewdef(r.oid))
    FROM rel r JOIN pg_class c ON c.oid = r.oid WHERE c.relkind IN ('v','m')
  UNION ALL
  SELECT format('sequence %s type=%s start=%s inc=%s min=%s max=%s cycle=%s', r.relname,
                format_type(s.seqtypid, NULL), s.seqstart, s.seqincrement, s.seqmin, s.seqmax, s.seqcycle)
    FROM pg_sequence s JOIN rel r ON r.oid = s.seqrelid
  UNION ALL
  SELECT format('function %s(%s) returns %s lang=%s kind=%s volatile=%s strict=%s secdef=%s parallel=%s leakproof=%s config=%s src=%s',
                p.proname, pg_get_function_identity_arguments(p.oid), pg_get_function_result(p.oid),
                l.lanname, p.prokind, p.provolatile, p.proisstrict, p.prosecdef, p.proparallel,
                p.proleakproof, p.proconfig::text, md5(p.prosrc))
    FROM pg_proc p JOIN pg_language l ON l.oid = p.prolang
   WHERE p.pronamespace = (SELECT oid FROM ns)
  UNION ALL
  SELECT format('type %s kind=%s labels=%s', t.typname, t.typtype,
                (SELECT string_agg(enumlabel, ',' ORDER BY enumsortorder) FROM pg_enum WHERE enumtypid = t.oid))
    FROM pg_type t WHERE t.typnamespace = (SELECT oid FROM ns) AND t.typtype IN ('e','d','b','r')
  UNION ALL
  SELECT format('acl %s %s', c.relname, c.relacl::text)
    FROM pg_class c WHERE c.relnamespace = (SELECT oid FROM ns) AND c.relacl IS NOT NULL
  UNION ALL
  SELECT format('acl %s(%s) %s', p.proname, pg_get_function_identity_arguments(p.oid), p.proacl::text)
    FROM pg_proc p WHERE p.pronamespace = (SELECT oid FROM ns) AND p.proacl IS NOT NULL
  UNION ALL
  SELECT format('comment %s: %s', pg_describe_object(d.classoid, d.objoid, d.objsubid), md5(d.description))
    FROM pg_description d
   WHERE (d.classoid = 'pg_class'::regclass AND d.objoid IN (SELECT oid FROM rel))
      OR (d.classoid = 'pg_proc'::regclass
          AND d.objoid IN (SELECT oid FROM pg_proc WHERE pronamespace = (SELECT oid FROM ns)))
  UNION ALL
  SELECT format('config %s condition=%s', (x.reloid)::regclass, e.extcondition[x.n])
    FROM pg_extension e, unnest(e.extconfig) WITH ORDINALITY AS x(reloid, n)
   WHERE e.extname = 'postvec'
  UNION ALL
  SELECT format('event trigger %s on %s tags=%s function=%s enabled=%s', ev.evtname, ev.evtevent,
                ev.evttags::text, ev.evtfoid::regproc, ev.evtenabled)
    FROM pg_event_trigger ev
    JOIN pg_depend dep ON dep.classid = 'pg_event_trigger'::regclass AND dep.objid = ev.oid
     AND dep.refobjid = (SELECT oid FROM ext) AND dep.deptype = 'e'
) lines(line)
ORDER BY line COLLATE "C";
SQL
}
catalog fresh     > "$WORK/fresh.catalog"
catalog upg_empty > "$WORK/upgraded.catalog"
[ -s "$WORK/fresh.catalog" ] || fail "empty catalog fingerprint for the fresh install"
if ! diff -u --label "fresh $NEW_VERSION install" --label "$PREV_VERSION upgraded to $NEW_VERSION" \
        "$WORK/fresh.catalog" "$WORK/upgraded.catalog" > "$WORK/catalog.diff"; then
    cat "$WORK/catalog.diff" >&2
    fail "the upgraded catalog differs from a fresh install: sql/postvec--${PREV_VERSION}--${NEW_VERSION}.sql is incomplete (lines marked + exist only after the upgrade, - only in a fresh install)"
fi
echo "    ok  catalog parity: $(wc -l < "$WORK/fresh.catalog") catalog facts identical, fresh vs upgraded"

stop
echo "==> upgrade test passed: $PREV_VERSION -> $NEW_VERSION"
