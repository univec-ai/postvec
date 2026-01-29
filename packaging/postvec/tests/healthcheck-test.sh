#!/usr/bin/env bash
# Unit tests for the image health check's verdict.
#
# The health check is what decides whether a container counts as up: it gates
# `docker run --wait`, compose `depends_on: service_healthy`, orchestrator
# readiness, and the release's own smoke test. Every one of its judgements is
# therefore load-bearing, and every one of them was previously only exercised
# end to end — where the only observable is "the container went healthy", which
# says nothing about *why*, and nothing at all about the cases where it must
# refuse.
#
# The two properties worth stating plainly, because both have bitten:
#
#   - It must **fail closed**. An empty report, a missing property, a NULL
#     rendering, a psql that could not connect — each has to be unhealthy.
#     A health check that reports healthy when it learned nothing is worse
#     than no health check.
#   - It must not repair anything. It runs read-only SQL and nothing else.
#
# `pg_isready` and `psql` are stubbed, so this needs no PostgreSQL, no
# container and no network, and runs in well under a second.
#
# Run: packaging/postvec/tests/healthcheck-test.sh

set -Eeuo pipefail

TESTS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PKG_DIR="$(cd "${TESTS_DIR}/.." && pwd)"
HEALTHCHECK="${PKG_DIR}/docker/postvec-healthcheck.sh"

WORK="$(mktemp -d)"
trap 'rm -rf "${WORK}"' EXIT
mkdir -p "${WORK}/bin"

# `pg_isready`: exits with whatever ${WORK}/isready-status says, so the
# "postmaster is not accepting connections yet" path is reachable.
cat > "${WORK}/bin/pg_isready" <<'EOF'
#!/usr/bin/env bash
exit "$(cat "${STUB_DIR}/isready-status")"
EOF

# `psql`: prints whatever ${WORK}/report holds and exits with
# ${WORK}/psql-status. It also records its argv, so the "does it write
# anything" property can be asserted rather than assumed.
cat > "${WORK}/bin/psql" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$@" > "${STUB_DIR}/psql-argv"
cat > "${STUB_DIR}/psql-stdin"
cat "${STUB_DIR}/report"
exit "$(cat "${STUB_DIR}/psql-status")"
EOF
chmod +x "${WORK}/bin/pg_isready" "${WORK}/bin/psql"

passed=0 failed=0
ok()  { printf '  \033[32mok\033[0m    %s\n' "$*"; passed=$((passed + 1)); }
bad() { printf '  \033[1;31mFAIL\033[0m  %s\n' "$*" >&2; failed=$((failed + 1)); }

# A report with every property true; callers flip one at a time.
full_report() {
    printf 'extension=true versions=true mode=true capability=true heartbeat=true model=true\n'
}

# `run_healthcheck <report> <isready-status> <psql-status> -- [env assignments]`
run_healthcheck() {
    local report="$1" isready="$2" psql_status="$3"; shift 3
    [[ "$1" == "--" ]] && shift
    printf '%s' "${report}" > "${WORK}/report"
    printf '%s' "${isready}" > "${WORK}/isready-status"
    printf '%s' "${psql_status}" > "${WORK}/psql-status"
    rm -f "${WORK}/psql-argv" "${WORK}/psql-stdin"
    env -i \
        PATH="${WORK}/bin:/usr/bin:/bin" \
        HOME="${WORK}" \
        STUB_DIR="${WORK}" \
        "$@" \
        bash "${HEALTHCHECK}" >"${WORK}/stdout" 2>"${WORK}/stderr"
}

# `expect <description> <expected exit> <report> [env assignments]`
expect() {
    local what="$1" want="$2" report="$3"; shift 3
    local status=0
    run_healthcheck "${report}" 0 0 -- "$@" || status=$?
    if [[ "${status}" == "${want}" ]]; then
        ok "${what}"
    else
        bad "${what}: exited ${status}, expected ${want}
        stdout: $(head -1 "${WORK}/stdout" 2>/dev/null)
        stderr: $(head -2 "${WORK}/stderr" 2>/dev/null | tr '\n' ' ')"
    fi
}

echo
echo "the happy paths"

expect "a fully true report in grpc mode is healthy" 0 "$(full_report)" \
    POSTVEC_MODE=grpc
expect "a fully true report in embedded mode is healthy" 0 "$(full_report)" \
    POSTVEC_MODE=embedded POSTVEC_EMBEDDED_MODELS=minilm
expect "an unset mode is accepted (the image pins it; this is the net)" 0 \
    "$(full_report)"

echo
echo "it fails closed"

# The core property. Each of these is a state in which the container is not
# serving, and each one used to be indistinguishable from a healthy container
# to anything reading the exit code.
for property in extension versions mode capability heartbeat model; do
    expect "a false ${property} is unhealthy" 1 \
        "$(full_report | sed "s/${property}=true/${property}=false/")" \
        POSTVEC_MODE=embedded POSTVEC_EMBEDDED_MODELS=minilm
done

# A NULL renders as the empty string, which is what "the heartbeat row does
# not exist at all" looks like — the state a never-started worker is in.
expect "an empty property value is unhealthy, not absent-therefore-fine" 1 \
    "extension=true versions=true mode=true capability=true heartbeat= model=true" \
    POSTVEC_MODE=embedded POSTVEC_EMBEDDED_MODELS=minilm

expect "a report missing a property entirely is unhealthy" 1 \
    "extension=true versions=true mode=true capability=true model=true" \
    POSTVEC_MODE=embedded POSTVEC_EMBEDDED_MODELS=minilm

expect "an empty report is unhealthy" 1 "" POSTVEC_MODE=grpc

expect "a SQL error in place of a report is unhealthy" 1 \
    "ERROR:  relation \"postvec.stats\" does not exist" POSTVEC_MODE=grpc

# Substring matching would accept `extension=true` inside `notextension=true`;
# it must not accept a *different* property's truth as this one's.
expect "one property cannot satisfy another" 1 \
    "extension=true versions=true mode=true capability=true heartbeat=true" \
    POSTVEC_MODE=grpc

echo
echo "connection and configuration faults"

status=0
run_healthcheck "$(full_report)" 1 0 -- POSTVEC_MODE=grpc || status=$?
[[ "${status}" == 1 ]] \
    && ok "a postmaster that is not ready is unhealthy" \
    || bad "pg_isready failure exited ${status}"

# ... and psql must not even be reached: a health check that probes SQL through
# a postmaster that is not accepting connections turns a startup delay into a
# connection-refused error in the logs.
[[ ! -f "${WORK}/psql-argv" ]] \
    && ok "psql is not run when the postmaster is not ready" \
    || bad "psql ran anyway: $(tr '\n' ' ' < "${WORK}/psql-argv")"

status=0
run_healthcheck "" 0 2 -- POSTVEC_MODE=grpc || status=$?
[[ "${status}" == 1 ]] \
    && ok "a psql that fails to connect is unhealthy" \
    || bad "a failing psql exited ${status}"

status=0
run_healthcheck "$(full_report)" 0 0 -- POSTVEC_MODE=embedded || status=$?
[[ "${status}" == 1 ]] \
    && ok "embedded mode with no configured model is unhealthy" \
    || bad "an embedded container with POSTVEC_EMBEDDED_MODELS unset exited ${status}"
grep -q "POSTVEC_EMBEDDED_MODELS is unset" "${WORK}/stderr" \
    && ok "and it names the missing variable" \
    || bad "the message does not name POSTVEC_EMBEDDED_MODELS: $(head -1 "${WORK}/stderr")"

# The same accident the entrypoint refuses: Compose renders an undefined
# interpolation as the empty string. Reading that as grpc here would skip the
# capability and model checks and pass an embedded container whose engine is
# doing nothing.
status=0
run_healthcheck "$(full_report)" 0 0 -- POSTVEC_MODE= || status=$?
[[ "${status}" == 1 ]] \
    && ok "an empty mode is refused rather than read as grpc" \
    || bad "an empty POSTVEC_MODE exited ${status}"

status=0
run_healthcheck "$(full_report)" 0 0 -- POSTVEC_MODE=bogus || status=$?
[[ "${status}" == 1 ]] \
    && ok "an unknown mode is refused" \
    || bad "POSTVEC_MODE=bogus exited ${status}"

echo
echo "it is read-only"

run_healthcheck "$(full_report)" 0 0 -- POSTVEC_MODE=grpc
# One psql invocation, and the statement it runs is a single SELECT. A health
# check that repairs things hides the fault it exists to report.
if grep -qE '^(INSERT|UPDATE|DELETE|CREATE|ALTER|DROP|TRUNCATE|SELECT postvec\.(refresh_models|enable|migrate))' \
        "${WORK}/psql-stdin"; then
    bad "the health check issues a writing statement:
$(grep -nE '^(INSERT|UPDATE|DELETE|CREATE|ALTER|DROP|TRUNCATE)' "${WORK}/psql-stdin")"
else
    ok "the SQL it runs writes nothing"
fi
grep -q -- "--set" "${WORK}/psql-argv" \
    && ok "values reach SQL as psql variables, not as interpolated text" \
    || bad "psql was called without --set: $(tr '\n' ' ' < "${WORK}/psql-argv")"

echo
printf '%d passed, %d failed\n' "${passed}" "${failed}"
(( failed == 0 ))
