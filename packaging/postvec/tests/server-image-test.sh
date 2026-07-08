#!/usr/bin/env bash
# Exercise a built postvec-server image the way a fleet operator would, on
# its own — without a database in front of it.
#
#   tests/server-image-test.sh ghcr.io/univec-ai/postvec-server:0.1.0-1
#
# The remote-mode smoke test (tests/image-smoke-test.sh --server-image) proves
# that PostgreSQL can embed *through* this image. This proves the image
# itself: that it is composed of this release's packages and nothing else,
# that it runs unprivileged with the admin port unexposed, that it becomes
# ready only once the bundled model answers, that the node's own tooling works
# inside it, that it says which licence it carries, and that it drains cleanly
# on SIGTERM.
#
# Run it against exactly what will be published — a digest in a release job.

set -Eeuo pipefail

TESTS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PKG_DIR="$(cd "${TESTS_DIR}/.." && pwd)"
# shellcheck source=../scripts/lib.sh
source "${PKG_DIR}/scripts/lib.sh"

IMAGE=""
while (($#)); do
    case "$1" in
    -h|--help) sed -n '2,16p' "$0"; exit 0 ;;
    -*) die "unknown argument: $1" ;;
    *) IMAGE="$1"; shift ;;
    esac
done
[[ -n "${IMAGE}" ]] || die "usage: server-image-test.sh <image>"

load_versions
# The model's name and package come from the verified archive the packages
# were built from, so the assertions follow a bundled-model change.
load_model_facts
need docker curl

RUN_ID="postvec-server-test-$$"
passed=0; failed=0
ok()   { printf '  \033[32mok\033[0m    %s\n' "$*"; passed=$((passed + 1)); }
bad()  { printf '  \033[1;31mFAIL\033[0m  %s\n' "$*" >&2; failed=$((failed + 1)); }
cleanup() { docker rm --force "${RUN_ID}" >/dev/null 2>&1 || true; }
trap cleanup EXIT

log "testing ${IMAGE}"

echo
echo "composition"

# The image is the release's packages. Ask dpkg inside it, and require every
# package to carry this release's identity — a stale .deb picked up from an
# earlier build would otherwise pass every functional check below.
inventory="$(docker run --rm --entrypoint dpkg-query "${IMAGE}" \
    -W -f='${Package} ${Version}\n' \
    postvec-server postvec-cli postvec-onnxruntime "${MODEL_PKG_NAME}" "${EXTRAS_METAPACKAGE}" 2>&1)" \
    || { bad "the image does not contain the five packages: ${inventory}"; inventory=""; }
if [[ -n "${inventory}" ]]; then
    ok "postvec-server, postvec-cli, postvec-onnxruntime, ${MODEL_PKG_NAME} and ${EXTRAS_METAPACKAGE} are installed"
    for package in postvec-server postvec-cli "${EXTRAS_METAPACKAGE}"; do
        version="$(awk -v p="${package}" '$1 == p {print $2}' <<<"${inventory}")"
        [[ "${version}" == "${RELEASE_ID}+deb12" ]] \
            && ok "${package} is ${version}" \
            || bad "${package} is ${version:-missing}, expected ${RELEASE_ID}+deb12"
    done
    version="$(awk '$1 == "postvec-onnxruntime" {print $2}' <<<"${inventory}")"
    [[ "${version}" == "${ORT_VERSION}-${PACKAGE_RELEASE}+deb12" ]] \
        && ok "postvec-onnxruntime is ${version}" \
        || bad "postvec-onnxruntime is ${version:-missing}, expected ${ORT_VERSION}-${PACKAGE_RELEASE}+deb12"
    version="$(awk -v p="${MODEL_PKG_NAME}" '$1 == p {print $2}' <<<"${inventory}")"
    [[ "${version}" == "${MODEL_PKG_VERSION}-${PACKAGE_RELEASE}+deb12" ]] \
        && ok "${MODEL_PKG_NAME} is ${version}" \
        || bad "${MODEL_PKG_NAME} is ${version:-missing}, expected ${MODEL_PKG_VERSION}-${PACKAGE_RELEASE}+deb12"
fi

# Nothing else from this project: a stray extension package or a second
# model would be a different image from the one the manifest describes.
extra="$(docker run --rm --entrypoint dpkg-query "${IMAGE}" -W -f='${Package}\n' 2>/dev/null \
    | grep -E '^(postvec|postgresql-[0-9]+-postvec)' \
    | grep -vxE "postvec-server|postvec-cli|postvec-onnxruntime|${MODEL_PKG_NAME}|${EXTRAS_METAPACKAGE}" || true)"
[[ -z "${extra}" ]] \
    && ok "no other postvec package is installed" \
    || bad "unexpected packages in the image: ${extra//$'\n'/, }"

# The licence is a label a registry shows and a scanner reads, so it has to
# be the reviewed one.
label="$(docker image inspect "${IMAGE}" --format '{{index .Config.Labels "org.opencontainers.image.licenses"}}')"
[[ "${label}" == "${SERVER_LICENSE}" ]] \
    && ok "org.opencontainers.image.licenses is ${SERVER_LICENSE}" \
    || bad "org.opencontainers.image.licenses is '${label}', expected ${SERVER_LICENSE}"
label="$(docker image inspect "${IMAGE}" --format '{{index .Config.Labels "org.opencontainers.image.version"}}')"
[[ "${label}" == "${RELEASE_ID}" ]] \
    && ok "org.opencontainers.image.version is ${RELEASE_ID}" \
    || bad "org.opencontainers.image.version is '${label}', expected ${RELEASE_ID}"

version="$(docker run --rm "${IMAGE}" postvec-server --version 2>&1 || true)"
[[ "${version}" == *"${POSTVEC_VERSION}"* ]] \
    && ok "postvec-server --version reports ${POSTVEC_VERSION}" \
    || bad "postvec-server --version said: ${version}"
version="$(docker run --rm "${IMAGE}" postvec --version 2>&1 || true)"
[[ "${version}" == *"${POSTVEC_VERSION}"* ]] \
    && ok "the bundled postvec CLI reports ${POSTVEC_VERSION}" \
    || bad "postvec --version said: ${version}"

echo
echo "serving"

docker run --detach --name "${RUN_ID}" --publish 127.0.0.1::22222 "${IMAGE}" >/dev/null

uid="$(docker exec "${RUN_ID}" id -u)"
[[ "${uid}" != 0 ]] \
    && ok "the node runs unprivileged (uid ${uid})" \
    || bad "the node runs as root"

# Exposed ports are the ones the image declares; the admin port must not be
# among them, because its routes mutate the engine and it is loopback-only by
# design.
exposed="$(docker image inspect "${IMAGE}" --format '{{range $p, $_ := .Config.ExposedPorts}}{{$p}} {{end}}')"
if [[ "${exposed}" == *"22223"* ]]; then
    bad "the admin port is exposed: ${exposed}"
else
    ok "the admin port is not exposed (${exposed% })"
fi

# `/ready` is what the healthcheck asks, and it is 503 until a model answers,
# so "healthy" means "serving", not "listening".
deadline=$(( SECONDS + 300 ))
until [[ "$(docker inspect --format '{{.State.Health.Status}}' "${RUN_ID}" 2>/dev/null)" == healthy ]]; do
    if (( SECONDS >= deadline )); then
        docker logs --tail 40 "${RUN_ID}" >&2
        bad "the node never became ready"
        printf '\n%d passed, %d failed\n' "${passed}" "${failed}"
        exit 1
    fi
    if [[ "$(docker inspect --format '{{.State.Running}}' "${RUN_ID}" 2>/dev/null)" != true ]]; then
        docker logs --tail 40 "${RUN_ID}" >&2
        bad "the node exited before becoming ready"
        printf '\n%d passed, %d failed\n' "${passed}" "${failed}"
        exit 1
    fi
    sleep 3
done
ok "healthy: /ready answered 200"

# The discovery listener, from outside the container, over the TLS pair the
# entrypoint generated — the way postvec's `/config` union reaches a node.
port="$(docker port "${RUN_ID}" 22222/tcp | head -1 | sed 's/.*://')"
config="$(curl --silent --show-error --insecure --fail "https://127.0.0.1:${port}/config" 2>&1)" \
    || { bad "GET /config failed: ${config}"; config=""; }
if [[ -n "${config}" ]]; then
    ok "discovery answers over HTTPS with the generated certificate"
    grep -q "${MODEL_NAME}" <<<"${config}" \
        && ok "/config advertises ${MODEL_NAME}" \
        || bad "/config does not advertise ${MODEL_NAME}: ${config:0:300}"
fi
health="$(curl --silent --insecure --output /dev/null --write-out '%{http_code}' "https://127.0.0.1:${port}/health" || true)"
[[ "${health}" == 200 ]] \
    && ok "/health is 200" \
    || bad "/health is ${health}"
index="$(curl --silent --insecure --fail "https://127.0.0.1:${port}/" 2>/dev/null || true)"
grep -q '<div id="root">' <<<"${index}" \
    && ok "/ serves the dashboard" \
    || bad "/ does not serve the dashboard: ${index:0:200}"
metrics="$(curl --silent --insecure --fail "https://127.0.0.1:${port}/metrics" 2>/dev/null || true)"
[[ -n "${metrics}" ]] \
    && ok "/metrics serves Prometheus text" \
    || bad "/metrics is empty or failed"

# The node's own tooling, inside the container, over the loopback admin port.
status_output="$(docker exec "${RUN_ID}" postvec-server status 2>&1)" && status_exit=0 || status_exit=$?
if (( status_exit == 0 )); then
    ok "postvec-server status reports healthy (exit 0)"
    grep -q "${MODEL_NAME}" <<<"${status_output}" \
        && ok "status lists ${MODEL_NAME} as resident" \
        || bad "status does not list ${MODEL_NAME}: ${status_output:0:300}"
else
    bad "postvec-server status exited ${status_exit}: ${status_output:0:300}"
fi

# The bundled CLI sees the same root the node serves from, with no flags.
# `model show`, not `model ls`: the listing truncates long names to fit a
# column, and "the name appears in the table" is not the claim anyway.
if shown="$(docker exec "${RUN_ID}" postvec model show "${MODEL_NAME}" 2>&1)"; then
    ok "postvec model show inside the image finds ${MODEL_NAME}"
else
    bad "postvec model show ${MODEL_NAME} failed inside the image: ${shown:0:300}"
fi

echo
echo "shutdown"

# SIGTERM starts a drain: /ready flips to 503 and the node keeps serving for
# the drain delay, then exits 0. `docker stop` sends SIGTERM and waits.
docker stop --time 45 "${RUN_ID}" >/dev/null
exit_code="$(docker inspect --format '{{.State.ExitCode}}' "${RUN_ID}")"
[[ "${exit_code}" == 0 ]] \
    && ok "SIGTERM drained and exited 0" \
    || { bad "exited ${exit_code} on SIGTERM"; docker logs --tail 20 "${RUN_ID}" >&2; }
# Captured first, then searched: `docker logs | grep -q` under pipefail
# reports failure whenever grep exits before docker logs has finished writing.
final_log="$(docker logs "${RUN_ID}" 2>&1 || true)"
if grep -qi 'drain\|shutting down\|shutdown' <<<"${final_log}"; then
    ok "the log records the drain"
else
    bad "the log does not mention draining: ${final_log: -300}"
fi

printf '\n%d passed, %d failed\n' "${passed}" "${failed}"
(( failed == 0 ))
