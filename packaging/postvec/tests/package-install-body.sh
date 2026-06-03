#!/usr/bin/env bash
# The body of the package install test, run *inside* a clean container of the
# target distribution by tests/package-install-test.sh. Not useful on its own.
#
# It is a separate file rather than an embedded heredoc because it is long
# enough to deserve syntax checking, shellcheck, and a diff that reads.
#
# Inputs (environment): PG_MAJORS, POSTVEC_VERSION, BUNDLED_MODEL_NAME,
# BUNDLED_MODEL_BACKEND, BUNDLED_MODEL_TARGET_DIM, MODEL_PKG_NAME,
# SERVER_LICENSE, DIST_FAMILY, MINIMAL. Packages are at /packages.
#
# The four model facts are derived from the verified registry archive by
# scripts/build-model-bundle.sh and passed in by the caller, so this test
# follows a bundled-model change instead of silently testing the previous one.

set -Eeuo pipefail

passed=0; failed=0
ok()   { printf '  \033[32mok\033[0m    %s\n' "$*"; passed=$((passed + 1)); }
bad()  { printf '  \033[1;31mFAIL\033[0m  %s\n' "$*" >&2; failed=$((failed + 1)); }
step() { printf '\n%s\n' "$*"; }

FIRST_MAJOR="$(awk '{print $1}' <<<"${PG_MAJORS}")"

# ---------------------------------------------------------------- distro glue
#
# Everything below this block is family-independent. The differences are real
# but small: the package manager, where PGDG puts PostgreSQL, and how a cluster
# is created and started without an init system.

case "${DIST_FAMILY}" in
deb)
    PKG_INSTALL()  { apt-get install -y -qq "$@" >/dev/null; }
    PKG_REMOVE()   { apt-get remove -y -qq "$@" >/dev/null; }
    PKG_OWNER()    { dpkg -S "$1" 2>/dev/null | cut -d: -f1; }
    PKG_LIST()     { dpkg-query -W -f='${Package}\n' "$1" 2>/dev/null || true; }
    # What a package *declares* it installs, which is not the same as what a
    # minimal image kept: see the doc-policy note further down.
    PKG_FILES()    { dpkg -L "$1" 2>/dev/null || true; }
    # The licence a package declares, from its copyright file (Debian has no
    # control field for it; the DEP-5 rendering carries a `License:` line).
    PKG_LICENSE()  { sed -n 's/^License: //p' "/usr/share/doc/$1/copyright" 2>/dev/null | head -1; }
    EXTENSION_PACKAGES() { PKG_LIST 'postgresql-*-postvec' | grep -E '^postgresql-[0-9]+-postvec$' || true; }
    PGVECTOR_PACKAGE()   { echo "postgresql-$1-pgvector"; }
    PG_SERVER_PACKAGE()  { echo "postgresql-$1"; }
    PG_BIN()       { echo "/usr/lib/postgresql/$1/bin"; }
    PG_LIB()       { echo "/usr/lib/postgresql/$1/lib"; }
    PG_EXT()       { echo "/usr/share/postgresql/$1/extension"; }
    PG_USER=postgres
    ;;
rpm)
    PKG_INSTALL()  { dnf install -y -q "$@" >/dev/null; }
    PKG_REMOVE()   { dnf remove -y -q "$@" >/dev/null; }
    PKG_OWNER()    { rpm -qf "$1" --qf '%{NAME}\n' 2>/dev/null; }
    PKG_LIST()     { rpm -qa --qf '%{NAME}\n' 2>/dev/null | grep -E "$1" || true; }
    PKG_FILES()    { rpm -ql "$1" 2>/dev/null || true; }
    PKG_LICENSE()  { rpm -q --qf '%{LICENSE}' "$1" 2>/dev/null; }
    EXTENSION_PACKAGES() { PKG_LIST '^postgresql[0-9]+-postvec$'; }
    PGVECTOR_PACKAGE()   { echo "pgvector_$1"; }
    PG_SERVER_PACKAGE()  { echo "postgresql$1-server"; }
    PG_BIN()       { echo "/usr/pgsql-$1/bin"; }
    PG_LIB()       { echo "/usr/pgsql-$1/lib"; }
    PG_EXT()       { echo "/usr/pgsql-$1/share/extension"; }
    PG_USER=postgres
    ;;
*) echo "unknown DIST_FAMILY=${DIST_FAMILY}" >&2; exit 2 ;;
esac

step "prerequisites"

# The documented bootstrap, run from the *same script* the documentation tells
# a user to run — not a reimplementation of it beside the documentation.
#
# This matters more than it looks. postvec's packages name an exact PostgreSQL
# major and pgvector 0.8, which no supported distribution ships in its own
# archives, so a user's very first command fails on an untouched host unless
# PGDG (and, on EL9, EPEL and CRB) is configured first. When the test did that
# itself, the test passed and the documented path did not exist. Now the
# documented path *is* the tested path, and a bootstrap that stops working
# fails here.
if [[ ! -x /postvec-prerequisites.sh && ! -f /postvec-prerequisites.sh ]]; then
    echo "the prerequisite script was not mounted; see tests/package-install-test.sh" >&2
    exit 2
fi
if bash /postvec-prerequisites.sh --pg "${FIRST_MAJOR}" --yes; then
    ok "the published prerequisite script configured PGDG for PG ${FIRST_MAJOR}"
else
    bad "the published prerequisite script failed — a user's first command would fail too"
    printf '%d passed, %d failed\n' "${passed}" "${failed}"
    exit 1
fi

# Rerunning it must change nothing: an operator who is unsure whether it has
# been run should be able to just run it.
if bash /postvec-prerequisites.sh --pg "${FIRST_MAJOR}" --yes >/dev/null 2>&1; then
    ok "the prerequisite script is idempotent"
else
    bad "the prerequisite script fails when run a second time"
fi

# Tools this *test* needs, which a user does not: process inspection and a JSON
# reader for the doctor assertions below. Installed separately and after the
# bootstrap, so they cannot quietly become part of what the bootstrap provides.
if [[ "${DIST_FAMILY}" == deb ]]; then
    PKG_INSTALL procps jq
else
    PKG_INSTALL procps-ng findutils jq
fi

# PostgreSQL itself is deliberately *not* preinstalled. The extension package
# declares a dependency on it, and the whole point of installing on a clean
# host is to find out whether that dependency names the right thing. Installing
# PostgreSQL first would satisfy it in advance and the name would never be
# tested — which is exactly how a package ships depending on
# `postgresql-18-server` on a distribution that calls it `postgresql-18`.
# Precisely the server packages the extension depends on — a wildcard would
# also match `postgresql-common`, which is a different thing entirely.
preinstalled=""
for major in ${PG_MAJORS}; do
    server="$(PG_SERVER_PACKAGE "${major}")"
    PKG_LIST "${server}" | grep -q . && preinstalled="${preinstalled} ${server}"
done
if [[ -n "${preinstalled}" ]]; then
    bad "already installed:${preinstalled}; the dependency would not be tested"
else
    ok "no PostgreSQL server is installed before postvec is"
fi

step "install"

# The package manager's own resolver, not `dpkg -i` / `rpm -i`: dependency
# resolution from the archives the bootstrap above configured is part of what is
# under test, and it is how the documentation tells users to install.
if [[ "${DIST_FAMILY}" == deb ]]; then
    install_result=0
    apt-get install -y -qq /packages/*.deb >/dev/null 2>/tmp/install.err || install_result=$?
else
    install_result=0
    dnf install -y -q /packages/*.rpm >/dev/null 2>/tmp/install.err || install_result=$?
fi
if (( install_result == 0 )); then
    ok "the package manager resolved and installed every package"
else
    bad "install failed: $(tail -3 /tmp/install.err)"
    printf '%d passed, %d failed\n' "${passed}" "${failed}"
    exit 1
fi

for major in ${PG_MAJORS}; do
    server="$(PG_SERVER_PACKAGE "${major}")"
    if PKG_LIST "${server}" | grep -q .; then
        ok "PG ${major} was pulled in by the extension package's dependency"
    else
        bad "${server} was not installed — the extension package names it wrongly"
    fi
    pgvector="$(PGVECTOR_PACKAGE "${major}")"
    if PKG_LIST "${pgvector}" | grep -q .; then
        ok "pgvector was pulled in for PG ${major}"
    else
        bad "pgvector was not installed for PG ${major}"
    fi
done

step "layout"

for major in ${PG_MAJORS}; do
    lib="$(PG_LIB "${major}")/postvec.so"
    ext="$(PG_EXT "${major}")"
    [[ -f "${lib}" ]] && ok "PG ${major}: library at ${lib}" || bad "PG ${major}: no ${lib}"
    [[ -f "${ext}/postvec.control" ]] \
        && ok "PG ${major}: control file" || bad "PG ${major}: no control file"
    [[ -f "${ext}/postvec--${POSTVEC_VERSION}.sql" ]] \
        && ok "PG ${major}: install SQL for ${POSTVEC_VERSION}" \
        || bad "PG ${major}: no install SQL"

    # Root-owned and not writable: nothing in a running system should be able
    # to replace an extension library in place.
    mode="$(stat -c %a "${lib}")"
    owner="$(stat -c %U:%G "${lib}")"
    [[ "${mode}" == 644 && "${owner}" == root:root ]] \
        && ok "PG ${major}: library is root-owned and not writable" \
        || bad "PG ${major}: library is ${owner} mode ${mode}"
done

if (( MINIMAL )); then
    step "the bundled model"
    printf '  skip  --minimal: the model package was not installed\n'
else
    step "the bundled model"
    MODEL_ROOT="/opt/postvec/models/${BUNDLED_MODEL_BACKEND}/${BUNDLED_MODEL_NAME}"
    [[ -f "${MODEL_ROOT}/ninference.hub.json" ]] \
        && ok "the descriptor is at ${MODEL_ROOT}" \
        || bad "no descriptor at ${MODEL_ROOT}"

    # The package manager owns the tree, which is what makes it read-only to
    # the engine and what `postvec model rm` refuses to touch.
    owner="$(PKG_OWNER "${MODEL_ROOT}/ninference.hub.json")"
    [[ "${owner}" == "${MODEL_PKG_NAME}" ]] \
        && ok "the model tree is owned by ${MODEL_PKG_NAME}" \
        || bad "the model tree is owned by: ${owner:-<nothing>}"

    # The CLI's install receipt is deliberately not shipped: it records
    # `installed_at`, so a package carrying one would differ on every clean
    # rebuild. Package integrity comes from dpkg/rpm, from
    # model-files.sha256, and from the release checksums — so `model show
    # --verify` having nothing to verify here is the designed outcome, not a
    # gap.
    [[ -e "${MODEL_ROOT}/.postvec-install.json" ]] \
        && bad "the package ships the CLI install receipt" \
        || ok "no CLI install receipt in a package-owned tree"
    if find /opt/postvec/models \
            \( -name '.staging' -o -name '.trash' -o -name '.swap' \
               -o -name '.postvec.lock' \) 2>/dev/null | grep -q .; then
        bad "a transaction path was installed under the model root"
    else
        ok "no transaction or lock path under the model root"
    fi

    report="$(postvec model show --path /opt/postvec \
                  "${BUNDLED_MODEL_NAME}" 2>&1 || true)"
    grep -qi 'package' <<<"${report}" \
        && ok "postvec model show reports it as package-owned" \
        || bad "postvec model show did not report package ownership: $(head -3 <<<"${report}")"

    removal="$(postvec model rm --path /opt/postvec \
                   "${BUNDLED_MODEL_NAME}" --yes 2>&1 || true)"
    if [[ -f "${MODEL_ROOT}/ninference.hub.json" ]]; then
        ok "postvec model rm refused a package-owned model"
    else
        bad "postvec model rm deleted a package-owned model: ${removal}"
    fi

    # Package integrity for a package-owned model tree is dpkg/rpm plus these
    # digests — there is deliberately no CLI receipt to verify against.
    #
    # Minimal container images do not install documentation: Debian's slim
    # images carry `path-exclude=/usr/share/doc/*` in /etc/dpkg/dpkg.cfg.d, and
    # the EL images set `tsflags=nodocs`. The file is *in* the package —
    # `dpkg -L` lists it and verify-package.sh asserts it — so its absence here
    # is the image's documentation policy, not a defect, and saying so beats
    # both a false failure and a silent pass.
    digests="/usr/share/doc/${MODEL_PKG_NAME}/model-files.sha256"
    if [[ -f "${digests}" ]]; then
        if ( cd "${MODEL_ROOT}" && sha256sum --quiet --check "${digests}" ); then
            ok "every installed model file matches ${digests}"
        else
            bad "the installed model does not match ${digests}"
        fi
    elif PKG_FILES "${MODEL_PKG_NAME}" | grep -qx "${digests}"; then
        printf '  skip  %s is in the package but excluded by this image'\''s doc policy\n' \
            "${digests##*/}"
    else
        bad "the model package does not ship ${digests}"
    fi
fi

step "coexistence"

# The reason the CLI is its own package: with two majors installed, exactly one
# package may own /usr/bin/postvec.
owner="$(PKG_OWNER /usr/bin/postvec)"
[[ "${owner}" == "postvec-cli" ]] \
    && ok "/usr/bin/postvec is owned by postvec-cli alone" \
    || bad "/usr/bin/postvec is owned by: ${owner}"

want=$(wc -w <<<"${PG_MAJORS}")
got=$(EXTENSION_PACKAGES | grep -c . || true)
[[ "${got}" == "${want}" ]] \
    && ok "${got} per-major extension package(s) installed side by side" \
    || bad "expected ${want} extension packages, found ${got}"

step "the package changed nothing"

# postvec's own packages must start nothing. PostgreSQL's Debian packaging
# does create and start a default cluster from its postinst — that is its
# business, not postvec's — so this checks for a postvec-managed cluster, not
# for any postgres process at all.
if [[ -d /etc/postgresql ]] && find /etc/postgresql -name '*postvec*' 2>/dev/null | grep -q .; then
    bad "a postvec configuration file appeared during package installation"
else
    ok "postvec started and configured nothing"
fi
for dir in /etc/postgresql /var/lib/pgsql; do
    if [[ -d "${dir}" ]] && find "${dir}" -name '*postvec*' 2>/dev/null | grep -q .; then
        bad "the package wrote PostgreSQL configuration under ${dir}"
    fi
done
ok "no PostgreSQL configuration was written"
[[ -e /var/lib/postvec ]] \
    && bad "the package created CLI-owned state; that belongs to postvec setup" \
    || ok "no CLI state was created"

# The external-provider credential directory is deliberately not packaged: only
# `postvec setup --embedded` knows which account owns the cluster, and a
# package-declared owner would be applied at unpack time, before PostgreSQL's
# own packages have created it. Absent here is also the zero-config state the
# host reads as "no external providers configured".
[[ -e /etc/postvec ]] \
    && bad "the package created /etc/postvec; the credential directory belongs to postvec setup" \
    || ok "no credential directory was created"

step "the CLI works"

version="$(postvec --version 2>&1 || true)"
[[ "${version}" == *"${POSTVEC_VERSION}"* ]] \
    && ok "postvec --version reports ${POSTVEC_VERSION}" \
    || bad "postvec --version said: ${version}"

# ---------------------------------------------------------- the runtime check
#
# The point of the whole exercise: this distribution's PostgreSQL must be able
# to load the library this distribution's package installed. A package that is
# perfectly formed and cannot be loaded is worse than one that fails to build.
#
# No init system here, so the cluster is driven with pg_ctl directly.

step "the extension loads into PostgreSQL ${FIRST_MAJOR}"

BIN="$(PG_BIN "${FIRST_MAJOR}")"
DATA=/var/lib/postvec-test/data
CONF_D=/etc/postvec-test/conf.d
SOCKET_DIR=/var/run/postgresql
# initdb refuses a non-empty directory, so the configuration directory lives
# outside the data directory — which is also how a real deployment is laid out.
mkdir -p "$(dirname "${DATA}")" "${CONF_D}" "${SOCKET_DIR}"
chown -R "${PG_USER}:${PG_USER}" "$(dirname "${DATA}")" "${CONF_D}" "${SOCKET_DIR}"
# Do not inherit the Docker daemon's umask for security-sensitive fixtures.
# In particular, Snap-packaged Docker may give docker exec an umask of 000;
# postvec must (and does) reject a group- or world-writable configuration path.
chmod 0755 "$(dirname "${DATA}")" "$(dirname "${CONF_D}")" "${CONF_D}"
chmod 2775 "${SOCKET_DIR}"

as_pg() { su "${PG_USER}" -s /bin/bash -c "$*"; }

as_pg "${BIN}/initdb -D ${DATA} -A trust --username=${PG_USER}" >/tmp/initdb.log 2>&1 \
    && ok "initdb" || bad "initdb failed: $(tail -3 /tmp/initdb.log)"

# An absolute `include_dir` is what `postvec setup --config-dir` requires: the
# CLI refuses to invent the relationship between a directory and the
# configuration that includes it. This is the documented explicit-installation
# flow, and it is identical on both families.
#
# The socket directory and port are left at their defaults so the CLI's own
# connection defaults reach this cluster — that is the flow an operator has.
cat >> "${DATA}/postgresql.conf" <<EOF
include_dir = '${CONF_D}'
unix_socket_directories = '${SOCKET_DIR}'
EOF
chown "${PG_USER}:${PG_USER}" "${DATA}/postgresql.conf"

start_cluster() {
    as_pg "${BIN}/pg_ctl -D ${DATA} -l /tmp/pg.log -w -t 60 start" >/tmp/pgctl.log 2>&1
}
stop_cluster() {
    as_pg "${BIN}/pg_ctl -D ${DATA} -m fast -w -t 60 stop" >/dev/null 2>&1 || true
}
psql_() { as_pg "${BIN}/psql -h ${SOCKET_DIR} -d postgres -tAX -v ON_ERROR_STOP=1 $*"; }

if start_cluster; then ok "the cluster starts"; else bad "cluster did not start: $(tail -5 /tmp/pg.log)"; fi

if psql_ "-c \"CREATE EXTENSION postvec CASCADE\"" >/tmp/create.log 2>&1; then
    ok "CREATE EXTENSION postvec CASCADE"
else
    bad "CREATE EXTENSION failed: $(tail -3 /tmp/create.log)"
fi

installed="$(psql_ "-c \"SELECT extversion FROM pg_extension WHERE extname='postvec'\"" 2>/dev/null || true)"
[[ "${installed}" == "${POSTVEC_VERSION}" ]] \
    && ok "the catalog reports ${installed}" \
    || bad "the catalog reports '${installed}', expected ${POSTVEC_VERSION}"

library="$(psql_ "-c 'SELECT postvec.version()'" 2>/dev/null || true)"
[[ "${library}" == "${POSTVEC_VERSION}" ]] \
    && ok "the loaded library reports ${library}" \
    || bad "the library reports '${library}' — the .so did not load, or is the wrong build"

capable="$(psql_ "-c \"SELECT (postvec.build_info()->'features'->>'embedded')::bool\"" 2>/dev/null || true)"
[[ "${capable}" == t ]] \
    && ok "the packaged library is the embedded-capable build" \
    || bad "build_info().features.embedded is '${capable}'"

step "postvec setup / doctor / uninstall"

# The explicit-installation flow: no postgresql-common anywhere in it, so it is
# the same on both families. `--no-restart` exits 4 by contract — the operator
# owns the restart when the CLI cannot own the service.
setup_status=0
postvec setup \
    --pg-config "${BIN}/pg_config" \
    --config-dir "${CONF_D}" \
    --database postgres \
    --grpc 127.0.0.1:33333 \
    --http https://127.0.0.1:22222 \
    --no-restart --yes --allow-unreachable >/tmp/setup.log 2>&1 || setup_status=$?
if [[ "${setup_status}" == 4 ]]; then
    ok "postvec setup applied its changes and deferred the restart (exit 4)"
elif [[ "${setup_status}" == 0 ]]; then
    ok "postvec setup completed"
else
    bad "postvec setup exited ${setup_status}: $(tail -5 /tmp/setup.log)"
fi

if grep -q postvec "${CONF_D}"/*.conf 2>/dev/null; then
    ok "setup wrote an owned configuration snippet"
else
    bad "setup wrote no configuration: $(ls -A "${CONF_D}" 2>/dev/null || echo empty)"
fi

# Restart so the preload takes effect and the worker actually runs.
stop_cluster
if start_cluster; then ok "the cluster restarts with postvec preloaded"; else bad "restart failed: $(tail -5 /tmp/pg.log)"; fi

# The worker is what a preload buys; prove it started.
beat=""
for _ in $(seq 1 30); do
    beat="$(psql_ "-c 'SELECT worker_last_beat IS NOT NULL FROM postvec.stats()'" 2>/dev/null || true)"
    [[ "${beat}" == t ]] && break
    sleep 1
done
[[ "${beat}" == t ]] && ok "the background worker is running and beating" \
                     || bad "no worker heartbeat within 30s: $(tail -5 /tmp/pg.log)"

# ------------------------------------------------------------------- doctor
#
# A report is not a diagnosis. Accepting "some JSON was printed" lets a doctor
# that has stopped evaluating anything satisfy the package gate by printing an
# empty check list — so what is asserted here is the *verdict*: which checks
# ran, what each concluded, and whether the exit code agrees with the state the
# cluster is actually in.

DOCTOR_JSON=/tmp/doctor.json
doctor_run() {
    doctor_status=0
    postvec doctor --pg-config "${BIN}/pg_config" --config-dir "${CONF_D}" \
        --database postgres --format json >"${DOCTOR_JSON}" 2>/tmp/doctor.err \
        || doctor_status=$?
}
doctor_status_of() { jq -r --arg id "$1" '[.checks[] | select(.id == $id) | .status] | first // "MISSING"' "${DOCTOR_JSON}"; }
doctor_blocking()  { jq -r '[.checks[] | select(.status == "FAIL" or (.required and .status == "SKIP")) | .id] | join(" ")' "${DOCTOR_JSON}"; }

# `must_pass <lifecycle label> <id>...`
doctor_must_pass() {
    local label="$1"; shift
    local id status broken=""
    for id in "$@"; do
        status="$(doctor_status_of "${id}")"
        [[ "${status}" == PASS ]] || broken="${broken} ${id}=${status}"
    done
    if [[ -z "${broken}" ]]; then
        ok "doctor (${label}): $# check(s) PASS — $*"
    else
        bad "doctor (${label}): expected PASS, got${broken}"
    fi
}

doctor_run

if jq -e '.schema_version == 1 and (.checks | length) > 0' "${DOCTOR_JSON}" >/dev/null 2>&1; then
    ok "postvec doctor produced a schema-1 report with $(jq '.checks | length' "${DOCTOR_JSON}") check(s)"
else
    bad "postvec doctor produced no usable report: $(tail -3 /tmp/doctor.err)"
fi

# Everything about the *installation* must be healthy at this point: the files
# are in place, the library loaded, the extension is installed at the version
# the package carries, and the worker the preload bought is beating.
doctor_must_pass "remote mode, no engine" \
    cluster.connection cluster.assets.postvec cluster.assets.vector \
    cluster.preload extension.installed extension.version extension.build-info \
    worker.heartbeat

# …and the one thing that is genuinely wrong — there is no inference host on this
# host — must be what doctor reports, with the exit code to match. A doctor that
# returned success here would be worse than useless.
blocking="$(doctor_blocking)"
if [[ "${doctor_status}" == 1 ]]; then
    ok "doctor exits 1 while the configured endpoints are unreachable"
else
    bad "doctor exited ${doctor_status} with no reachable engine; expected 1"
fi
if grep -qE '(^| )remote\.' <<<"${blocking}"; then
    ok "doctor names the unreachable endpoint: ${blocking}"
else
    bad "doctor did not report the missing engine; blocking checks were: ${blocking:-<none>}"
fi
# Nothing *else* may be blocking. A doctor that fails checks unrelated to the
# fault teaches operators to ignore it.
# `|| true`: grep exits 1 when it filters everything out, which here is the
# *good* outcome — and under `set -e` a command substitution that fails takes
# the whole test down before it can report anything.
unexpected="$(tr ' ' '\n' <<<"${blocking}" \
    | grep -vE '^(remote\.|inference\.|models\.|$)' | tr '\n' ' ' || true)"
[[ -z "${unexpected// /}" ]] \
    && ok "no unrelated check is blocking" \
    || bad "unrelated blocking checks: ${unexpected}"

# --------------------------------------------------------------- inference
#
# Only meaningful when the engine assets are installed. This is the assertion
# that proves the whole embedded stack — package, ONNX Runtime, model bundle,
# descriptor — works on this distribution, not just on the one it was built on.

if (( MINIMAL )); then
    step "inference"
    printf '  skip  --minimal: engine assets were not installed\n'
else
    step "inference with the bundled model"
    embedded_status=0
    postvec setup \
        --pg-config "${BIN}/pg_config" \
        --config-dir "${CONF_D}" \
        --database postgres \
        --embedded --path /opt/postvec \
        --model "${BUNDLED_MODEL_NAME}" \
        --switch-mode --no-restart --yes --allow-unreachable \
        >/tmp/setup-embedded.log 2>&1 || embedded_status=$?
    if [[ "${embedded_status}" == 0 || "${embedded_status}" == 4 ]]; then
        ok "postvec setup switched the cluster to embedded mode (exit ${embedded_status})"
    else
        bad "switching to embedded mode failed (exit ${embedded_status}): $(tail -5 /tmp/setup-embedded.log)"
    fi
    grep -q "postvec.mode *= *'embedded'" "${CONF_D}"/*.conf 2>/dev/null \
        && ok "the snippet selects embedded mode" \
        || bad "the snippet does not select embedded mode: $(cat "${CONF_D}"/*.conf 2>/dev/null)"

    stop_cluster
    start_cluster || bad "the cluster did not restart in embedded mode"

    # Model loading is slow and happens after the postmaster is accepting
    # connections, so this polls rather than assuming.
    dims=""
    for _ in $(seq 1 90); do
        dims="$(psql_ "-c \"SELECT vector_dims(postvec.embed('package install test','${BUNDLED_MODEL_NAME}')::vector)\"" 2>/dev/null || true)"
        [[ -n "${dims}" ]] && break
        sleep 2
    done
    if [[ "${dims}" == "${BUNDLED_MODEL_TARGET_DIM}" ]]; then
        ok "the bundled model embeds text (${dims} dimensions)"
    else
        bad "embed() returned '${dims}', expected ${BUNDLED_MODEL_TARGET_DIM}"
        printf '        models in the cache: %s\n' \
            "$(psql_ "-c \"SELECT coalesce(string_agg(name, ' '), '(none)') FROM postvec.models\"" 2>&1 | head -2)"
        printf '        engine log:\n'
        grep -iE 'postvec|engine|onnx' /tmp/pg.log | tail -15 | sed 's/^/          /'
    fi

    # The other half of the doctor contract: a *healthy* verdict once the
    # installation is actually complete. Asserting only that doctor reports
    # faults would be satisfied by a doctor that reports faults unconditionally.
    #
    # Polled, because the model cache refreshes on the worker's own cadence
    # shortly after the engine comes up — but bounded, so "it never became
    # healthy" is a failure and not a wait.
    for _ in $(seq 1 30); do
        doctor_run
        [[ "${doctor_status}" == 0 ]] && break
        sleep 2
    done
    if [[ "${doctor_status}" == 0 ]]; then
        ok "doctor reports a healthy cluster in embedded mode (exit 0)"
    else
        bad "doctor still exits ${doctor_status} in embedded mode; blocking: $(doctor_blocking)"
    fi
    doctor_must_pass "embedded, engine up" \
        cluster.mode embedded.build-capability embedded.path \
        embedded.onnx-runtime embedded.grpc-listener embedded.http-listener \
        embedded.loaded-models embedded.cache-consistency models.cache

    # `inference.probe` exists only to report that the engine could not be
    # probed at all — when the probe succeeds, the per-mode `embedded.*` checks
    # above are what it produces instead. So its *absence* is the assertion
    # here, and its presence would mean doctor reported healthy while knowing
    # nothing about the inference side.
    probe="$(doctor_status_of inference.probe)"
    [[ "${probe}" == MISSING ]] \
        && ok "doctor probed the engine (no inference.probe fallback in the report)" \
        || bad "doctor fell back to inference.probe=${probe}; the engine was not examined"

    # ----------------------------------------------------- external providers
    #
    # Files only: no provider is contacted (`--no-verify`), because a package
    # test must not need an API key or the internet. What it proves is the
    # packaging-shaped half — that the released CLI can create the credential
    # directory the packages deliberately do not ship, with the right mode and
    # owner, and round-trip a connector file through the shipped binary — and,
    # since this full cell has a live embedded host, that host and CLI agree
    # honestly about a key source that cannot resolve.
    step "external providers (no network)"

    PROVIDERS_D=/etc/postvec/providers.d
    if [[ -d "${PROVIDERS_D}" ]]; then
        dir_mode="$(stat -c %a "${PROVIDERS_D}")"
        dir_owner="$(stat -c %U "${PROVIDERS_D}")"
        [[ "${dir_mode}" == 700 && "${dir_owner}" == "${PG_USER}" ]] \
            && ok "setup --embedded created ${PROVIDERS_D} (0700, ${dir_owner})" \
            || bad "${PROVIDERS_D} is ${dir_owner} mode ${dir_mode}, expected ${PG_USER} 700"
    else
        bad "postvec setup --embedded did not create ${PROVIDERS_D}"
    fi

    provider_status=0
    postvec provider add openai \
        --pg-config "${BIN}/pg_config" --config-dir "${CONF_D}" \
        --model text-embedding-3-small \
        --api-key-env POSTVEC_PACKAGE_TEST_KEY \
        --no-verify --yes >/tmp/provider-add.log 2>&1 || provider_status=$?
    # The embedded host is up (asserted just above) and reads the file at
    # reload — and POSTVEC_PACKAGE_TEST_KEY is deliberately not exported in
    # the postmaster's environment. The designed outcome is therefore partial
    # (exit 3): the host fails closed on the unresolvable key source, serves
    # nothing from the file, and the CLI reports that instead of claiming
    # success. Exit 0 here would mean the shipped host accepted a key it
    # cannot resolve.
    if [[ "${provider_status}" == 3 ]] \
        && grep -q 'INCOMPLETE: provider file problem reported by the host' /tmp/provider-add.log \
        && grep -q 'POSTVEC_PACKAGE_TEST_KEY' /tmp/provider-add.log \
        && grep -q '0 provider(s), 0 model(s) now served' /tmp/provider-add.log; then
        ok "provider add wrote the file; the host honestly refused the unresolvable test key (exit 3)"
    else
        bad "postvec provider add exited ${provider_status}: $(tail -5 /tmp/provider-add.log)"
    fi

    if [[ -f "${PROVIDERS_D}/openai.toml" ]]; then
        file_mode="$(stat -c %a "${PROVIDERS_D}/openai.toml")"
        file_owner="$(stat -c %U "${PROVIDERS_D}/openai.toml")"
        [[ "${file_mode}" == 600 && "${file_owner}" == "${PG_USER}" ]] \
            && ok "the connector file is ${file_owner}-owned and 0600" \
            || bad "openai.toml is ${file_owner} mode ${file_mode}, expected ${PG_USER} 600"
    else
        bad "no ${PROVIDERS_D}/openai.toml after provider add"
    fi

    listing="$(postvec provider ls --pg-config "${BIN}/pg_config" \
                   --config-dir "${CONF_D}" 2>&1 || true)"
    grep -q 'POSTVEC_PACKAGE_TEST_KEY' <<<"${listing}" \
        && ok "provider ls names the key *source*" \
        || bad "provider ls did not report the key source: $(head -3 <<<"${listing}")"

    postvec provider rm openai --pg-config "${BIN}/pg_config" \
        --config-dir "${CONF_D}" --yes >/tmp/provider-rm.log 2>&1 || true
    [[ -e "${PROVIDERS_D}/openai.toml" ]] \
        && bad "provider rm left the connector file behind: $(tail -3 /tmp/provider-rm.log)" \
        || ok "provider rm removed the connector file"
fi

# ------------------------------------------------------------ the inference node
#
# The full install is also what a *node* host installs, so the packaged node
# is started here against the packaged runtime and model — the one place
# where this distribution's OpenSSL, libgomp and libc are proven to resolve for
# the postvec-server binary, and where the packaged unit's engine root
# (/opt/postvec, via the drop-in) is proven to hold what the node needs.
#
# No systemd in a container, so the unit is not started: the assertions are
# that the package laid out what the unit needs (account, unit, drop-in,
# configuration, lease directory contract), and then the binary is run the way
# the unit would run it — as the service account, with the packaged config,
# `--insecure` because the test provisions no certificate.

if (( MINIMAL )); then
    step "the inference node"
    printf '  skip  --minimal: postvec-server was not installed\n'
else
    step "the inference node"

    getent passwd postvec-server >/dev/null \
        && ok "the postvec-server account exists" \
        || bad "no postvec-server account was created"
    shell="$(getent passwd postvec-server | cut -d: -f7)"
    [[ "${shell}" == */nologin ]] \
        && ok "the account cannot log in (${shell})" \
        || bad "the account's shell is ${shell}"

    [[ -f /usr/lib/systemd/system/postvec-server.service ]] \
        && ok "the unit is installed" \
        || bad "no /usr/lib/systemd/system/postvec-server.service"
    dropin=/usr/lib/systemd/system/postvec-server.service.d/packaged.conf
    if [[ -f "${dropin}" ]] && grep -q '^Environment=POSTVEC_SERVER_ROOT=/opt/postvec$' "${dropin}"; then
        ok "the packaged drop-in points the unit at /opt/postvec"
    else
        bad "no drop-in setting POSTVEC_SERVER_ROOT=/opt/postvec"
    fi
    [[ -f /etc/postvec-server/config.json ]] \
        && ok "/etc/postvec-server/config.json is installed" \
        || bad "no /etc/postvec-server/config.json"
    [[ -d /var/lib/postvec-server ]] \
        && ok "/var/lib/postvec-server exists" \
        || bad "no /var/lib/postvec-server"
    owner="$(PKG_OWNER /usr/bin/postvec-server)"
    [[ "${owner}" == postvec-server ]] \
        && ok "/usr/bin/postvec-server is owned by postvec-server" \
        || bad "/usr/bin/postvec-server is owned by: ${owner}"

    # The one package under different terms says so in its own metadata.
    licence="$(PKG_LICENSE postvec-server)"
    [[ "${licence}" == "${SERVER_LICENSE}" ]] \
        && ok "postvec-server declares licence ${SERVER_LICENSE}" \
        || bad "postvec-server declares licence '${licence}', expected ${SERVER_LICENSE}"

    # Installing the package started nothing.
    if pgrep -x postvec-server >/dev/null 2>&1; then
        bad "a postvec-server process is running after package installation"
    else
        ok "installing the package started no node"
    fi

    version="$(postvec-server --version 2>&1 || true)"
    [[ "${version}" == *"${POSTVEC_VERSION}"* ]] \
        && ok "postvec-server --version reports ${POSTVEC_VERSION}" \
        || bad "postvec-server --version said: ${version}"

    # What the unit's ExecStartPre does, and `setpriv` for its User=. Not
    # `runuser`: that opens a PAM session that, on SIGTERM, kills the child
    # after a grace period rather than letting it drain — the test would then
    # be measuring runuser. setpriv just drops privileges and execs, so the
    # PID below is the node itself. curl and openssl are test tools, like jq
    # above; a node host needs neither.
    install -d -m 1775 -o root -g postvec-server /run/lock/postvec
    # EL9's base image ships curl-minimal, which provides the curl command
    # and conflicts with the full `curl` package unless dnf may erase it.
    # Installing `curl` there is redundant and fails; only openssl is needed.
    if [[ "${DIST_FAMILY}" == deb ]]; then
        PKG_INSTALL curl openssl
    else
        PKG_INSTALL openssl
    fi
    # A certificate pair the way an operator supplies one — absolute paths on
    # the command line override the packaged config's relative defaults — so
    # the discovery listener is exercised over TLS, as it is in production.
    install -d -m 0750 -o postvec-server -g postvec-server /tmp/node-certs
    openssl req -x509 -newkey rsa:2048 -nodes -days 2 -subj "/CN=postvec-server" \
        -addext "subjectAltName=DNS:localhost,IP:127.0.0.1" \
        -keyout /tmp/node-certs/server.key -out /tmp/node-certs/server.crt >/dev/null 2>&1
    chown postvec-server:postvec-server /tmp/node-certs/server.key /tmp/node-certs/server.crt
    chmod 0600 /tmp/node-certs/server.key
    # Loopback only: this is a test container, not a node on a network.
    setpriv --reuid=postvec-server --regid=postvec-server --init-groups postvec-server \
        --config /etc/postvec-server/config.json \
        --root /opt/postvec --bind 127.0.0.1 \
        --ssl-cert /tmp/node-certs/server.crt --ssl-cert-key /tmp/node-certs/server.key \
        >/tmp/node.log 2>&1 &
    node_pid=$!
    deadline=$(( SECONDS + 240 ))
    ready=""
    while (( SECONDS < deadline )); do
        ready="$(curl --silent --insecure --output /dev/null --write-out '%{http_code}' \
                    https://127.0.0.1:22222/ready 2>/dev/null || true)"
        [[ "${ready}" == 200 ]] && break
        kill -0 "${node_pid}" 2>/dev/null || break
        sleep 3
    done
    if [[ "${ready}" == 200 ]]; then
        ok "the packaged node serves the packaged model as the service account, over TLS"
        config="$(curl --silent --insecure https://127.0.0.1:22222/config 2>/dev/null || true)"
        grep -q "${BUNDLED_MODEL_NAME}" <<<"${config}" \
            && ok "/config advertises ${BUNDLED_MODEL_NAME} from /opt/postvec" \
            || bad "/config does not advertise ${BUNDLED_MODEL_NAME}: ${config:0:200}"
        if status_out="$(postvec-server status 2>&1)"; then
            ok "postvec-server status reports healthy (exit 0)"
        else
            bad "postvec-server status failed: ${status_out:0:200}"
        fi
        kill -TERM "${node_pid}"
        if wait "${node_pid}"; then
            ok "the node drained and exited 0 on SIGTERM"
        else
            bad "the node exited non-zero on SIGTERM: $(tail -5 /tmp/node.log)"
        fi
    else
        bad "the node never became ready (last /ready: ${ready:-none}): $(tail -10 /tmp/node.log)"
        kill "${node_pid}" 2>/dev/null || true
    fi
fi

# ------------------------------------------------------------ detached symbols
#
# The debug packages are mandatory in a release, so they are tested the way
# someone debugging a crashed backend would use them: install, then check that
# the detached file is where the loader looks and that it belongs to the
# library actually installed. A -dbgsym whose build id does not match the
# shipped library is indistinguishable from a working one until the moment
# somebody needs a backtrace, which is the worst possible moment to find out.

if [[ -d /debug-packages ]] && compgen -G '/debug-packages/*' >/dev/null; then
    step "detached symbols"

    PKG_INSTALL binutils
    if [[ "${DIST_FAMILY}" == deb ]]; then
        PKG_INSTALL /debug-packages/*.deb
    else
        PKG_INSTALL /debug-packages/*.rpm
    fi
    ok "the debug packages install on top of the release packages"

    build_id_of() { readelf -n "$1" 2>/dev/null | awk '/Build ID:/ {print $3; exit}'; }

    for major in ${PG_MAJORS}; do
        lib="$(PG_LIB "${major}")/postvec.so"
        detached="/usr/lib/debug${lib}.debug"
        if [[ ! -f "${detached}" ]]; then
            bad "PG ${major}: no detached symbols at ${detached}"
            continue
        fi
        ok "PG ${major}: symbols are at ${detached}"
        if [[ "$(build_id_of "${lib}")" == "$(build_id_of "${detached}")" ]]; then
            ok "PG ${major}: the symbols belong to the installed library"
        else
            bad "PG ${major}: build id mismatch between library and symbols"
        fi
        # What the whole exercise is for: the stripped library must point at
        # the file that was installed, and that file must hold real DWARF.
        #
        # Asserted by reading the link and the target separately rather than by
        # asking readelf to follow it: EL9 ships binutils 2.35, which does not
        # follow debug links at all and has no --follow-links. A check written
        # that way passes on Debian, fails on EL9, and measures the toolchain
        # rather than the packages.
        link="$(readelf --debug-dump=links "${lib}" 2>/dev/null || true)"
        if [[ "${link}" == *"$(basename "${detached}")"* && "${link}" == *"CRC value"* ]]; then
            ok "PG ${major}: the library links to ${detached##*/} with a CRC"
        else
            bad "PG ${major}: the library carries no usable .gnu_debuglink"
        fi
        if readelf --sections "${detached}" 2>/dev/null | grep '\.debug_line' >/dev/null; then
            ok "PG ${major}: the detached file carries line tables"
        else
            bad "PG ${major}: the detached file has no DWARF line information"
        fi
    done

    cli_detached=/usr/lib/debug/usr/bin/postvec.debug
    if [[ -f "${cli_detached}" ]] \
        && [[ "$(build_id_of /usr/bin/postvec)" == "$(build_id_of "${cli_detached}")" ]]; then
        ok "the CLI's symbols are installed and match the binary"
    else
        bad "the CLI's detached symbols are missing or do not match"
    fi

    server_detached=/usr/lib/debug/usr/bin/postvec-server.debug
    if [[ -f "${server_detached}" ]] \
        && [[ "$(build_id_of /usr/bin/postvec-server)" == "$(build_id_of "${server_detached}")" ]]; then
        ok "the node's symbols are installed and match the binary"
    else
        bad "the node's detached symbols are missing or do not match"
    fi
fi

step "uninstall and removal"

uninstall_status=0
postvec uninstall --pg-config "${BIN}/pg_config" --config-dir "${CONF_D}" \
    --database postgres --no-restart --yes >/tmp/uninstall.log 2>&1 || uninstall_status=$?
remaining="$(psql_ "-c \"SELECT count(*) FROM pg_extension WHERE extname='postvec'\"" 2>/dev/null || echo '?')"
[[ "${remaining}" == 0 ]] \
    && ok "postvec uninstall removed the extension (exit ${uninstall_status})" \
    || bad "the extension is still installed after uninstall: $(tail -3 /tmp/uninstall.log)"

stop_cluster

PKG_REMOVE "$(EXTENSION_PACKAGES | head -1)" 2>/dev/null || true
[[ ! -f "$(PG_LIB "${FIRST_MAJOR}")/postvec.so" ]] \
    && ok "removing the package took the library away" \
    || bad "the library survived package removal"
[[ -d "${DATA}/base" ]] \
    && ok "removal left the data directory alone" \
    || bad "removal touched the data directory"

printf '\n%d passed, %d failed\n' "${passed}" "${failed}"
(( failed == 0 ))
