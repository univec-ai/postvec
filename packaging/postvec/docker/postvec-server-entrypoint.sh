#!/usr/bin/env bash
# Container entrypoint for postvec-server.
#
# Its one job is the TLS certificate. The discovery listener serves HTTPS and
# the server refuses to start without a readable certificate pair — deliberately,
# because silently falling back to plain HTTP on a port operators write into
# `POSTVEC_HTTP_ENDPOINTS=https://…` produces a listener that answers and a
# client that fails confusingly.
#
# So: if no pair is present, generate a self-signed one **at container start**.
# Per container, not baked into the image — an image that shipped a private key
# would hand the same key to everyone who pulled it. postvec's discovery accepts
# self-signed certificates on a trusted network, which is the only kind of
# network this port belongs on.
#
# Mount your own pair over /ninference/certs (or point --ssl-cert / --ssl-cert-key
# elsewhere) and this step does nothing. Pass --insecure and it does nothing
# either.
set -Eeuo pipefail

ROOT="${POSTVEC_SERVER_ROOT:-${NINFERENCE_PATH:-/ninference}}"
CERTS="${ROOT}/certs"

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
postvec-server: no TLS certificate at ${CERTS}/server.{crt,key}, and ${ROOT}
is not writable so one cannot be generated here.

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
