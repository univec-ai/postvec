#!/usr/bin/env bash
# Wrap the official PostgreSQL entrypoint: compute postvec settings, then
# exec so PostgreSQL is PID 1.
#
# Settings are POSTMASTER-context (preloaded background worker), so they
# belong here, not in an init script.
set -Eeuo pipefail

# Overridable only so the test harness can substitute a recorder for it; in an
# image this is always the official entrypoint.
OFFICIAL_ENTRYPOINT="${POSTVEC_OFFICIAL_ENTRYPOINT:-/usr/local/bin/docker-entrypoint.sh}"

fatal() { printf 'postvec: %s\n' "$*" >&2; exit 78; }   # EX_CONFIG
usage() { printf 'postvec: %s\n' "$*" >&2; exit 64; }   # EX_USAGE

# ---------------------------------------------------------------- delegation

# `docker run <image> -c work_mem=...` means "run postgres with these flags",
# and `docker run <image> psql ...` means "run this other program". Both are
# official-image behaviour and neither should be given postvec settings.
if [[ "${1:-}" == -* ]]; then
    set -- postgres "$@"
fi
if [[ "${1:-}" != postgres ]]; then
    exec "${OFFICIAL_ENTRYPOINT}" "$@"
fi

# ------------------------------------------------------------- configuration

# The official image's own convention: VAR_FILE points at a secret file and
# wins over VAR. Reimplemented here because these values are needed *before*
# the official entrypoint runs — a POSTGRES_DB supplied only as
# POSTGRES_DB_FILE would otherwise be invisible to us and postvec would serve
# the wrong database.
file_env() {
    local var="$1" file_var="${1}_FILE" default="${2:-}"
    if [[ -n "${!var:-}" && -n "${!file_var:-}" ]]; then
        usage "${var} and ${file_var} are mutually exclusive"
    fi
    local value="${default}"
    if [[ -n "${!var:-}" ]]; then
        value="${!var}"
    elif [[ -n "${!file_var:-}" ]]; then
        [[ -r "${!file_var}" ]] || fatal "${file_var}=${!file_var} is not readable"
        value="$(< "${!file_var}")"
    fi
    export "${var}=${value}"
    unset "${file_var}"
}

file_env POSTGRES_USER postgres
file_env POSTGRES_DB "${POSTGRES_USER}"
file_env POSTVEC_DATABASES "${POSTGRES_DB}"
file_env POSTVEC_SHARED_PRELOAD_LIBRARIES ""

# `${VAR-default}`, not `${VAR:-default}`: an *empty* POSTVEC_MODE is user
# error and must say so, not quietly become grpc. Compose renders an
# undefined interpolation as the empty string, so `POSTVEC_MODE: "${MODE}"`
# with MODE unset would otherwise disable the engine a complete image was
# built around — silently, and only visibly as "search returns FTS only".
#
# The unset fallback stays grpc rather than following the extension's own
# default (`embedded`), because the image knows something the extension does
# not: whether it carries engine assets. Both variants pin POSTVEC_MODE
# explicitly, so this is a net, not a policy.
mode="${POSTVEC_MODE-grpc}"
case "${mode}" in
grpc|embedded) ;;
"") usage "POSTVEC_MODE is set but empty — use 'grpc' or 'embedded', or unset it" ;;
*) usage "POSTVEC_MODE must be 'grpc' or 'embedded', not '${mode}'" ;;
esac

# These values become `postgres -c name=value` arguments. A newline would let a
# value inject an unrelated setting, and a NUL or carriage return would be
# silently mangled — refuse rather than guess.
for value in "${POSTVEC_DATABASES}" "${POSTVEC_SHARED_PRELOAD_LIBRARIES}" \
             "${POSTVEC_GRPC_ENDPOINTS:-}" "${POSTVEC_HTTP_ENDPOINTS:-}" \
             "${POSTVEC_EMBEDDED_MODELS:-}" "${POSTVEC_PATH:-}"; do
    if [[ "${value}" == *$'\n'* || "${value}" == *$'\r'* ]]; then
        usage "configuration values may not contain line breaks"
    fi
done

[[ -n "${POSTVEC_DATABASES//[[:space:],]/}" ]] \
    || usage "POSTVEC_DATABASES is empty — name at least one database to serve"

# `shared_preload_libraries` has its own grammar — quoting, `""` escapes, and
# no case folding, because the items are file names — so the merge is delegated
# to the CLI, which implements exactly that grammar. A user list is preserved
# in order (load order matters to some extensions) and postvec is appended only
# if absent.
# It also *validates*: a value the postmaster would reject (an unterminated
# quote, an empty item) is refused here rather than silently repaired into
# something that starts. The CLI prints the reason on stderr.
if ! preloads="$(postvec __preload-merge "${POSTVEC_SHARED_PRELOAD_LIBRARIES}")"; then
    usage "POSTVEC_SHARED_PRELOAD_LIBRARIES is not a valid shared_preload_libraries value"
fi

postvec_args=(
    -c "shared_preload_libraries=${preloads}"
    -c "postvec.database=${POSTVEC_DATABASES}"
    -c "postvec.mode=${mode}"
)

if [[ "${mode}" == embedded ]]; then
    root="${POSTVEC_PATH:-/opt/postvec}"
    # Fail fast on image corruption: an embedded image whose engine assets are
    # missing can never become healthy, and saying so now is far better than a
    # server that starts, retries every 30 seconds and queues jobs forever.
    [[ -d "${root}/models" ]] || fatal "no model directory under ${root}
This image was built without engine assets, or ${root} was shadowed by a mount.
Use a *-complete image tag, or set POSTVEC_MODE=grpc to use remote inference."
    compgen -G "${root}/libs/**/libonnxruntime.so*" >/dev/null 2>&1 \
        || compgen -G "${root}/libs/*/lib/libonnxruntime.so*" >/dev/null 2>&1 \
        || fatal "no ONNX Runtime under ${root}/libs — the engine could not load a model"

    postvec_args+=(
        -c "postvec.path=${root}"
        # Loopback only, and deliberately not configurable from the
        # environment: anything that can reach the gRPC listener can drive
        # inference, and it has neither authentication nor TLS.
        -c "postvec.embedded_listen=127.0.0.1:33433"
        -c "postvec.embedded_http_listen=127.0.0.1:33434"
    )
    if [[ -n "${POSTVEC_EMBEDDED_MODELS:-}" ]]; then
        postvec_args+=(-c "postvec.embedded_models=${POSTVEC_EMBEDDED_MODELS}")
    fi
else
    if [[ -n "${POSTVEC_GRPC_ENDPOINTS:-}" ]]; then
        postvec_args+=(-c "postvec.grpc_endpoints=${POSTVEC_GRPC_ENDPOINTS}")
    fi
    if [[ -n "${POSTVEC_HTTP_ENDPOINTS:-}" ]]; then
        postvec_args+=(-c "postvec.http_endpoints=${POSTVEC_HTTP_ENDPOINTS}")
    fi
    if [[ -z "${POSTVEC_GRPC_ENDPOINTS:-}" ]]; then
        printf 'postvec: no POSTVEC_GRPC_ENDPOINTS set — the worker will start and idle.\n' >&2
        printf 'postvec: vectors fill once an inference endpoint is configured.\n' >&2
    fi
fi

printf 'postvec: mode=%s databases=%s preload=%s\n' \
    "${mode}" "${POSTVEC_DATABASES}" "${preloads}" >&2

# Generated settings come first so an explicit `-c` from `docker run` still
# wins — the last occurrence of a setting is the one PostgreSQL uses. The
# healthcheck is what catches an override that removes postvec or points the
# worker at an engine that is not there.
exec "${OFFICIAL_ENTRYPOINT}" postgres "${postvec_args[@]}" "${@:2}"
