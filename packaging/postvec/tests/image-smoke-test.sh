#!/usr/bin/env bash
# Live test of a built postvec image: the user journey, the container contract,
# and the failure modes that must fail *fast* rather than quietly.
#
#   tests/image-smoke-test.sh ghcr.io/univec-ai/postvec:0.1.0-1-pg18-complete
#   tests/image-smoke-test.sh --variant remote \
#       --server-image postvec-server:amd64 \
#       ghcr.io/univec-ai/postvec:0.1.0-1-pg18
#
# Everything runs against the exact image reference given — pass a digest in a
# release job, so what is tested is bit-for-bit what will be published.

set -Eeuo pipefail

# The bundled model's name and dimension are the two facts this test cannot
# invent: they are what the image was built around, and repeating them as
# literals here is how a bundled-model change passes a green smoke test against
# the wrong model, or fails on a correct one whose vectors are a different width.
#
# The name comes from the reviewed pin in versions.env. The dimension is
# *derived* from the verified registry archive, so it comes from the facts the
# bundle step wrote — the same file the packages were built from. `--model` and
# `--dims` still override both, which is what keeps the test runnable against an
# image somebody hands you from a checkout that need not match it.
TESTS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PKG_DIR="$(cd "${TESTS_DIR}/.." && pwd)"
MODEL="$(sed -n 's/^BUNDLED_MODEL_NAME=//p' "${PKG_DIR}/versions.env")"
[[ -n "${MODEL}" ]] || { echo "no BUNDLED_MODEL_NAME in ${PKG_DIR}/versions.env" >&2; exit 2; }

MODEL_FACTS="${PKG_DIR}/build/payload-common/model-facts.env"
DIMS=""
if [[ -f "${MODEL_FACTS}" ]]; then
    DIMS="$(sed -n 's/^MODEL_TARGET_DIM=//p' "${MODEL_FACTS}")"
fi

VARIANT=complete
HEALTH_TIMEOUT=300
IMAGE=""
REMOTE_GRPC=""
REMOTE_HTTP=""
SERVER_IMAGE=""
while (($#)); do
    case "$1" in
    --variant) VARIANT="$2"; shift 2 ;;
    --model)   MODEL="$2"; shift 2 ;;
    --dims)    DIMS="$2"; shift 2 ;;
    --timeout) HEALTH_TIMEOUT="$2"; shift 2 ;;
    --grpc)    REMOTE_GRPC="$2"; shift 2 ;;
    --http)    REMOTE_HTTP="$2"; shift 2 ;;
    # A locally built postvec-server image; the test starts it itself.
    --server-image) SERVER_IMAGE="$2"; shift 2 ;;
    -h|--help) sed -n '2,10p' "$0"; exit 0 ;;
    *)         IMAGE="$1"; shift ;;
    esac
done
[[ -n "${IMAGE}" ]] || { echo "usage: image-smoke-test.sh [--variant remote|complete] <image>" >&2; exit 2; }

# No default, and no guess. A wrong dimension either fails a correct image or —
# worse, if it were merely skipped — passes a broken one.
[[ -n "${DIMS}" ]] || { cat >&2 <<EOF
the bundled model's dimension is unknown.

It is derived from the verified registry archive, so it is read from
  build/payload-common/model-facts.env
which this checkout does not have. Either build the model bundle:
  scripts/build-model-bundle.sh --cli <postvec binary>
or state it, when testing an image this checkout did not produce:
  tests/image-smoke-test.sh --model <name> --dims <n> ${IMAGE}
EOF
exit 2; }
[[ "${DIMS}" =~ ^[1-9][0-9]*$ ]] || { echo "--dims must be a positive integer (got: ${DIMS})" >&2; exit 2; }

RUN_ID="postvec-smoke-$$"
VOLUME="${RUN_ID}-data"
NETWORK="${RUN_ID}-net"
SERVER="${RUN_ID}-server"
PASSWORD_FILE="$(mktemp)"
printf 'smoke-%s' "$$" > "${PASSWORD_FILE}"

passed=0 failed=0
ok()  { printf '  \033[32mok\033[0m    %s\n' "$*"; passed=$((passed + 1)); }
bad() { printf '  \033[1;31mFAIL\033[0m  %s\n' "$*" >&2; failed=$((failed + 1)); }

cleanup() {
    docker rm --force "${RUN_ID}" "${SERVER}" >/dev/null 2>&1 || true
    docker volume rm "${VOLUME}" >/dev/null 2>&1 || true
    docker network rm "${NETWORK}" >/dev/null 2>&1 || true
    rm -f "${PASSWORD_FILE}"
}
trap cleanup EXIT

# PG 18 changed the official image's storage layout; mounting the wrong path is
# a silent data-loss bug, so the test uses the same path the docs tell users to.
pg_major="$(docker run --rm --entrypoint sh "${IMAGE}" -c 'echo "$PG_MAJOR"' 2>/dev/null || echo 18)"
if (( pg_major >= 18 )); then DATA_MOUNT=/var/lib/postgresql; else DATA_MOUNT=/var/lib/postgresql/data; fi

start_container() {
    docker run --detach --name "${RUN_ID}" \
        --env POSTGRES_USER=app \
        --env POSTGRES_DB=app \
        --env POSTGRES_PASSWORD_FILE=/run/secrets/pw \
        --volume "${PASSWORD_FILE}:/run/secrets/pw:ro" \
        --volume "${VOLUME}:${DATA_MOUNT}" \
        --publish 127.0.0.1:0:5432 \
        ${NETWORK_ARGS} \
        "$@" "${IMAGE}" >/dev/null
}
NETWORK_ARGS=""

wait_healthy() {
    local deadline=$(( SECONDS + HEALTH_TIMEOUT )) status
    while (( SECONDS < deadline )); do
        status="$(docker inspect --format '{{.State.Health.Status}}' "${RUN_ID}" 2>/dev/null || echo gone)"
        case "${status}" in
        healthy)   return 0 ;;
        unhealthy) docker logs --tail 40 "${RUN_ID}" >&2; return 1 ;;
        gone)      docker logs --tail 40 "${RUN_ID}" >&2; return 1 ;;
        esac
        sleep 2
    done
    docker logs --tail 40 "${RUN_ID}" >&2
    return 1
}

sql() { docker exec --interactive "${RUN_ID}" psql -U app -d app -tAX -v ON_ERROR_STOP=1 "$@"; }

# The worker's first /config poll is jittered 0–15 s so a cluster start does
# not fire every database at once. Remote health does not wait for the cache
# (the engine may come up later), so callers that need a model must poll.
wait_for_model() {
    local deadline=$(( SECONDS + 45 )) n=""
    while (( SECONDS < deadline )); do
        n="$(sql -c "SELECT count(*) FROM postvec.models WHERE name = '${MODEL}'" 2>/dev/null || true)"
        if [[ "${n}" == 1 ]]; then
            printf '%s' "${n}"
            return 0
        fi
        sleep 2
    done
    printf '%s' "${n:-0}"
    return 1
}

echo "image: ${IMAGE} (${VARIANT}, PG ${pg_major}, data at ${DATA_MOUNT})"
echo
echo "boot"

start_container
if wait_healthy; then ok "becomes healthy"; else bad "never became healthy"; fi

# One process tree, PostgreSQL as PID 1 — no supervisor, no second service.
init="$(docker exec "${RUN_ID}" cat /proc/1/comm 2>/dev/null || echo unknown)"
[[ "${init}" == postgres ]] && ok "PostgreSQL is PID 1" || bad "PID 1 is '${init}', not postgres"

# The inference listeners must never be reachable from outside the container:
# they have neither authentication nor TLS.
published="$(docker port "${RUN_ID}" | sort)"
if [[ "$(wc -l <<<"${published}")" -le 2 && "${published}" == *5432* && "${published}" != *3343* ]]; then
    ok "only PostgreSQL is published (${published//$'\n'/, })"
else
    bad "unexpected published ports: ${published//$'\n'/, }"
fi

echo
echo "extension"

version="$(sql -c "SELECT postvec.version()")"
[[ -n "${version}" ]] && ok "postvec ${version} is installed" || bad "no extension"

catalog="$(sql -c "SELECT extversion FROM pg_extension WHERE extname='postvec'")"
[[ "${catalog}" == "${version}" ]] \
    && ok "catalog and library versions agree" \
    || bad "catalog ${catalog} vs library ${version} — the worker will park"

embedded_capable="$(sql -c "SELECT (postvec.build_info()->'features'->>'embedded')::bool")"
[[ "${embedded_capable}" == t ]] \
    && ok "the library reports embedded capability" \
    || bad "the published library must be the embedded-capable build"

# pgvector is a hard dependency; CASCADE should have pulled it in.
vector="$(sql -c "SELECT extversion FROM pg_extension WHERE extname='vector'")"
[[ -n "${vector}" ]] && ok "pgvector ${vector} is installed" || bad "no pgvector"

if [[ "${VARIANT}" == complete ]]; then
    echo
    echo "inference (no external service, no API key)"

    # External providers are opt-in, and the image ships none. Both halves of
    # that are asserted here, because "zero-config still works" is exactly the
    # claim a provider feature is able to break silently: an image that shipped
    # a providers.d, or a host that advertised a provider-backed model without
    # one, would still pass every check below it.
    docker exec "${RUN_ID}" test -e /etc/postvec/providers.d \
        && bad "the image ships /etc/postvec/providers.d — providers must be opt-in" \
        || ok "no provider configuration in the image"

    external="$(sql -c "SELECT count(*) FROM postvec.models
                         WHERE raw->'extra'->>'provider' IS NOT NULL")"
    [[ "${external}" == 0 ]] \
        && ok "the model cache advertises no provider-backed model" \
        || bad "${external} provider-backed model(s) in an image configured with none"

    dim="$(sql -c "SELECT target_dim FROM postvec.models WHERE name = '${MODEL}'")"
    [[ "${dim}" == "${DIMS}" ]] \
        && ok "${MODEL} is loaded at ${dim} dimensions" \
        || bad "model cache says '${dim}', expected ${DIMS}"

    actual="$(sql -c "SELECT vector_dims(postvec.embed('Postgres can run this embedding locally','${MODEL}')::vector)")"
    [[ "${actual}" == "${DIMS}" ]] && ok "embed() returns ${DIMS} values" || bad "embed() returned ${actual}"

    # Finite and unit-normalised: the descriptor asks for normalisation, and a
    # silently unnormalised vector breaks cosine distance without erroring.
    norm="$(sql -c "SELECT round(sqrt(sum(v::float8*v::float8))::numeric,3)
                      FROM unnest(postvec.embed('normalisation check','${MODEL}')) v")"
    [[ "${norm}" == "1.000" ]] && ok "vectors are unit-normalised (L2 = ${norm})" \
                                || bad "L2 norm is ${norm}, expected 1.000"

    nonfinite="$(sql -c "SELECT count(*) FROM unnest(postvec.embed('finite check','${MODEL}')) v
                          WHERE v <> v OR v = 'Infinity'::real OR v = '-Infinity'::real")"
    [[ "${nonfinite}" == 0 ]] && ok "no NaN or infinite components" || bad "${nonfinite} non-finite components"

    echo
    echo "the whole point: write, sync, search"

    sql <<SQL >/dev/null
CREATE TABLE smoke (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, body text NOT NULL);
SELECT postvec.enable('smoke','body', model => '${MODEL}', create_fts_index => true);
INSERT INTO smoke(body) VALUES
  ('quarterly revenue guidance increased'),
  ('the office plants need water'),
  ('migrating embedding models normally requires re-embedding all source text');
SQL
    ok "enable() and three inserts"

    # The worker fills the shadow column asynchronously; poll with a deadline
    # rather than sleeping for a guess.
    deadline=$(( SECONDS + 60 ))
    filled=0
    while (( SECONDS < deadline )); do
        filled="$(sql -c "SELECT count(*) FROM smoke WHERE body_semantic IS NOT NULL")"
        [[ "${filled}" == 3 ]] && break
        sleep 2
    done
    [[ "${filled}" == 3 ]] && ok "the worker filled all three vectors" \
                            || bad "only ${filled}/3 vectors filled within 60s"

    # The row that should win shares no keyword with the query — if it ranks
    # first, the semantic leg is genuinely working.
    top="$(sql -c "SELECT s.pk_value FROM postvec.search('smoke','body',
                     'switching AI models without redoing the work') s
                   ORDER BY s.rrf_score DESC LIMIT 1")"
    expected="$(sql -c "SELECT id FROM smoke WHERE body LIKE 'migrating%'")"
    [[ "${top}" == "${expected}" ]] \
        && ok "hybrid search ranks the semantically matching row first" \
        || bad "search returned row ${top}, expected ${expected}"
fi

if [[ "${VARIANT}" == remote ]]; then
    echo
    echo "remote mode"

    # Without a reachable engine the worker must *degrade*, not fail: it keeps
    # beating, queues work without burning retry attempts, and search falls
    # back to full text. That is the documented behaviour and it is what makes
    # a temporarily unreachable inference host survivable.
    sql <<'SQL' >/dev/null
CREATE TABLE IF NOT EXISTS smoke (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, body text NOT NULL);
SQL
    ok "a table can be created while the engine is unreachable"

    # Heartbeats are change-gated: an idle worker writes once per
    # postvec.heartbeat_interval_ms (default 30 s), not once per poll tick.
    # A four-second double sample only observes that deliberate quiet and
    # false-alarms. Freshness — the same budget doctor uses — is the proof
    # of life.
    beat_ok="$(sql <<'SQL'
SELECT (s.worker_last_beat IS NOT NULL
        AND s.worker_last_beat::timestamptz > clock_timestamp()
            - (COALESCE(NULLIF(current_setting('postvec.heartbeat_interval_ms', true), ''), '30000')::int
               + 3 * COALESCE(NULLIF(current_setting('postvec.poll_interval_ms', true), ''), '5000')::int
               + 2000) * interval '1 millisecond')::text
  FROM postvec.stats() s
SQL
)"
    # ::text, not psql's default boolean, so this is 'true'/'false' — the
    # same spelling the healthcheck compares against. 't' is what you get
    # from an uncast boolean and is how this check false-alarmed.
    [[ "${beat_ok}" == true ]] \
        && ok "the worker is beating with no engine configured" \
        || bad "the worker is not beating (freshness check: ${beat_ok:-empty})"

    dead="$(sql -c "SELECT queue_dead FROM postvec.stats()")"
    [[ "${dead}" == 0 ]] \
        && ok "no job was dead-lettered for an unreachable engine" \
        || bad "${dead} dead-lettered job(s) — an unreachable engine must not burn attempts"

    # Degradation is half the contract; the other half is that it works when
    # an engine *is* reachable. That needs a real inference node — either one
    # the caller names, or a postvec-server image, which the test brings up
    # itself on a private network.
    if [[ -n "${SERVER_IMAGE}" ]]; then
        echo
        echo "remote inference against a live postvec-server"

        docker network create "${NETWORK}" >/dev/null
        docker run --detach --name "${SERVER}" --network "${NETWORK}" \
            --network-alias postvec-server "${SERVER_IMAGE}" >/dev/null

        # postvec-server's healthcheck is /ready, which is 503 until a model
        # can actually answer — so "healthy" here means it can serve, not
        # merely that it is listening.
        deadline=$(( SECONDS + 300 ))
        until [[ "$(docker inspect --format '{{.State.Health.Status}}' "${SERVER}" 2>/dev/null)" == healthy ]]; do
            if (( SECONDS >= deadline )); then
                docker logs --tail 30 "${SERVER}" >&2
                bad "postvec-server never became ready"
                break
            fi
            sleep 3
        done

        docker rm --force "${RUN_ID}" >/dev/null
        docker volume rm "${VOLUME}" >/dev/null 2>&1 || true
        NETWORK_ARGS="--network ${NETWORK}"
        start_container \
            --env "POSTVEC_GRPC_ENDPOINTS=postvec-server:33333" \
            --env "POSTVEC_HTTP_ENDPOINTS=https://postvec-server:22222"
        if wait_healthy; then
            ok "healthy against a live engine"
        else
            bad "unhealthy against postvec-server"
        fi

        if models="$(wait_for_model)"; then
            ok "the remote model catalogue was discovered over /config"
        else
            bad "the model cache does not contain ${MODEL} (${models})"
        fi

        dims="$(sql -c "SELECT vector_dims(postvec.embed('remote smoke test','${MODEL}')::vector)" || true)"
        [[ "${dims}" == "${DIMS}" ]] \
            && ok "remote embed() returns ${DIMS} values" \
            || bad "remote embed() returned '${dims}'"

        # The whole remote path, not just one call: trigger, queue, worker,
        # gRPC round trip, write-back, and search.
        sql <<SQL >/dev/null
CREATE TABLE remote_smoke (id bigint GENERATED ALWAYS AS IDENTITY PRIMARY KEY, body text NOT NULL);
SELECT postvec.enable('remote_smoke','body', model => '${MODEL}', create_fts_index => true);
INSERT INTO remote_smoke(body) VALUES
  ('quarterly revenue guidance increased'),
  ('the office plants need water'),
  ('migrating embedding models normally requires re-embedding all source text');
SQL
        deadline=$(( SECONDS + 120 ))
        filled=0
        while (( SECONDS < deadline )); do
            filled="$(sql -c "SELECT count(*) FROM remote_smoke WHERE body_semantic IS NOT NULL")"
            [[ "${filled}" == 3 ]] && break
            sleep 2
        done
        [[ "${filled}" == 3 ]] \
            && ok "the worker filled all three vectors over gRPC" \
            || bad "only ${filled}/3 vectors filled against a live engine"

        top="$(sql -c "SELECT s.pk_value FROM postvec.search('remote_smoke','body',
                        'switching AI models without redoing the work') s
                      ORDER BY s.rrf_score DESC LIMIT 1")"
        expected="$(sql -c "SELECT id FROM remote_smoke WHERE body LIKE 'migrating%'")"
        [[ "${top}" == "${expected}" ]] \
            && ok "hybrid search over remotely computed vectors ranks correctly" \
            || bad "remote search returned row ${top}, expected ${expected}"

        NETWORK_ARGS=""
    elif [[ -n "${REMOTE_GRPC}" ]]; then
        echo
        echo "remote inference against ${REMOTE_GRPC}"
        docker rm --force "${RUN_ID}" >/dev/null
        docker volume rm "${VOLUME}" >/dev/null 2>&1 || true
        start_container \
            --env "POSTVEC_GRPC_ENDPOINTS=${REMOTE_GRPC}" \
            --env "POSTVEC_HTTP_ENDPOINTS=${REMOTE_HTTP}"
        if wait_healthy; then ok "healthy against a real engine"; else bad "unhealthy against ${REMOTE_GRPC}"; fi
        if ! models="$(wait_for_model)"; then
            bad "the model cache does not contain ${MODEL} (${models})"
        fi
        dims="$(sql -c "SELECT vector_dims(postvec.embed('remote smoke test','${MODEL}')::vector)" || true)"
        [[ "${dims}" == "${DIMS}" ]] \
            && ok "remote embed() returns ${DIMS} values" \
            || bad "remote embed() returned '${dims}'"
    else
        bad "remote inference was not tested: pass --server-image (or --grpc/--http)"
        printf '        build one: scripts/build-server-image.sh\n' >&2
    fi
fi

echo
echo "persistence and lifecycle"

before="$(sql -c "SELECT count(*) FROM pg_extension WHERE extname='postvec'")"
docker restart "${RUN_ID}" >/dev/null
if wait_healthy; then ok "survives a restart" ; else bad "did not become healthy after a restart"; fi
after="$(sql -c "SELECT count(*) FROM pg_extension WHERE extname='postvec'")"
[[ "${before}" == "${after}" ]] && ok "the extension is still installed" || bad "extension lost across restart"

# Recreating the container on the same volume must keep the data — and must
# *not* re-run the initialisation scripts.
docker rm --force "${RUN_ID}" >/dev/null
start_container
if wait_healthy; then ok "a fresh container on the same volume is healthy"; else bad "recreate failed"; fi
if [[ "${VARIANT}" == complete ]]; then
    rows="$(sql -c "SELECT count(*) FROM smoke WHERE body_semantic IS NOT NULL")"
    [[ "${rows}" == 3 ]] && ok "data and vectors persisted" || bad "only ${rows}/3 rows survived"
fi

# A database engine must be allowed to checkpoint on the way out.
docker stop --timeout 60 "${RUN_ID}" >/dev/null
exit_code="$(docker inspect --format '{{.State.ExitCode}}' "${RUN_ID}")"
[[ "${exit_code}" == 0 ]] && ok "SIGTERM shuts PostgreSQL down cleanly (exit 0)" \
                           || bad "exit code ${exit_code} on SIGTERM — not a clean shutdown"
docker logs "${RUN_ID}" 2>&1 | grep 'database system is shut down' >/dev/null \
    && ok "the shutdown is recorded in the log" \
    || bad "no clean-shutdown line in the log"

docker rm --force "${RUN_ID}" >/dev/null
docker volume rm "${VOLUME}" >/dev/null

echo
echo "failure modes"

# Each of these must be refused loudly at startup, not accepted and left to
# produce a container that answers connections but never works.
expect_startup_failure() {
    local what="$1"; shift
    docker rm --force "${RUN_ID}" >/dev/null 2>&1 || true
    # A regression that lets PostgreSQL start turns `docker run` in the
    # foreground into a process that never returns, so the deadline is part of
    # the assertion rather than a safety net.
    local status=0
    timeout --signal=KILL 120 \
        docker run --rm --name "${RUN_ID}" --env POSTGRES_PASSWORD=x "$@" "${IMAGE}" \
        >/dev/null 2>&1 || status=$?
    docker rm --force "${RUN_ID}" >/dev/null 2>&1 || true
    case "${status}" in
    0)   bad "${what}: the container started anyway" ;;
    137) bad "${what}: the container was still running after 120s — it should have refused immediately" ;;
    *)   ok "${what} (exit ${status})" ;;
    esac
}

expect_startup_failure "an invalid POSTVEC_MODE is refused"   --env POSTVEC_MODE=bogus
expect_startup_failure "an empty database list is refused"    --env POSTVEC_DATABASES=" "
expect_startup_failure "a newline in a value is refused"      --env "POSTVEC_DATABASES=app
-c log_statement=all"
expect_startup_failure "embedded mode without engine assets fails fast" \
    --env POSTVEC_MODE=embedded --env POSTVEC_PATH=/nonexistent

echo
printf '%d passed, %d failed\n' "${passed}" "${failed}"
(( failed == 0 ))
