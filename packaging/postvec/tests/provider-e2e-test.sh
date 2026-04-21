#!/usr/bin/env bash
# Packaged external-provider gate. One scenario suite through the shipped
# artifacts: trigger -> queue -> worker -> host gateway -> provider ->
# write-back -> search. No internet, no paid request, no real credential
# (the mock is tests/provider-mock.py, loopback inside the inference container).
#
#   tests/provider-e2e-test.sh --target package --distro debian12 --pg 18
#   tests/provider-e2e-test.sh --target package --distro el9 --pg 18
#   tests/provider-e2e-test.sh --target image --variant complete <image>
#   tests/provider-e2e-test.sh --target image --variant remote \
#       --server-image postvec-server:amd64 <image>
#
# The scenarios:
#   A  live verification: `provider add` probes the mock (no --no-verify)
#   B  success + write-back + hybrid search
#   C  bounded upstream retry (429, 500, success — exactly three requests)
#   D  oversized response → dead letter naming the budget, bounded RSS
#   E  slow/trickling response → deadline, bounded retries, bounded RSS
#   F  connector reload + key rotation (key A → key B, no restart)
#   G  in-flight crash of the serving process + recovery
#
# Speed knobs (all SIGHUP; recorded in the output): max_retries=2,
# retry_backoff_ms=500, job_visibility_timeout_ms=5000, poll_interval_ms=1000,
# embed_timeout_ms=20000 (8000 during E only).

set -Eeuo pipefail

TESTS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PKG_DIR="$(cd "${TESTS_DIR}/.." && pwd)"
# shellcheck source=../scripts/lib.sh
source "${PKG_DIR}/scripts/lib.sh"

TARGET=""; DISTRO=debian12; PG_MAJOR=18; RELEASE_ARCH=""
VARIANT=complete; SERVER_IMAGE=""; IMAGE=""
while (($#)); do
    case "$1" in
    --target)       TARGET="$2"; shift 2 ;;
    --distro)       DISTRO="$2"; shift 2 ;;
    --pg)           PG_MAJOR="$2"; shift 2 ;;
    --arch)         RELEASE_ARCH="$2"; shift 2 ;;
    --variant)      VARIANT="$2"; shift 2 ;;
    --server-image) SERVER_IMAGE="$2"; shift 2 ;;
    -h|--help)      sed -n '2,28p' "$0"; exit 0 ;;
    *)              IMAGE="$1"; shift ;;
    esac
done
[[ -n "${TARGET}" ]] || die "pass --target package|image (see --help)"
RELEASE_ARCH="${RELEASE_ARCH:-$(host_release_arch)}"
load_versions
need docker
need python3

MOCK_PORT=8099
PROV_MODEL="openai-text-embedding-3-small"
KEYA="pv-e2e-alpha-$$-$RANDOM"
KEYB="pv-e2e-bravo-$$-$RANDOM"

RUN_ID="postvec-pv13-$$"
SRV_ID="${RUN_ID}-srv"
NETWORK="${RUN_ID}-net"
VOLUME="${RUN_ID}-data"
STAGE=""

passed=0 failed=0
ok()   { printf '  \033[32mok\033[0m    %s\n' "$*"; passed=$((passed + 1)); }
bad()  { printf '  \033[1;31mFAIL\033[0m  %s\n' "$*" >&2; failed=$((failed + 1)); }
step() { printf '\n%s\n' "$*"; }

cleanup() {
    if (( failed > 0 )); then
        echo "---- serving-container log tail ----" >&2
        docker logs --tail 40 "${SRV}" >&2 2>/dev/null || true
    fi
    docker rm --force "${RUN_ID}" "${SRV_ID}" >/dev/null 2>&1 || true
    docker volume rm "${VOLUME}" >/dev/null 2>&1 || true
    docker network rm "${NETWORK}" >/dev/null 2>&1 || true
    if [[ -n "${STAGE}" ]]; then rm -rf "${STAGE}"; fi
}
trap cleanup EXIT

# ---------------------------------------------------------------- adapters
#
# DB   — the container PostgreSQL runs in.
# SRV  — the container the provider-serving process runs in (== DB for the
#        embedded cells; the postvec-server container for the remote cell).
# The mock always runs inside SRV, on ITS loopback: the serving process dials
# 127.0.0.1, so no TLS and no allow_insecure_transport enter the test.
DB=""; SRV=""; EMBEDDED=1
BIN=""      # package cells: the PG bin dir inside the container

dbsql() {
    if [[ "${TARGET}" == package ]]; then
        printf '%s\n' "$1" | docker exec -i "${DB}" su postgres -s /bin/bash -c \
            "${BIN}/psql -h /var/run/postgresql -d postgres -tAX -v ON_ERROR_STOP=1 -f -"
    else
        printf '%s\n' "$1" | docker exec -i "${DB}" \
            psql -U app -d app -tAX -v ON_ERROR_STOP=1 -f -
    fi
}

# HTTP against loopback listeners inside a container, via the python3 the
# harness installed there (curl is not in every image under test).
container_http() { # <container> <method> <url> [body]
    docker exec -i "$1" python3 - "$2" "$3" "${4-}" <<'PY'
import sys, urllib.request
method, url, body = sys.argv[1], sys.argv[2], sys.argv[3]
data = body.encode() if body else None
req = urllib.request.Request(url, data=data, method=method)
print(urllib.request.urlopen(req, timeout=15).read().decode())
PY
}

mockctl()    { container_http "${SRV}" POST "http://127.0.0.1:${MOCK_PORT}/control" "$1" >/dev/null; }
mock_state() { # <field>
    container_http "${SRV}" GET "http://127.0.0.1:${MOCK_PORT}/control" \
        | python3 -c "import json,sys; print(json.load(sys.stdin)['$1'])"
}

start_mock() {
    docker exec -d "${SRV}" python3 /provider-mock.py "${MOCK_PORT}"
    wait_for "the provider mock answers /healthz" 30 mock_healthy
}

reload_providers() {
    if (( EMBEDDED )); then
        container_http "${SRV}" POST "http://127.0.0.1:33434/admin/providers/reload"
    else
        container_http "${SRV}" POST "http://127.0.0.1:22223/admin/providers/reload"
    fi
}

# The pid of the process that dials the provider: the postvec launcher
# bgworker (embedded) or postvec-server. Exactly one, or the RSS evidence
# would be about an unknown process.
serving_pid() {
    local mode needle
    if (( EMBEDDED )); then mode=cmdline; needle="postvec launcher"
    else mode=comm; needle="postvec-server"; fi
    docker exec -i "${SRV}" python3 - "${mode}" "${needle}" <<'PY'
import os, sys
mode, needle = sys.argv[1], sys.argv[2]
pids = []
me = str(os.getpid())
for p in os.listdir("/proc"):
    if not p.isdigit() or p == me:
        continue
    try:
        if mode == "comm":
            hit = open(f"/proc/{p}/comm").read().strip() == needle
        else:
            hit = needle.encode() in open(f"/proc/{p}/cmdline", "rb").read()
    except OSError:
        continue
    if hit:
        pids.append(p)
if len(pids) != 1:
    print(f"expected exactly one serving pid, found {pids}", file=sys.stderr)
    sys.exit(1)
print(pids[0])
PY
}

vm_hwm_kb() { # peak RSS of the serving process, in kB
    local pid
    pid="$(serving_pid)" || return 1
    docker exec "${SRV}" sed -n 's/^VmHWM:[[:space:]]*\([0-9]*\).*/\1/p' "/proc/${pid}/status"
}

wait_for() { # <description> <seconds> <command...>
    local desc="$1" deadline=$(( SECONDS + $2 )); shift 2
    while (( SECONDS < deadline )); do
        if "$@" >/dev/null 2>&1; then return 0; fi
        sleep 1
    done
    # The condition may become true during the final sleep. Check once at the
    # boundary instead of reporting a timeout from state sampled a second ago.
    if "$@" >/dev/null 2>&1; then return 0; fi
    echo "timed out waiting for: ${desc}" >&2
    return 1
}

filled_count() { dbsql "SELECT count(*) FROM e2e WHERE body_semantic IS NOT NULL"; }
filled_is()    { [[ "$(filled_count)" == "$1" ]]; }
dead_count()   { dbsql "SELECT count(*) FROM postvec.jobs_dead"; }
dead_is()      { [[ "$(dead_count)" == "$1" ]]; }
queue_count()  { dbsql "SELECT count(*) FROM postvec.jobs"; }
requests_are() { [[ "$(mock_state requests)" == "$1" ]]; }
db_alive()     { [[ "$(dbsql 'SELECT 1' 2>/dev/null)" == 1 ]]; }
model_cached() { [[ "$(dbsql "SELECT count(*) FROM postvec.models WHERE name = '${PROV_MODEL}'")" == 1 ]]; }
in_flight_nonzero() { [[ "$(mock_state in_flight)" -ge 1 ]]; }
hb_fresh() {
    # The cast matters (stats() reports text) and so does the window:
    # heartbeats are change-gated to heartbeat_interval_ms (default 30 s),
    # so freshness is the doctor's budget, not a tight double-sample.
    [[ "$(dbsql "SELECT worker_last_beat::timestamptz > now() - interval '45 seconds' FROM postvec.stats()" 2>/dev/null)" == t ]]
}
mock_healthy() { container_http "${SRV}" GET "http://127.0.0.1:${MOCK_PORT}/healthz" >/dev/null 2>&1; }
container_healthy() { # <container>
    [[ "$(docker inspect --format '{{.State.Health.Status}}' "$1" 2>/dev/null)" == healthy ]]
}

# ------------------------------------------------------------ target setup

setup_package() {
    distro_facts "${DISTRO}"
    arch_facts "${RELEASE_ARCH}"
    load_model_facts
    case "${DIST_FAMILY}" in
    deb) BIN="/usr/lib/postgresql/${PG_MAJOR}/bin" ;;
    rpm) BIN="/usr/pgsql-${PG_MAJOR}/bin" ;;
    esac

    # The full release package set for this one major, from its three roots —
    # the same selection the full install test stages.
    local common noarch cell
    common="$(common_dist_dir "${DISTRO}" "${RELEASE_ARCH}")"
    noarch="$(noarch_dist_dir "${DISTRO}")"
    cell="$(extension_dist_dir "${DISTRO}" "${PG_MAJOR}" "${RELEASE_ARCH}")"
    [[ -d "${common}" && -d "${noarch}" && -d "${cell}" ]] \
        || die "packages missing; build ${DISTRO}/pg${PG_MAJOR} first (scripts/release.sh --distro ${DISTRO} --pg ${PG_MAJOR})"
    STAGE="$(mktemp -d)"
    shopt -s nullglob
    local f
    for f in "${common}"/postvec-cli[-_]* "${common}"/postvec-onnxruntime[-_]* \
             "${noarch}"/postvec-model-* "${noarch}"/"${EXTRAS_METAPACKAGE}"[-_]* \
             "${cell}"/postgresql*postvec[-_]*; do
        case "${f}" in *-dbgsym[-_]*|*-debuginfo[-_]*|*.spdx.json) continue ;; esac
        cp "${f}" "${STAGE}/"
    done
    shopt -u nullglob

    log "PV-13 package cell: ${DISTRO} / PG ${PG_MAJOR} on ${DIST_BASE_IMAGE}"
    docker run --detach --name "${RUN_ID}" \
        --platform "${OCI_PLATFORM}" \
        --volume "${STAGE}:/packages:ro" \
        --volume "${PKG_DIR}/scripts/postvec-prerequisites.sh:/postvec-prerequisites.sh:ro" \
        --volume "${TESTS_DIR}/provider-e2e-install.sh:/provider-e2e-install.sh:ro" \
        --volume "${TESTS_DIR}/provider-mock.py:/provider-mock.py:ro" \
        --env "PG_MAJOR=${PG_MAJOR}" \
        --env "POSTVEC_VERSION=${POSTVEC_VERSION}" \
        --env "BUNDLED_MODEL_NAME=${MODEL_NAME}" \
        --env "DIST_FAMILY=${DIST_FAMILY}" \
        --env DEBIAN_FRONTEND=noninteractive \
        "${DIST_BASE_IMAGE}" sleep infinity >/dev/null
    DB="${RUN_ID}"; SRV="${RUN_ID}"; EMBEDDED=1

    docker exec "${DB}" bash /provider-e2e-install.sh \
        || die "the install phase failed; see above"
    ok "full install reaches a serving embedded cluster"
}

image_password_file=""
start_pg_image() { # extra docker-run args...
    docker run --detach --name "${RUN_ID}" \
        --env POSTGRES_USER=app \
        --env POSTGRES_DB=app \
        --env POSTGRES_PASSWORD_FILE=/run/secrets/pw \
        --volume "${image_password_file}:/run/secrets/pw:ro" \
        --volume "${VOLUME}:${DATA_MOUNT}" \
        "$@" "${IMAGE}" >/dev/null
}

wait_pg_healthy() {
    wait_for "the PostgreSQL container reports healthy" 300 container_healthy "${RUN_ID}"
}

setup_image_common_bits() {
    # The harness's runtime inside the container under test: python3 for the
    # mock and the loopback HTTP calls. A test-time addition to a disposable
    # container, never to the image.
    docker exec -u root "$1" bash -c \
        'apt-get update -qq >/dev/null && apt-get install -y -qq python3-minimal >/dev/null' \
        || die "cannot install python3 in $1"
    docker cp "${TESTS_DIR}/provider-mock.py" "$1:/provider-mock.py"
}

setup_complete_image() {
    [[ -n "${IMAGE}" ]] || die "pass the complete image reference"
    image_password_file="$(mktemp)"; printf 'pv13-%s' "$$" > "${image_password_file}"
    local pg_major
    pg_major="$(docker run --rm --entrypoint sh "${IMAGE}" -c 'echo "$PG_MAJOR"' 2>/dev/null || echo 18)"
    if (( pg_major >= 18 )); then DATA_MOUNT=/var/lib/postgresql; else DATA_MOUNT=/var/lib/postgresql/data; fi
    log "PV-13 complete-image cell: ${IMAGE}"
    start_pg_image
    DB="${RUN_ID}"; SRV="${RUN_ID}"; EMBEDDED=1
    wait_pg_healthy || die "the image never became healthy"
    setup_image_common_bits "${DB}"
    ok "the complete image serves an embedded cluster"
}

setup_remote_image() {
    [[ -n "${IMAGE}" ]] || die "pass the remote image reference"
    [[ -n "${SERVER_IMAGE}" ]] || die "pass --server-image (a postvec-server image)"
    image_password_file="$(mktemp)"; printf 'pv13-%s' "$$" > "${image_password_file}"
    local pg_major
    pg_major="$(docker run --rm --entrypoint sh "${IMAGE}" -c 'echo "$PG_MAJOR"' 2>/dev/null || echo 18)"
    if (( pg_major >= 18 )); then DATA_MOUNT=/var/lib/postgresql; else DATA_MOUNT=/var/lib/postgresql/data; fi

    log "PV-13 remote cell: ${IMAGE} + ${SERVER_IMAGE}"
    docker network create "${NETWORK}" >/dev/null
    docker run --detach --name "${SRV_ID}" --network "${NETWORK}" \
        --network-alias postvec-server "${SERVER_IMAGE}" >/dev/null
    wait_for "postvec-server reports healthy (/ready)" 300 container_healthy "${SRV_ID}" \
        || die "postvec-server never became ready"

    start_pg_image --network "${NETWORK}" \
        --env "POSTVEC_GRPC_ENDPOINTS=postvec-server:33333" \
        --env "POSTVEC_HTTP_ENDPOINTS=https://postvec-server:22222"
    DB="${RUN_ID}"; SRV="${SRV_ID}"; EMBEDDED=0
    wait_pg_healthy || die "the remote image never became healthy"
    setup_image_common_bits "${SRV}"

    # The shipped CLI administers the node's providers.d on the node itself.
    # Prefer the build tree's binary; in CI's image jobs only the packages
    # are present, so fall back to extracting the CLI from the deb — which is
    # also the more honest artifact to exercise.
    local cli="${PKG_DIR}/build/debian12-pg${PG_MAJOR}-${RELEASE_ARCH}/cli/postvec"
    [[ -x "${cli}" ]] || cli="${PKG_DIR}/build/debian12-pg18-${RELEASE_ARCH}/cli/postvec"
    if [[ ! -x "${cli}" ]]; then
        local deb
        deb="$(find "$(common_dist_dir debian12 "${RELEASE_ARCH}")" \
                    -name 'postvec-cli_*.deb' 2>/dev/null | head -1)"
        [[ -n "${deb}" ]] || die "no built postvec CLI and no postvec-cli deb; build debian12 first"
        need dpkg-deb
        STAGE="${STAGE:-$(mktemp -d)}"
        cli="${STAGE}/postvec"
        dpkg-deb --fsys-tarfile "${deb}" | tar -xO ./usr/bin/postvec > "${cli}"
        chmod +x "${cli}"
    fi
    docker cp "${cli}" "${SRV_ID}:/usr/local/bin/postvec"
    ok "the remote pair serves; the shipped CLI is on the node"
}

# --------------------------------------------------------------- provider ops

provider_keys_dir() { if (( EMBEDDED )); then echo /etc/postvec/keys; else echo /opt/postvec/keys; fi; }
provider_key_path() { echo "$(provider_keys_dir)/e2e.key"; }
provider_file_owner() {
    if [[ "${TARGET}" == package ]]; then echo postgres
    elif (( EMBEDDED )); then echo postgres
    else echo postvec-server; fi
}

write_key() { # <value> — atomic replace, owner + 0600 preserved
    local owner root; owner="$(provider_file_owner)"
    if (( EMBEDDED )); then root=/etc/postvec; else root=/opt/postvec; fi
    docker exec -u root -e KEYVAL="$1" "${SRV}" bash -c "
        set -e
        mkdir -p ${root} $(provider_keys_dir)
        chown ${owner}:${owner} ${root} $(provider_keys_dir)
        # Docker exec inherits the daemon umask. Snap-packaged Docker can use
        # 000, so mkdir -p would leave ${root} world-writable and provider add
        # would correctly refuse it. Parent 0755, keys 0700, regardless of umask
        # (same reason provider-e2e-install.sh pins CONF_D).
        chmod 0755 ${root}
        chmod 0700 $(provider_keys_dir)
        printf '%s' \"\${KEYVAL}\" > $(provider_key_path).new
        chmod 600 $(provider_key_path).new
        chown ${owner}:${owner} $(provider_key_path).new
        mv $(provider_key_path).new $(provider_key_path)"
}

provider_add() { # runs the shipped CLI, WITH the live verification probe
    if [[ "${TARGET}" == package ]]; then
        docker exec "${DB}" postvec provider add openai \
            --pg-config "${BIN}/pg_config" --config-dir /etc/postvec-test/conf.d \
            --model text-embedding-3-small \
            --api-key-file "$(provider_key_path)" \
            --base-url "http://127.0.0.1:${MOCK_PORT}" \
            --yes
    elif (( EMBEDDED )); then
        docker exec "${DB}" postvec provider add openai \
            --path /etc/postvec \
            --model text-embedding-3-small \
            --api-key-file "$(provider_key_path)" \
            --base-url "http://127.0.0.1:${MOCK_PORT}" \
            --acknowledge-in-use --yes
    else
        docker exec "${SRV}" postvec provider add openai \
            --path /opt/postvec \
            --model text-embedding-3-small \
            --api-key-file "$(provider_key_path)" \
            --base-url "http://127.0.0.1:${MOCK_PORT}" \
            --acknowledge-in-use --yes
    fi
}

# ----------------------------------------------------------------- scenarios

apply_speed_knobs() {
    step "speed knobs (all SIGHUP; values recorded here by design)"
    dbsql "ALTER SYSTEM SET postvec.max_retries = 2;
           ALTER SYSTEM SET postvec.retry_backoff_ms = 500;
           ALTER SYSTEM SET postvec.job_visibility_timeout_ms = 5000;
           ALTER SYSTEM SET postvec.poll_interval_ms = 1000;
           ALTER SYSTEM SET postvec.embed_timeout_ms = 20000;
           SELECT pg_reload_conf();" >/dev/null
    local got
    got="$(dbsql "SELECT current_setting('postvec.max_retries')")"
    [[ "${got}" == 2 ]] && ok "max_retries=2 retry_backoff_ms=500 visibility=5000ms poll=1000ms embed_timeout=20000ms" \
                        || bad "speed knobs did not apply (max_retries=${got})"
}

scenario_A_live_verification() {
    step "A. live verification (the probe hits the mock; no --no-verify)"
    write_key "${KEYA}"
    mockctl "{\"reset\": true, \"require_bearer\": \"${KEYA}\"}"
    local out status=0
    out="$(provider_add 2>&1)" || status=$?
    if (( status == 0 )); then
        ok "provider add verified against the mock"
    elif (( status == 3 )) && grep -q "models serve now" <<<"${out}"; then
        # The designed partial on a host whose ingress was sized before any
        # provider existed: the model serves, full throughput wants a restart.
        ok "provider add verified; partial only for startup ingress sizing (models serve now)"
    else
        bad "provider add exited ${status}: $(tail -3 <<<"${out}")"
        return
    fi
    grep -q "dim 1536 (verified)" <<<"${out}" \
        && ok "the probe verified the declared 1536 dimensions" \
        || bad "no verified-dimension line in: $(tail -3 <<<"${out}")"
    requests_are 1 \
        && ok "exactly one verification request was spent" \
        || bad "expected 1 probe request, mock saw $(mock_state requests)"
    [[ "$(mock_state auth_failures)" == 0 ]] \
        && ok "the probe authenticated with the key file's key" \
        || bad "$(mock_state auth_failures) request(s) carried the wrong credential"
    dbsql "SELECT postvec.refresh_models()" >/dev/null 2>&1 || true
    wait_for "the provider model reaches the model cache" 30 model_cached \
        && ok "${PROV_MODEL} is served and cached" \
        || bad "${PROV_MODEL} never reached postvec.models"
}

scenario_B_success_and_search() {
    step "B. success, write-back, and hybrid search"
    mockctl '{"reset": true}'
    dbsql "CREATE TABLE e2e (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, body text NOT NULL);
           SELECT postvec.enable('e2e','body', model => '${PROV_MODEL}', create_fts_index => true);
           INSERT INTO e2e(body) VALUES
             ('quarterly revenue guidance increased'),
             ('the office plants need water'),
             ('hosted vector conversion avoids re embedding source text');" >/dev/null
    ok "enable() on the provider model and three inserts"
    wait_for "all three vectors" 60 filled_is 3 \
        && ok "the worker filled all three vectors through the provider" \
        || bad "only $(filled_count)/3 vectors filled"
    dead_is 0 && ok "no dead letters" || bad "$(dead_count) dead letter(s) on the happy path"
    local top expected
    top="$(dbsql "SELECT s.pk_value FROM postvec.search('e2e','body',
                    'hosted conversion of stored vector spaces') s
                  ORDER BY s.rrf_score DESC LIMIT 1")"
    expected="$(dbsql "SELECT id FROM e2e WHERE body LIKE 'hosted%'")"
    [[ "${top}" == "${expected}" && -n "${top}" ]] \
        && ok "search ranks the matching row first (deterministic mock vectors)" \
        || bad "search returned row '${top}', expected '${expected}'"
}

RSS_BASELINE_KB=0
rss_baseline() {
    RSS_BASELINE_KB="$(vm_hwm_kb)" || { bad "cannot read the serving process's VmHWM"; return; }
    ok "RSS baseline: serving process VmHWM ${RSS_BASELINE_KB} kB"
}

scenario_C_bounded_retry() {
    step "C. bounded upstream retry (429, then 500, then success)"
    mockctl '{"reset": true, "statuses": [429, 500]}'
    dbsql "INSERT INTO e2e(body) VALUES ('bounded retry probe row')" >/dev/null
    wait_for "the retried row's vector" 60 filled_is 4 \
        && ok "the row was embedded after the transient failures" \
        || bad "the row never filled ($(filled_count)/4)"
    # The in-client retry (providers/src/retry.rs, MAX_RETRIES=2) absorbs the
    # 429 and the 500 inside ONE worker attempt: exactly three upstream
    # requests, and none after — a quiet period proves the count is final.
    sleep 3
    requests_are 3 \
        && ok "exactly three upstream requests (429, 500, success), none after" \
        || bad "expected exactly 3 upstream requests, mock saw $(mock_state requests)"
    dead_is 0 && ok "no dead letter for a recovered transient" \
              || bad "$(dead_count) dead letter(s)"
}

scenario_D_oversized() {
    step "D. oversized response (no honest Content-Length)"
    mockctl '{"reset": true, "mode": "oversized"}'
    dbsql "INSERT INTO e2e(body) VALUES ('oversized response probe row')" >/dev/null
    wait_for "the oversized row to dead-letter" 60 dead_is 1 \
        && ok "the row dead-lettered instead of retrying forever" \
        || bad "no dead letter within 60s (dead=$(dead_count), requests=$(mock_state requests))"
    local err
    err="$(dbsql "SELECT last_error FROM postvec.jobs_dead ORDER BY dead_id DESC LIMIT 1" 2>/dev/null || true)"
    grep -qi "budget" <<<"${err}" \
        && ok "the error names the response-size budget" \
        || bad "dead-letter error does not name the budget: ${err:0:120}"
    requests_are 1 \
        && ok "a deterministic bad response was requested exactly once" \
        || bad "expected 1 request, mock saw $(mock_state requests)"
    db_alive && ok "PostgreSQL remains responsive" || bad "SELECT 1 failed after the oversized body"
    local hwm
    hwm="$(vm_hwm_kb)" || { bad "cannot read VmHWM"; return; }
    ok "RSS evidence: VmHWM ${hwm} kB (baseline ${RSS_BASELINE_KB} kB)"
    mockctl '{"mode": "success"}'
}

scenario_E_trickle() {
    step "E. slow reader (a response that never finishes)"
    # 8 s worker deadline for THIS scenario only: each worker attempt then
    # aborts its single in-flight request at the deadline, so the three
    # queue attempts (max_retries=2) make exactly three upstream requests.
    dbsql "ALTER SYSTEM SET postvec.embed_timeout_ms = 8000; SELECT pg_reload_conf();" >/dev/null
    mockctl '{"reset": true, "mode": "trickle"}'
    dbsql "INSERT INTO e2e(body) VALUES ('slow reader probe row')" >/dev/null
    # The caller-side deadline is 8 s, but a cancelled serving-side HTTP task
    # may retain its connector permit until the provider client's own 20 s
    # timeout. Allow three queue attempts plus generous scheduling headroom on
    # a release host that is simultaneously running Docker package tests.
    wait_for "the trickle row to dead-letter" 180 dead_is 2 \
        && ok "retries stopped at the configured limit and the row dead-lettered" \
        || bad "no dead letter within 180s (dead=$(dead_count), queue=$(queue_count), requests=$(mock_state requests))"
    local reqs
    reqs="$(mock_state requests)"
    (( reqs >= 1 && reqs <= 6 )) \
        && ok "bounded attempts: ${reqs} upstream request(s) for three worker tries" \
        || bad "unbounded retries: ${reqs} upstream requests"
    db_alive && ok "PostgreSQL remains responsive" || bad "SELECT 1 failed during the trickle"
    local hwm delta
    hwm="$(vm_hwm_kb)" || { bad "cannot read VmHWM"; return; }
    delta=$(( hwm - RSS_BASELINE_KB ))
    # Ceiling rationale: a one-row request's computed response budget is
    # ~256 KiB and the absolute reader ceiling is 64 MiB; 262144 kB (256 MiB)
    # of VmHWM growth across D+E means the whole aggregate provider budget
    # leaked — far above any legitimate observation (single-digit MiB in
    # amd64 runs), far below a machine-threatening allocation.
    (( delta < 262144 )) \
        && ok "RSS bounded: VmHWM grew ${delta} kB since baseline (< 262144 kB)" \
        || bad "VmHWM grew ${delta} kB since baseline — the reader is not bounded"
    mockctl '{"mode": "success"}'
    dbsql "ALTER SYSTEM SET postvec.embed_timeout_ms = 20000; SELECT pg_reload_conf();" >/dev/null
}

scenario_F_key_rotation() {
    step "F. connector reload and key rotation (A -> B, no restart)"
    mockctl "{\"reset\": true, \"require_bearer\": \"${KEYA}\"}"
    dbsql "INSERT INTO e2e(body) VALUES ('rotation row under key alpha')" >/dev/null
    wait_for "a row under key A" 90 filled_is 5 \
        && ok "a row embeds under key A" \
        || bad "the key-A row never filled (filled=$(filled_count) queue=$(queue_count) dead=$(dead_count) requests=$(mock_state requests))"
    write_key "${KEYB}"
    mockctl "{\"require_bearer\": \"${KEYB}\", \"reset\": true}"
    local reload
    reload="$(reload_providers)" || { bad "provider reload failed"; return; }
    grep -q '"success": *true' <<<"${reload}" \
        && ok "the host reloaded the connector (same file, rotated key file)" \
        || bad "reload answered: ${reload:0:120}"
    dbsql "INSERT INTO e2e(body) VALUES ('rotation row under key bravo')" >/dev/null
    wait_for "a row under key B" 60 filled_is 6 \
        && ok "a row embeds under key B — the rotation took without a restart" \
        || bad "the key-B row never filled (filled=$(filled_count) queue=$(queue_count) requests=$(mock_state requests) auth_failures=$(mock_state auth_failures))"
    [[ "$(mock_state auth_failures)" == 0 ]] \
        && ok "no request carried the stale credential" \
        || bad "$(mock_state auth_failures) request(s) still used the old key"
}

scenario_G_crash_recovery() {
    step "G. in-flight crash of the serving process"
    mockctl '{"reset": true, "mode": "hold"}'
    dbsql "INSERT INTO e2e(body) VALUES ('crash recovery probe row')" >/dev/null
    wait_for "a provider request to be genuinely in flight" 30 in_flight_nonzero \
        && ok "a provider request is held in flight" \
        || { bad "no request reached the mock before the crash"; return; }

    if (( EMBEDDED )); then
        local pid
        pid="$(serving_pid)" || { bad "no serving pid"; return; }
        docker exec -u root "${SRV}" bash -c "kill -9 ${pid}"
        ok "killed the embedded launcher (pid ${pid}) mid-request"
        # A bgworker crash takes PostgreSQL through crash recovery — expected
        # and documented for embedded mode. The database must come BACK.
        mockctl '{"release": true, "mode": "success"}'
        wait_for "PostgreSQL to finish crash recovery" 120 db_alive \
            && ok "PostgreSQL recovered" \
            || { bad "PostgreSQL did not come back"; return; }
    else
        docker kill "${SRV}" >/dev/null
        ok "killed postvec-server mid-request"
        db_alive && ok "PostgreSQL keeps answering while the engine is down" \
                 || bad "PostgreSQL failed while only the engine was killed"
        docker start "${SRV}" >/dev/null
        wait_for "postvec-server to be ready again" 300 container_healthy "${SRV}" \
            && ok "postvec-server restarted and is ready" \
            || { bad "postvec-server never came back"; return; }
        # The mock died with the container; bring it back with the same
        # requirements before the worker's next attempt can succeed.
        start_mock || { bad "the mock did not restart"; return; }
        mockctl "{\"mode\": \"success\", \"require_bearer\": \"${KEYB}\"}"
    fi

    wait_for "the in-flight row to drain after recovery" 120 filled_is 7 \
        && ok "the queued row filled after recovery (claim reclaimed, no loss)" \
        || bad "the crash row never filled ($(filled_count)/7, dead=$(dead_count))"
    wait_for "the worker heartbeat to resume" 120 hb_fresh \
        && ok "the worker is beating again" \
        || bad "no fresh heartbeat after recovery"
    [[ "$(dbsql "SELECT count(*) FROM e2e WHERE body_semantic IS NOT NULL")" == 7 ]] \
        && [[ "$(queue_count)" == 0 ]] \
        && ok "no wedged claim and no duplicate write-back" \
        || bad "queue=$(queue_count) after recovery"
}

final_asserts() {
    step "final state"
    dead_is 2 \
        && ok "exactly the two designed dead letters (oversized, trickle)" \
        || bad "expected 2 dead letters, found $(dead_count)"
    [[ "$(queue_count)" == 0 ]] \
        && ok "the queue is drained" \
        || bad "$(queue_count) queued job(s) remain"
    # The credentials must be nowhere an operator can stumble on them.
    local logs
    logs="$(docker logs "${DB}" 2>&1 || true; docker logs "${SRV}" 2>&1 || true;
            docker exec "${DB}" cat /tmp/pg.log 2>/dev/null || true)"
    if grep -qF -e "${KEYA}" -e "${KEYB}" <<<"${logs}"; then
        bad "a credential value leaked into a log"
    else
        ok "neither key value appears in any log"
    fi
}

# ------------------------------------------------------------------- driver

case "${TARGET}" in
package)
    setup_package
    ;;
image)
    case "${VARIANT}" in
    complete) setup_complete_image ;;
    remote)   setup_remote_image ;;
    *) die "--variant must be complete or remote" ;;
    esac
    ;;
*) die "--target must be package or image" ;;
esac

start_mock || die "the provider mock never became ready"
apply_speed_knobs
scenario_A_live_verification
scenario_B_success_and_search
rss_baseline
scenario_C_bounded_retry
scenario_D_oversized
scenario_E_trickle
scenario_F_key_rotation
scenario_G_crash_recovery
final_asserts

printf '\n%d passed, %d failed\n' "${passed}" "${failed}"
(( failed == 0 ))
