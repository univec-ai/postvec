#!/usr/bin/env bash
# Container health for a postvec image.
#
# `pg_isready` alone says the postmaster accepts connections — which is true
# long before the extension exists, before the worker is beating, and, in
# embedded mode, long before a model is loaded. Every one of those is the
# difference between a container that answers and a container that works, so
# each is checked here.
#
# Strictly read-only: it runs no inference, refreshes no model cache, starts
# nothing and writes nothing. Health checks that repair things hide the fault
# they were meant to report.
set -Eeuo pipefail

user="${POSTGRES_USER:-postgres}"
database="${POSTVEC_HEALTHCHECK_DATABASE:-${POSTGRES_DB:-${user}}}"
mode="${POSTVEC_MODE:-grpc}"
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
    echo "  started with it cleared or is not a postvec embedded image" >&2
    exit 1
fi
# An idle worker writes one liveness beat per postvec.heartbeat_interval_ms
# (default 30 s), not one per poll tick. The default budget matches doctor:
# liveness interval + three poll ticks + 2 s. A hardcoded 30 s — equal to
# the interval — made an idle container flap unhealthy between writes.
# POSTVEC_HEALTHCHECK_BEAT_AGE, when set, overrides the whole budget (seconds).
max_beat_age="${POSTVEC_HEALTHCHECK_BEAT_AGE:-}"

pg_isready --quiet --username "${user}" --dbname "${database}" || exit 1

# One round trip, one boolean per property, so a failure names itself instead
# of arriving as a bare "unhealthy".
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
    format('extension=%s versions=%s capability=%s heartbeat=%s model=%s',
        ((SELECT count(*) FROM pg_extension WHERE extname = 'postvec') = 1)::text,
        -- The library actually loaded into this backend against the SQL
        -- installed in this database. A mismatch parks the worker.
        ((SELECT extversion FROM pg_extension WHERE extname = 'postvec')
            IS NOT DISTINCT FROM postvec.version())::text,
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
)"

# Every property must say true. `false` fails, and so does an empty value —
# which is what a NULL renders as, and what "no heartbeat row exists at all"
# looks like.
for property in extension versions capability heartbeat model; do
    if [[ "${report}" != *"${property}=true"* ]]; then
        printf 'postvec: unhealthy — %s\n' "${report:-no answer from ${database}}" >&2
        exit 1
    fi
done

printf 'postvec: healthy — %s\n' "${report}"
