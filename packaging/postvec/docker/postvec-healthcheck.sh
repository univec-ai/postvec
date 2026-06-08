#!/usr/bin/env bash
# Container health. pg_isready is not enough: also require the extension,
# a live worker, and in embedded mode a loaded model.
#
# Read-only: no inference, no cache refresh, no writes.
set -Eeuo pipefail

user="${POSTGRES_USER:-postgres}"
database="${POSTVEC_HEALTHCHECK_DATABASE:-${POSTGRES_DB:-${user}}}"
# `${VAR-grpc}`, matching the entrypoint: an empty POSTVEC_MODE is user error,
# not a request for remote inference. Reading it as grpc here would skip the
# capability and model checks and report an embedded container healthy while
# its engine did nothing.
mode="${POSTVEC_MODE-grpc}"
case "${mode}" in
grpc | embedded) ;;
*)
    echo "unhealthy: POSTVEC_MODE is '${mode}', which is neither 'grpc' nor 'embedded'" >&2
    exit 1
    ;;
esac
# The first configured model is the one the image promises. This release
# bundles exactly one; if that ever changes, compare sets here.
#
# There is deliberately no default. The image always sets
# POSTVEC_EMBEDDED_MODELS from the bundle it was built with, so an unset value
# in embedded mode is a misconfigured image — and a hard-coded fallback would
# probe a model that may not be installed and report the wrong fault.
model="${POSTVEC_EMBEDDED_MODELS:-}"
if [[ "${mode}" == embedded && -z "${model}" ]]; then
    echo "unhealthy: POSTVEC_EMBEDDED_MODELS is unset in an embedded-mode container;" >&2
    echo "  the image sets it from the model it bundles, so this container was" >&2
    echo "  started with it cleared or is not a postvec local image" >&2
    exit 1
fi
# An idle worker writes one beat per postvec.heartbeat_interval_ms
# (default 30 s), not per poll. Default budget matches doctor: interval
# plus three poll ticks plus 2 s. A budget equal to the interval flaps
# an idle container. POSTVEC_HEALTHCHECK_BEAT_AGE overrides the whole
# budget in seconds.
max_beat_age="${POSTVEC_HEALTHCHECK_BEAT_AGE:-}"

pg_isready --quiet --username "${user}" --dbname "${database}" || exit 1

# One round trip, one boolean per property, so a failure names itself instead
# of arriving as a bare "unhealthy".
#
# The status is captured rather than inherited. Under `set -e` a failing
# command substitution in an assignment ends the script *with psql's own exit
# code* — 2 for a connection that dropped mid-check — which is the one value
# Docker reserves in the health-check contract, and which skips every
# diagnostic below. A health check that cannot connect is unhealthy, and it
# should say why.
report=""
psql_status=0
report="$(
    psql --no-psqlrc --tuples-only --no-align --field-separator=' ' \
         --username "${user}" --dbname "${database}" \
         --set ON_ERROR_STOP=1 \
         --set mode="${mode}" \
         --set model="${model}" \
         --set beat_age="${max_beat_age}" <<'SQL' 2>&1
-- Every value is cast to text explicitly: PostgreSQL's default boolean
-- rendering is 't'/'f', and a shell contract built on that is one output
-- format change away from reporting every container healthy.
SELECT
    format('extension=%s versions=%s mode=%s capability=%s heartbeat=%s model=%s',
        ((SELECT count(*) FROM pg_extension WHERE extname = 'postvec') = 1)::text,
        -- The library actually loaded into this backend against the SQL
        -- installed in this database. A mismatch parks the worker.
        ((SELECT extversion FROM pg_extension WHERE extname = 'postvec')
            IS NOT DISTINCT FROM postvec.version())::text,
        -- The mode the *server* is running, against the one this container was
        -- told to run. They diverge when POSTVEC_MODE is changed without a
        -- restart, or when a mounted configuration file overrides the
        -- entrypoint's `-c postvec.mode=` — states in which every other check
        -- below is answering about the wrong half of the installation.
        -- An unset or empty setting is the extension's default, embedded.
        (:'mode' = CASE
                       WHEN COALESCE(NULLIF(current_setting('postvec.mode', true), ''), 'embedded')
                            = 'grpc' THEN 'grpc'
                       ELSE 'embedded'
                   END)::text,
        -- An embedded-mode image whose library cannot do embedded mode would
        -- start, warn once, and never embed anything.
        (:'mode' <> 'embedded'
            OR (postvec.build_info()->'features'->>'embedded')::boolean)::text,
        -- The heartbeat row survives a dead worker, so its presence proves
        -- nothing — only its age does. Idle workers write once per
        -- heartbeat_interval_ms; the default budget is that interval plus
        -- three poll ticks and two seconds (same as postvec doctor).
        (SELECT (worker_last_beat::timestamptz > clock_timestamp()
                    - CASE
                        WHEN :'beat_age' <> '' THEN (:'beat_age' || ' seconds')::interval
                        ELSE (COALESCE(NULLIF(current_setting('postvec.heartbeat_interval_ms', true), ''), '30000')::int
                              + 3 * COALESCE(NULLIF(current_setting('postvec.poll_interval_ms', true), ''), '5000')::int
                              + 2000) * interval '1 millisecond'
                      END)::text
           FROM postvec.stats()),
        (:'mode' <> 'embedded'
            OR EXISTS (SELECT 1 FROM postvec.models
                        WHERE name = split_part(:'model', ',', 1)))::text
    );
SQL
)" || psql_status=$?

if (( psql_status != 0 )); then
    printf 'postvec: unhealthy — psql exited %d: %s\n' \
        "${psql_status}" "${report:-no output}" >&2
    exit 1
fi

# Every property must say true. `false` fails, and so does an empty value —
# which is what a NULL renders as, and what "no heartbeat row exists at all"
# looks like.
for property in extension versions mode capability heartbeat model; do
    if [[ "${report}" != *"${property}=true"* ]]; then
        printf 'postvec: unhealthy — %s\n' "${report:-no answer from ${database}}" >&2
        exit 1
    fi
done

printf 'postvec: healthy — %s\n' "${report}"
