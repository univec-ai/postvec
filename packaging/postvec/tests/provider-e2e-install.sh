#!/usr/bin/env bash
# The install phase of the PV-13 provider gate's PACKAGE cells, run *inside*
# a clean container of the target distribution by
# tests/provider-e2e-test.sh. Not useful on its own.
#
# It brings the container to the state an operator's full install reaches —
# documented prerequisites, every release package, an initdb'd cluster with
# postvec set up in embedded mode and a beating worker — and stops there.
# The provider scenarios themselves are driven from the host so that the one
# scenario suite serves the package cells and the image cells alike
# (package-install-body.sh proves the install contract in depth; this only
# has to reach a serving state).
#
# Inputs (environment): PG_MAJOR, POSTVEC_VERSION, BUNDLED_MODEL_NAME,
# DIST_FAMILY. Packages are at /packages.

set -Eeuo pipefail

case "${DIST_FAMILY}" in
deb)
    PKG_INSTALL() { apt-get install -y -qq "$@" >/dev/null; }
    BIN="/usr/lib/postgresql/${PG_MAJOR}/bin"
    TEST_TOOLS=(procps jq python3)
    ;;
rpm)
    PKG_INSTALL() { dnf install -y -q "$@" >/dev/null; }
    BIN="/usr/pgsql-${PG_MAJOR}/bin"
    TEST_TOOLS=(procps-ng findutils jq python3)
    ;;
*) echo "unknown DIST_FAMILY=${DIST_FAMILY}" >&2; exit 2 ;;
esac

echo "== provider-e2e install: prerequisites"
bash /postvec-prerequisites.sh --pg "${PG_MAJOR}" --yes >/dev/null

# Tools the *test harness* needs (process inspection, JSON, the mock's
# runtime), which a user does not — installed separately so they cannot
# quietly become part of what the bootstrap provides.
PKG_INSTALL "${TEST_TOOLS[@]}"

echo "== provider-e2e install: packages"
if [[ "${DIST_FAMILY}" == deb ]]; then
    apt-get install -y -qq /packages/*.deb >/dev/null
else
    dnf install -y -q /packages/*.rpm >/dev/null
fi

echo "== provider-e2e install: cluster"
DATA=/var/lib/postvec-test/data
CONF_D=/etc/postvec-test/conf.d
SOCKET_DIR=/var/run/postgresql
mkdir -p "$(dirname "${DATA}")" "${CONF_D}" "${SOCKET_DIR}"
chown -R postgres:postgres "$(dirname "${DATA}")" "${CONF_D}" "${SOCKET_DIR}"
# Docker exec inherits the daemon's umask. Snap-packaged Docker can use 000,
# which would make CONF_D world-writable and correctly trip postvec's safety
# check. Test fixtures must have deterministic modes regardless of the daemon.
chmod 0755 "$(dirname "${DATA}")" "$(dirname "${CONF_D}")" "${CONF_D}"
chmod 2775 "${SOCKET_DIR}"

as_pg() { su postgres -s /bin/bash -c "$*"; }

as_pg "${BIN}/initdb -D ${DATA} -A trust --username=postgres" >/tmp/initdb.log 2>&1
cat >> "${DATA}/postgresql.conf" <<EOF
include_dir = '${CONF_D}'
unix_socket_directories = '${SOCKET_DIR}'
EOF
chown postgres:postgres "${DATA}/postgresql.conf"

start_cluster() { as_pg "${BIN}/pg_ctl -D ${DATA} -l /tmp/pg.log -w -t 60 start" >/tmp/pgctl.log 2>&1; }
stop_cluster()  { as_pg "${BIN}/pg_ctl -D ${DATA} -m fast -w -t 60 stop" >/dev/null 2>&1 || true; }
psql_() { as_pg "${BIN}/psql -h ${SOCKET_DIR} -d postgres -tAX -v ON_ERROR_STOP=1 $*"; }

start_cluster
psql_ "-c \"CREATE EXTENSION postvec CASCADE\"" >/dev/null

echo "== provider-e2e install: postvec setup --embedded"
setup_status=0
postvec setup \
    --pg-config "${BIN}/pg_config" \
    --config-dir "${CONF_D}" \
    --database postgres \
    --embedded --path /opt/postvec/ninference \
    --model "${BUNDLED_MODEL_NAME}" \
    --no-restart --yes --allow-unreachable >/tmp/setup.log 2>&1 || setup_status=$?
if [[ "${setup_status}" != 0 && "${setup_status}" != 4 ]]; then
    echo "postvec setup failed (exit ${setup_status}):" >&2
    tail -20 /tmp/setup.log >&2
    exit 1
fi

stop_cluster
start_cluster

echo "== provider-e2e install: waiting for the worker heartbeat"
for _ in $(seq 1 60); do
    beat="$(psql_ "-c 'SELECT worker_last_beat IS NOT NULL FROM postvec.stats()'" 2>/dev/null || true)"
    [[ "${beat}" == t ]] && break
    sleep 1
done
if [[ "${beat:-}" != t ]]; then
    echo "no worker heartbeat within 60s:" >&2
    tail -20 /tmp/pg.log >&2
    exit 1
fi

echo "== provider-e2e install: READY (PG ${PG_MAJOR}, ${DIST_FAMILY})"
