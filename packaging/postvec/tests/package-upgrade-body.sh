#!/usr/bin/env bash
# Runs inside a clean distribution container; driven by package-upgrade-test.sh.
#
#   /packages-old  the previous release's CLI + extension package
#   /packages-new  this release's CLI + extension package
#
# The sequence is the one a user follows: install the old release, use it,
# upgrade the packages, restart PostgreSQL, then ALTER EXTENSION.

set -Eeuo pipefail

: "${MAJOR:?}" "${OLD_VERSION:?}" "${NEW_VERSION:?}" "${DIST_FAMILY:?}"

passed=0 failed=0
ok()   { printf '  \033[32mok\033[0m    %s\n' "$*"; passed=$((passed + 1)); }
bad()  { printf '  \033[1;31mFAIL\033[0m  %s\n' "$*" >&2; failed=$((failed + 1)); }
die()  { bad "$*"; printf '\n%d passed, %d failed\n' "${passed}" "${failed}" >&2; exit 1; }
step() { printf '\n%s\n' "$*"; }

case "${DIST_FAMILY}" in
deb)
    PKG_INSTALL() { apt-get install -y -qq "$@" >/dev/null; }
    PG_BIN="/usr/lib/postgresql/${MAJOR}/bin"
    PG_EXT="/usr/share/postgresql/${MAJOR}/extension"
    PKG_EXT=deb ;;
rpm)
    PKG_INSTALL() { dnf install -y -q "$@" >/dev/null; }
    PG_BIN="/usr/pgsql-${MAJOR}/bin"
    PG_EXT="/usr/pgsql-${MAJOR}/share/extension"
    PKG_EXT=rpm ;;
*) echo "unknown DIST_FAMILY=${DIST_FAMILY}" >&2; exit 2 ;;
esac

step "prerequisites"
bash /postvec-prerequisites.sh --pg "${MAJOR}" --yes >/tmp/prereq.log 2>&1 \
    && ok "prerequisite bootstrap (PGDG${DIST_FAMILY/rpm/, EPEL, CRB})" \
    || die "postvec-prerequisites.sh failed: $(tail -5 /tmp/prereq.log)"

step "the previous release: postvec ${OLD_VERSION}"
PKG_INSTALL /packages-old/*."${PKG_EXT}" 2>/tmp/install-old.err \
    && ok "installed $(cd /packages-old && ls)" \
    || die "installing the previous release failed: $(tail -5 /tmp/install-old.err)"
[[ -f "${PG_EXT}/postvec--${OLD_VERSION}.sql" ]] \
    && ok "install script postvec--${OLD_VERSION}.sql in ${PG_EXT}" \
    || die "no postvec--${OLD_VERSION}.sql in ${PG_EXT}"

DATA=/var/lib/postvec-upgrade/data
SOCKET_DIR=/var/run/postgresql
mkdir -p "$(dirname "${DATA}")" "${SOCKET_DIR}"
chown -R postgres:postgres "$(dirname "${DATA}")" "${SOCKET_DIR}"
chmod 2775 "${SOCKET_DIR}"
as_pg() { su postgres -s /bin/bash -c "$*"; }
as_pg "${PG_BIN}/initdb -D ${DATA} -A trust --username=postgres" >/tmp/initdb.log 2>&1 \
    || die "initdb failed: $(tail -3 /tmp/initdb.log)"
echo "unix_socket_directories = '${SOCKET_DIR}'" >> "${DATA}/postgresql.conf"
# PGDG's el9 initdb turns the logging collector on, which sends the server log
# to ${DATA}/log instead of pg_ctl's -l file the checks below read.
echo "logging_collector = off" >> "${DATA}/postgresql.conf"
start() { as_pg "${PG_BIN}/pg_ctl -D ${DATA} -l /tmp/pg.log -w -t 60 -o \"$*\" start" >/tmp/pgctl.log 2>&1 \
              || die "the cluster did not start: $(tail -5 /tmp/pg.log)"; }
stop()  { as_pg "${PG_BIN}/pg_ctl -D ${DATA} -m fast -w -t 60 stop" >/dev/null 2>&1 || true; }
sql()   { as_pg "${PG_BIN}/psql -h ${SOCKET_DIR} -d postgres -tAXq -v ON_ERROR_STOP=1 -c \"$*\""; }

start ""
sql "CREATE EXTENSION postvec CASCADE" >/tmp/create.log 2>&1 \
    || die "CREATE EXTENSION postvec failed: $(cat /tmp/create.log)"
[[ "$(sql "SELECT extversion FROM pg_extension WHERE extname = 'postvec'")" == "${OLD_VERSION}" ]] \
    && ok "CREATE EXTENSION postvec at ${OLD_VERSION}" \
    || die "extension is not at ${OLD_VERSION}"

# Use it: a model, an enabled column, rows whose trigger queued work.
as_pg "${PG_BIN}/psql -h ${SOCKET_DIR} -d postgres -qAX -v ON_ERROR_STOP=1" >/tmp/seed.log 2>&1 <<'SQL' \
    || die "seeding the previous release failed: $(tail -5 /tmp/seed.log)"
INSERT INTO postvec.models (name, model_type, target_model, target_dim, raw)
VALUES ('m', 'embed', 'm', 3, '{}'::jsonb);
CREATE TABLE notes (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, body text);
SELECT postvec.enable('public.notes', 'body', 'm');
INSERT INTO notes (body) VALUES ('first note'), ('second note'), ('third note');
SQL
DIGEST_SQL="SELECT concat_ws(' ', (SELECT count(*) FROM postvec.registry), (SELECT count(*) FROM postvec.jobs), (SELECT md5(string_agg(t::text, ',' ORDER BY id)) FROM notes t))"
BEFORE="$(sql "${DIGEST_SQL}")"
ok "populated: registry, queued jobs, user rows (${BEFORE})"
stop

step "upgrade the packages to postvec ${NEW_VERSION}"
PKG_INSTALL /packages-new/*."${PKG_EXT}" 2>/tmp/install-new.err \
    && ok "upgraded to $(cd /packages-new && ls)" \
    || die "upgrading the packages failed: $(tail -5 /tmp/install-new.err)"
[[ -f "${PG_EXT}/postvec--${NEW_VERSION}.sql" ]] \
    && ok "install script postvec--${NEW_VERSION}.sql in ${PG_EXT}" \
    || die "no postvec--${NEW_VERSION}.sql in ${PG_EXT}"
# A path from this install: a direct OLD--NEW script, or a chain
# (0.1.0 -> 0.3.0 is 0.1.0--0.2.0.sql then 0.2.0--0.3.0.sql). ALTER
# EXTENSION walks whatever starts at OLD.
shopt -s nullglob
starters=("${PG_EXT}/postvec--${OLD_VERSION}--"*.sql)
shopt -u nullglob
(( ${#starters[@]} )) && [[ -f "${starters[0]}" ]] \
    && ok "upgrade path from ${OLD_VERSION} on disk ($(basename "${starters[0]}")${starters[1]:+, …})" \
    || die "no postvec--${OLD_VERSION}--*.sql in ${PG_EXT}: ALTER EXTENSION has no path from ${OLD_VERSION}"
grep -q "default_version = '${NEW_VERSION}'" "${PG_EXT}/postvec.control" \
    && ok "control file default_version = ${NEW_VERSION}" \
    || die "control file: $(grep default_version "${PG_EXT}/postvec.control")"

# The production order: library preloaded, background worker serving this
# database, schema still at the previous version.
start "-c shared_preload_libraries=postvec -c postvec.database=postgres -c postvec.mode=grpc"
[[ "$(sql "SELECT postvec.build_info() ->> 'version'")" == "${NEW_VERSION}" ]] \
    && ok "the new library (${NEW_VERSION}) loads against the ${OLD_VERSION} catalog" \
    || die "loaded library is $(sql "SELECT postvec.build_info() ->> 'version'"), not ${NEW_VERSION}"
PARKED="the installed extension is version ${OLD_VERSION} but this postvec.so is ${NEW_VERSION}"
for _ in $(seq 1 150); do grep -qF "${PARKED}" /tmp/pg.log && break; sleep 0.2; done
grep -qF "${PARKED}" /tmp/pg.log \
    && ok "the worker parks until the schema is upgraded" \
    || die "the worker did not report the version skew within 30s: $(tail -5 /tmp/pg.log)"
[[ "$(sql "${DIGEST_SQL}")" == "${BEFORE}" ]] \
    && ok "nothing changed while parked" || die "data changed before ALTER EXTENSION"

step "ALTER EXTENSION postvec UPDATE"
sql "ALTER EXTENSION postvec UPDATE" >/tmp/alter.log 2>&1 \
    || die "ALTER EXTENSION postvec UPDATE failed: $(cat /tmp/alter.log)"
[[ "$(sql "SELECT extversion FROM pg_extension WHERE extname = 'postvec'")" == "${NEW_VERSION}" ]] \
    && ok "extension is at ${NEW_VERSION}" || die "extension did not reach ${NEW_VERSION}"
[[ "$(sql "${DIGEST_SQL}")" == "${BEFORE}" ]] \
    && ok "every row intact" || die "rows changed across the upgrade"
for _ in $(seq 1 150); do [[ "$(sql "SELECT count(*) FROM postvec.worker_heartbeat")" != 0 ]] && break; sleep 0.2; done
[[ "$(sql "SELECT count(*) FROM postvec.worker_heartbeat")" != 0 ]] \
    && ok "the worker resumed (heartbeat)" || die "the worker did not resume within 30s of ALTER EXTENSION"
jobs_before="$(sql "SELECT count(*) FROM postvec.jobs")"
sql "INSERT INTO notes (body) VALUES ('written after the upgrade')"
[[ "$(sql "SELECT count(*) FROM postvec.jobs")" -gt "${jobs_before}" ]] \
    && ok "the generated trigger still enqueues" || die "an insert after the upgrade enqueued nothing"
version="$(postvec --version 2>&1 || true)"
[[ "${version}" == *"${NEW_VERSION}"* ]] \
    && ok "postvec --version reports ${NEW_VERSION}" || bad "postvec --version said: ${version}"
stop

printf '\n%d passed, %d failed\n' "${passed}" "${failed}"
(( failed == 0 ))
