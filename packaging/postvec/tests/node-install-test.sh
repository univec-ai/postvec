#!/usr/bin/env bash
# Install the node bundle on a clean host that has no PostgreSQL, and assert
# what must be true of a fleet node afterwards.
#
#   tests/node-install-test.sh --distro debian12
#   tests/node-install-test.sh --distro el9 --arch amd64
#   tests/node-install-test.sh --distro debian12 --without-cli
#
# tests/package-install-test.sh installs the extension, the CLI, the node and
# the engine assets together: that proves coexistence on a database host.
# This is the other topology, the inference host, and the questions only it
# can answer:
#
#   * does `postvec-server` plus the engine assets install on a host with no
#     PGDG repository and no PostgreSQL, from local files and the
#     distribution's own archive;
#   * is postgresql-common the only PostgreSQL package that arrives, and
#     only via the CLI's Recommends;
#   * does the packaged node then serve the packaged model as its service
#     account, over TLS, with the certificate pair where the packaged
#     configuration says it is;
#   * does removal leave the configuration, the engine root and the account;
#   * with `--without-cli`, does the node package stand alone.
#
# No PostgreSQL bootstrap: a node host never runs it, and running it here
# would hide a package that pulled PostgreSQL in.

set -Eeuo pipefail

TESTS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PKG_DIR="$(cd "${TESTS_DIR}/.." && pwd)"
# shellcheck source=../scripts/lib.sh
source "${PKG_DIR}/scripts/lib.sh"

DISTRO=debian12; RELEASE_ARCH=""; WITH_CLI=1
while (($#)); do
    case "$1" in
    --distro)      DISTRO="$2"; shift 2 ;;
    --arch)        RELEASE_ARCH="$2"; shift 2 ;;
    --without-cli) WITH_CLI=0; shift ;;
    -h|--help) sed -n '2,28p' "$0"; exit 0 ;;
    *) die "unknown argument: $1" ;;
    esac
done
RELEASE_ARCH="${RELEASE_ARCH:-$(host_release_arch)}"

load_versions
load_model_facts
distro_facts "${DISTRO}"
arch_facts "${RELEASE_ARCH}"
need docker

STAGE="$(mktemp_mountable_dir node-install-stage)"
trap 'rm -rf "${STAGE}"' EXIT

COMMON_DIR="$(common_dist_dir "${DISTRO}" "${RELEASE_ARCH}")"
NOARCH_DIR="$(noarch_dist_dir "${DISTRO}")"
[[ -d "${COMMON_DIR}" && -d "${NOARCH_DIR}" ]] || die "no packages at ${COMMON_DIR} / ${NOARCH_DIR}
Build them: scripts/build-extension-stage.sh --distro ${DISTRO} --pg 18 --arch ${RELEASE_ARCH} --with-server
            scripts/build-packages.sh --distro ${DISTRO} --pg 18 --arch ${RELEASE_ARCH}"

shopt -s nullglob
release_only() {
    local candidate
    for candidate in "$@"; do
        case "${candidate}" in
        *-dbgsym[-_]*|*-debuginfo[-_]*|*.spdx.json) continue ;;
        esac
        printf '%s\n' "${candidate}"
    done
}
mapfile -t packages < <(release_only \
    "${COMMON_DIR}"/postvec-server[-_]* \
    "${COMMON_DIR}"/postvec-onnxruntime[-_]* \
    "${NOARCH_DIR}"/postvec-model-* \
    "${NOARCH_DIR}"/"${EXTRAS_METAPACKAGE}"[-_]*)
(( ${#packages[@]} == 4 )) || die "expected the node, the runtime, the model and the metapackage, found ${#packages[@]}:
$(printf '  %s\n' "${packages[@]}")"
if (( WITH_CLI )); then
    mapfile -t -O 4 packages < <(release_only "${COMMON_DIR}"/postvec-cli[-_]*)
    (( ${#packages[@]} == 5 )) || die "expected exactly one postvec-cli package in ${COMMON_DIR}"
fi
shopt -u nullglob
cp "${packages[@]}" "${STAGE}/"

cli_note="without the CLI"; (( WITH_CLI )) && cli_note="with the CLI"
log "installing the node bundle on ${DIST_BASE_IMAGE} (${cli_note})"

docker run --rm \
    --platform "${OCI_PLATFORM}" \
    --volume "${STAGE}:/packages:ro" \
    --env "POSTVEC_VERSION=${POSTVEC_VERSION}" \
    --env "BUNDLED_MODEL_NAME=${MODEL_NAME}" \
    --env "SERVER_LICENSE=${SERVER_LICENSE}" \
    --env "DIST_FAMILY=${DIST_FAMILY}" \
    --env "WITH_CLI=${WITH_CLI}" \
    --env DEBIAN_FRONTEND=noninteractive \
    "${DIST_BASE_IMAGE}" \
    bash -euo pipefail -c '
passed=0; failed=0
ok()   { printf "  \033[32mok\033[0m    %s\n" "$*"; passed=$((passed + 1)); }
bad()  { printf "  \033[1;31mFAIL\033[0m  %s\n" "$*" >&2; failed=$((failed + 1)); }
step() { printf "\n%s\n" "$*"; }

case "${DIST_FAMILY}" in
deb)
    apt-get update -qq
    PKG_INSTALL() { apt-get install -y -qq "$@" >/dev/null; }
    PKG_REMOVE()  { apt-get remove -y -qq "$@" >/dev/null; }
    PKG_LIST()    { dpkg-query -W -f="\${Package}\n" 2>/dev/null | grep -E "$1" || true; }
    PKG_LICENSE() { sed -n "s/^License: //p" "/usr/share/doc/$1/copyright" 2>/dev/null | head -1; }
    INSTALL_ALL() { apt-get install -y -qq /packages/*.deb >/dev/null; }
    ;;
rpm)
    PKG_INSTALL() { dnf install -y -q "$@" >/dev/null; }
    PKG_REMOVE()  { dnf remove -y -q "$@" >/dev/null; }
    PKG_LIST()    { rpm -qa --qf "%{NAME}\n" 2>/dev/null | grep -E "$1" || true; }
    PKG_LICENSE() { rpm -q --qf "%{LICENSE}" "$1" 2>/dev/null; }
    INSTALL_ALL() { dnf install -y -q /packages/*.rpm >/dev/null; }
    ;;
esac

step "install"
if INSTALL_ALL 2>/tmp/install.err; then
    ok "the node bundle installed from local files and the distribution archive alone"
else
    bad "install failed: $(tail -5 /tmp/install.err)"
    printf "\n%d passed, %d failed\n" "${passed}" "${failed}"; exit 1
fi

step "what arrived"
# No PostgreSQL server of any major, from either family. postgresql-common
# (the CLI'"'"'s Depends) is tolerated when the CLI is installed; nothing else.
servers="$(PKG_LIST "^postgresql-?[0-9]+(-server)?$")"
[[ -z "${servers}" ]] \
    && ok "no PostgreSQL server package was pulled in" \
    || bad "PostgreSQL arrived on a node host: ${servers//$'"'"'\n'"'"'/, }"
extension="$(PKG_LIST "^postgresql-?[0-9]+-postvec$")"
[[ -z "${extension}" ]] \
    && ok "no extension package was pulled in" \
    || bad "the extension arrived on a node host: ${extension}"
if [[ "${WITH_CLI}" == 1 ]]; then
    [[ -x /usr/bin/postvec ]] \
        && ok "the CLI is installed (postvec-cli was supplied)" \
        || bad "postvec-cli was supplied but /usr/bin/postvec is missing"
else
    [[ ! -e /usr/bin/postvec ]] \
        && ok "no CLI: the node package stands alone" \
        || bad "/usr/bin/postvec appeared without postvec-cli being supplied"
    [[ -z "$(PKG_LIST "^postgresql-common$")" ]] \
        && ok "no postgresql-common either" \
        || bad "postgresql-common arrived without the CLI"
fi
[[ -x /usr/bin/postvec-server ]] && ok "/usr/bin/postvec-server" || bad "no node binary"
[[ -f /usr/lib/systemd/system/postvec-server.service ]] && ok "the unit" || bad "no unit"
[[ -f /usr/lib/systemd/system/postvec-server.service.d/packaged.conf ]] && ok "the drop-in" || bad "no drop-in"
[[ -f /etc/postvec-server/config.json ]] && ok "/etc/postvec-server/config.json" || bad "no config"
[[ -f /opt/postvec/server/ui/index.html ]] && ok "the dashboard under /opt/postvec/server/ui" || bad "no dashboard"
getent passwd postvec-server >/dev/null && ok "the postvec-server account" || bad "no account"
[[ -f "/opt/postvec/models/onnx-runtime/${BUNDLED_MODEL_NAME}/ninference.hub.json" ]] \
    && ok "the model is at the root the drop-in names" \
    || bad "no model descriptor under /opt/postvec/models"
[[ -n "$(find /opt/postvec/libs -name "libonnxruntime.so*" -print -quit 2>/dev/null)" ]] \
    && ok "ONNX Runtime is under /opt/postvec/libs" \
    || bad "no libonnxruntime under /opt/postvec/libs"
licence="$(PKG_LICENSE postvec-server)"
[[ "${licence}" == "${SERVER_LICENSE}" ]] \
    && ok "postvec-server declares ${SERVER_LICENSE}" \
    || bad "postvec-server declares ${licence:-nothing}"
pgrep -x postvec-server >/dev/null 2>&1 \
    && bad "a node is running after installation" \
    || ok "nothing was started"
# The packaged configuration must point at the pair beside it, not into the
# read-only engine root.
grep -q "\"/etc/postvec-server/server.key\"" /etc/postvec-server/config.json \
    && ok "the packaged config names /etc/postvec-server/server.key" \
    || bad "the packaged config does not name the key under /etc/postvec-server"

step "serve, exactly as the unit would"
if [[ "${DIST_FAMILY}" == deb ]]; then PKG_INSTALL curl openssl procps; else PKG_INSTALL openssl procps-ng; fi
# The certificate pair, installed the way the postinstall says to: root-owned,
# key readable by the service group only.
openssl req -x509 -newkey rsa:2048 -nodes -days 2 -subj "/CN=postvec-server" \
    -addext "subjectAltName=DNS:localhost,IP:127.0.0.1" \
    -keyout /tmp/server.key -out /tmp/server.crt >/dev/null 2>&1
install -o root -g postvec-server -m 0644 /tmp/server.crt /etc/postvec-server/server.crt
install -o root -g postvec-server -m 0640 /tmp/server.key /etc/postvec-server/server.key
# ExecStartPre, and the drop-in'"'"'s environment; the config file supplies
# the certificate paths, so no --ssl flags: this is the packaged default
# working end to end.
install -d -m 1775 -o root -g postvec-server /run/lock/postvec
POSTVEC_SERVER_ROOT=/opt/postvec setpriv --reuid=postvec-server --regid=postvec-server --init-groups \
    postvec-server --config /etc/postvec-server/config.json --bind 127.0.0.1 >/tmp/node.log 2>&1 &
node_pid=$!
deadline=$(( SECONDS + 240 )); ready=""
while (( SECONDS < deadline )); do
    ready="$(curl --silent --insecure --output /dev/null --write-out "%{http_code}" https://127.0.0.1:22222/ready 2>/dev/null || true)"
    [[ "${ready}" == 200 ]] && break
    kill -0 "${node_pid}" 2>/dev/null || break
    sleep 3
done
if [[ "${ready}" == 200 ]]; then
    ok "the node serves the packaged model over TLS with the packaged configuration"
    curl --silent --insecure https://127.0.0.1:22222/config | grep -q "${BUNDLED_MODEL_NAME}" \
        && ok "/config advertises ${BUNDLED_MODEL_NAME}" \
        || bad "/config does not advertise ${BUNDLED_MODEL_NAME}"
    postvec-server status >/dev/null 2>&1 && ok "postvec-server status exits 0" || bad "status failed"
    kill -TERM "${node_pid}"
    if wait "${node_pid}"; then ok "drained and exited 0 on SIGTERM"; else bad "non-zero exit on SIGTERM: $(tail -5 /tmp/node.log)"; fi
else
    bad "the node never became ready: $(tail -10 /tmp/node.log)"
    kill "${node_pid}" 2>/dev/null || true
fi

step "removal keeps the operator'"'"'s state"
# An operator-edited configuration, because that is the case that matters:
# Debian `remove` keeps a conffile edited or not; RPM removes an unchanged
# %config(noreplace) file and keeps an edited one as .rpmsave. Both families
# must end with the operator'"'"'s edit still on disk.
sed -i "s/\"peers\": \[\]/\"peers\": [\"10.0.0.11\"]/" /etc/postvec-server/config.json
grep -q "10.0.0.11" /etc/postvec-server/config.json || bad "could not edit the configuration for the removal test"
PKG_REMOVE postvec-server
[[ ! -e /usr/bin/postvec-server ]] && ok "the binary is gone" || bad "the binary survived removal"
if [[ "${DIST_FAMILY}" == deb ]]; then
    [[ -f /etc/postvec-server/config.json ]] && grep -q "10.0.0.11" /etc/postvec-server/config.json \
        && ok "the edited conffile is kept (remove; purge would delete it)" \
        || bad "the edited conffile did not survive apt remove"
else
    [[ -f /etc/postvec-server/config.json.rpmsave ]] && grep -q "10.0.0.11" /etc/postvec-server/config.json.rpmsave \
        && ok "the edited configuration is kept as config.json.rpmsave" \
        || bad "the edited configuration was not preserved as .rpmsave"
fi
[[ -f /etc/postvec-server/server.key ]] && ok "the certificate pair is kept" || bad "removal deleted the certificate"
[[ -d "/opt/postvec/models/onnx-runtime/${BUNDLED_MODEL_NAME}" ]] && ok "the engine root is kept" || bad "removal touched the engine root"
getent passwd postvec-server >/dev/null && ok "the account is kept (uid never reused)" || bad "removal deleted the account"

printf "\n%d passed, %d failed\n" "${passed}" "${failed}"
(( failed == 0 ))
'
