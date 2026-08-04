#!/usr/bin/env bash
# postvec-server container entrypoint. Job: the TLS pair the discovery
# listener requires. The server will not start without a readable pair.
#
# If none is present, generate a self-signed pair at container start, not
# in the image: a baked-in key would be shared by every pull. Mount your
# own pair or pass --insecure and this does nothing.
#
# POSTVEC_SERVER_CERTS_DIR matches the packaged config
# (/etc/postvec-server/server.{crt,key}). The crate default is <root>/certs.
set -Eeuo pipefail

case "${1:-}:${2:-}" in
    postvec-server:managed|postvec-server:status|postvec-server:load|postvec-server:unload|postvec-server:--help|postvec-server:--version)
        exec "$@" ;;
esac

ROOT="${POSTVEC_SERVER_ROOT:-/opt/postvec}"
CERTS="${POSTVEC_SERVER_CERTS_DIR:-${ROOT}/certs}"

wants_insecure() {
    case "${POSTVEC_SERVER_INSECURE:-}" in 1 | true | yes | on) return 0 ;; esac
    for arg in "$@"; do
        [[ "${arg}" == "--insecure" ]] && return 0
    done
    return 1
}

# An operator-supplied path means an operator-supplied certificate; never
# generate over the top of one.
supplies_own_cert() {
    [[ -n "${POSTVEC_SERVER_SSL_CERT:-}" ]] && return 0
    for arg in "$@"; do
        case "${arg}" in --ssl-cert | --ssl-cert-key | --ssl-key) return 0 ;; esac
    done
    return 1
}

if ! wants_insecure "$@" && ! supplies_own_cert "$@" \
    && [[ ! -f "${CERTS}/server.crt" || ! -f "${CERTS}/server.key" ]]; then
    if ! mkdir -p "${CERTS}" 2>/dev/null; then
        cat >&2 <<EOF
postvec-server: no TLS certificate at ${CERTS}/server.{crt,key}, and that
directory is not writable so one cannot be generated here.

Either mount a certificate pair at that path, point --ssl-cert/--ssl-cert-key
at one, or pass --insecure to serve discovery over plain HTTP (development
and CI only).
EOF
        exit 1
    fi
    # The SANs cover the ways a container is reached: its compose service name,
    # localhost for a healthcheck, and the hostname Docker assigns it. `bash`
    # supplies HOSTNAME itself — the `hostname` binary is not among the four
    # packages this image installs, and a substitution that came back empty
    # would hand openssl a malformed SAN list.
    echo "postvec-server: generating a self-signed certificate in ${CERTS}" >&2
    openssl req -x509 -newkey rsa:2048 -nodes -days 3650 \
        -subj "/CN=postvec-server" \
        -addext "subjectAltName=DNS:postvec-server,DNS:localhost,DNS:${HOSTNAME:-localhost},IP:127.0.0.1" \
        -keyout "${CERTS}/server.key" \
        -out "${CERTS}/server.crt" >/dev/null 2>&1
    chmod 0600 "${CERTS}/server.key"
fi

exec "$@"
