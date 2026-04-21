#!/usr/bin/env bash
# Unit tests for the image entrypoint's argument computation.
#
# The entrypoint decides four things — whether to delegate, which databases to
# serve, which mode, and what shared_preload_libraries becomes — and then
# execs. Each of those has a failure mode that is invisible until a container
# is running and wrong, so they are tested here against a recorder that stands
# in for the official entrypoint. No PostgreSQL, no container, no network:
# `tests/entrypoint-test.sh` runs in under a second.
#
# Run: packaging/postvec/tests/entrypoint-test.sh [path-to-postvec-binary]

set -Eeuo pipefail

TESTS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PKG_DIR="$(cd "${TESTS_DIR}/.." && pwd)"
REPO_ROOT="$(cd "${PKG_DIR}/../.." && pwd)"
ENTRYPOINT="${PKG_DIR}/docker/postvec-entrypoint.sh"

# The entrypoint delegates the preload merge to the CLI. Point at a build of
# it, or let the test fall back to a locally built one.
POSTVEC_BIN="${1:-${POSTVEC_BIN:-}}"
if [[ -z "${POSTVEC_BIN}" ]]; then
    # Newest first: a stale release build lying next to a fresh debug one
    # would otherwise be tested instead of the code under change.
    for candidate in $(ls -t "${REPO_ROOT}"/target/{release,debug}/postvec 2>/dev/null) \
                     "$(command -v postvec || true)"; do
        [[ -x "${candidate}" ]] && { POSTVEC_BIN="${candidate}"; break; }
    done
fi
[[ -x "${POSTVEC_BIN}" ]] || {
    echo "no postvec binary found; build it with: cargo build -p postvec-cli" >&2
    exit 1
}
# The binary is symlinked into a temporary directory below, so a relative path
# (`target/debug/postvec`, which is how CI invokes this) would become a link to
# nothing. Resolve it before anything else uses it.
POSTVEC_BIN="$(realpath "${POSTVEC_BIN}")"

WORK="$(mktemp -d)"
trap 'rm -rf "${WORK}"' EXIT

# A recorder standing in for the official entrypoint: it writes its argv, one
# argument per line, and exits. That is exactly the contract under test.
cat > "${WORK}/recorder" <<'EOF'
#!/usr/bin/env bash
printf '%s\n' "$@"
EOF
chmod +x "${WORK}/recorder"

mkdir -p "${WORK}/bin"
ln -sf "${POSTVEC_BIN}" "${WORK}/bin/postvec"

passed=0 failed=0
ok()   { printf '  \033[32mok\033[0m    %s\n' "$*"; passed=$((passed + 1)); }
bad()  { printf '  \033[1;31mFAIL\033[0m  %s\n' "$*" >&2; failed=$((failed + 1)); }

# Run the entrypoint with a clean environment plus the given assignments, and
# capture what it would have exec'd. `env -i` matters: a stray POSTGRES_DB in
# the developer's shell must not change a test's result.
run_entrypoint() {
    local -a assignments=()
    while [[ "$1" != "--" ]]; do assignments+=("$1"); shift; done
    shift
    env -i \
        PATH="${WORK}/bin:/usr/bin:/bin" \
        HOME="${WORK}" \
        POSTVEC_OFFICIAL_ENTRYPOINT="${WORK}/recorder" \
        "${assignments[@]}" \
        bash "${ENTRYPOINT}" "$@" 2>"${WORK}/stderr"
}

# ${1} description, ${2} expected substring of the recorded argv, then
# `env assignments -- argv`.
expect_arg() {
    local what="$1" expected="$2"; shift 2
    local out status=0
    out="$(run_entrypoint "$@")" || status=$?
    if (( status )); then
        bad "${what}: entrypoint exited ${status} ($(head -1 "${WORK}/stderr"))"
        return
    fi
    # `-e`: an expected argument may itself start with a dash (`-c`).
    if grep -Fxq -e "${expected}" <<<"${out}"; then
        ok "${what}"
    else
        bad "${what}: no argument '${expected}' in:"$'\n'"$(sed 's/^/        /' <<<"${out}")"
    fi
}

expect_exit() {
    local what="$1" want="$2"; shift 2
    local status=0
    run_entrypoint "$@" >/dev/null || status=$?
    if [[ "${status}" == "${want}" ]]; then
        ok "${what} (exit ${status})"
    else
        bad "${what}: expected exit ${want}, got ${status}: $(head -1 "${WORK}/stderr")"
    fi
}

echo "entrypoint: delegation"

# A non-postgres command is somebody's `docker run image psql ...`; it must
# reach the official entrypoint untouched, with no postvec settings attached.
out="$(run_entrypoint -- psql --version)"
if [[ "${out}" == "psql
--version" ]]; then ok "a non-postgres command is passed through verbatim"
else bad "non-postgres passthrough produced: ${out}"; fi

# `docker run image -c work_mem=64MB` means postgres with flags.
expect_arg "a leading flag implies postgres" "postgres" -- -c work_mem=64MB
expect_arg "and the user's flag survives" "-c" -- -c work_mem=64MB
expect_arg "with its value" "work_mem=64MB" -- -c work_mem=64MB

echo
echo "entrypoint: preload merge"

expect_arg "postvec is preloaded by default" \
    "shared_preload_libraries=postvec" -- postgres

# The regression this exists for: an operator's own preload list must survive.
# Overwriting it silently disables their extensions.
expect_arg "a user list is preserved and postvec appended" \
    "shared_preload_libraries=pg_stat_statements,postvec" \
    POSTVEC_SHARED_PRELOAD_LIBRARIES=pg_stat_statements -- postgres

expect_arg "an already-present postvec is not duplicated" \
    "shared_preload_libraries=pg_cron,postvec" \
    POSTVEC_SHARED_PRELOAD_LIBRARIES="pg_cron, postvec" -- postgres

expect_arg "surrounding whitespace is ignored" \
    "shared_preload_libraries=pg_stat_statements,postvec" \
    POSTVEC_SHARED_PRELOAD_LIBRARIES="  pg_stat_statements  " -- postgres

# These are file names, not identifiers: the server does not fold their case,
# so neither may we — `mylib` is simply a different file from `MyLib`.
expect_arg "case is preserved" \
    "shared_preload_libraries=PG_Stat_Statements,postvec" \
    POSTVEC_SHARED_PRELOAD_LIBRARIES="PG_Stat_Statements" -- postgres

# Quoting is re-emitted only where it is load-bearing: `MyLib` and `"MyLib"`
# name the same file, but a name containing a comma cannot survive unquoted.
expect_arg "unnecessary quoting is dropped" \
    'shared_preload_libraries=MyLib,postvec' \
    POSTVEC_SHARED_PRELOAD_LIBRARIES='"MyLib"' -- postgres

expect_arg "load-bearing quoting is kept" \
    'shared_preload_libraries="my,lib",postvec' \
    POSTVEC_SHARED_PRELOAD_LIBRARIES='"my,lib"' -- postgres

expect_arg "the \$libdir spelling counts as present" \
    'shared_preload_libraries=$libdir/postvec' \
    POSTVEC_SHARED_PRELOAD_LIBRARIES='$libdir/postvec' -- postgres

echo
echo "entrypoint: databases"

expect_arg "defaults to POSTGRES_DB" \
    "postvec.database=app" POSTGRES_DB=app -- postgres
expect_arg "falls back to POSTGRES_USER" \
    "postvec.database=alice" POSTGRES_USER=alice -- postgres
expect_arg "then to postgres" \
    "postvec.database=postgres" -- postgres
expect_arg "POSTVEC_DATABASES wins and may list several" \
    "postvec.database=app,analytics" \
    POSTGRES_DB=app POSTVEC_DATABASES=app,analytics -- postgres

# The official image's _FILE convention has to work here too: these values are
# consumed before the official entrypoint gets a chance to expand them.
printf 'secretdb' > "${WORK}/dbname"
expect_arg "POSTGRES_DB_FILE is honoured" \
    "postvec.database=secretdb" POSTGRES_DB_FILE="${WORK}/dbname" -- postgres
expect_exit "POSTGRES_DB and POSTGRES_DB_FILE together are refused" 64 \
    POSTGRES_DB=a POSTGRES_DB_FILE="${WORK}/dbname" -- postgres

echo
echo "entrypoint: validation"

expect_exit "an unknown mode is refused" 64 POSTVEC_MODE=bogus -- postgres

# Compose renders an undefined interpolation as the empty string, so an empty
# POSTVEC_MODE is a realistic accident rather than a hypothetical one. Silently
# reading it as grpc would disable the engine a complete image was built
# around, and the only symptom would be search quietly degrading to FTS.
expect_exit "an empty mode is refused rather than defaulted" 64 POSTVEC_MODE= -- postgres
expect_exit "an empty database list is refused" 64 POSTVEC_DATABASES=" , " -- postgres
expect_exit "a newline in a value is refused" 64 \
    POSTVEC_DATABASES=$'app\n-c log_statement=all' -- postgres
expect_exit "embedded mode without engine assets fails fast" 78 \
    POSTVEC_MODE=embedded POSTVEC_PATH="${WORK}/absent" -- postgres

# A preload list PostgreSQL would refuse must be refused here, not repaired
# into one that starts.
for malformed in '"postvec' 'a,,b' ',postvec' 'postvec,'; do
    expect_exit "a malformed preload list is refused (${malformed})" 64 \
        POSTVEC_SHARED_PRELOAD_LIBRARIES="${malformed}" -- postgres
done

echo
echo "entrypoint: embedded mode"

root="${WORK}/engine"
mkdir -p "${root}/models/onnx-runtime/demo" "${root}/libs/onnxruntime/lib"
touch "${root}/libs/onnxruntime/lib/libonnxruntime.so.1.22.0"

expect_arg "the engine root is passed through" \
    "postvec.path=${root}" \
    POSTVEC_MODE=embedded POSTVEC_PATH="${root}" -- postgres
expect_arg "the mode is set" "postvec.mode=embedded" \
    POSTVEC_MODE=embedded POSTVEC_PATH="${root}" -- postgres

# The security contract: inference listeners are loopback, and no environment
# variable can move them off it.
expect_arg "the gRPC listener is loopback" \
    "postvec.embedded_listen=127.0.0.1:33433" \
    POSTVEC_MODE=embedded POSTVEC_PATH="${root}" -- postgres
expect_arg "the discovery listener is loopback" \
    "postvec.embedded_http_listen=127.0.0.1:33434" \
    POSTVEC_MODE=embedded POSTVEC_PATH="${root}" -- postgres

out="$(run_entrypoint POSTVEC_MODE=embedded POSTVEC_PATH="${root}" \
        POSTVEC_EMBEDDED_LISTEN=0.0.0.0:33433 -- postgres)"
if grep -q '0\.0\.0\.0' <<<"${out}"; then
    bad "an environment variable moved the inference listener off loopback"
else
    ok "no environment variable can expose the inference listener"
fi

echo
echo "entrypoint: remote mode"

expect_arg "gRPC endpoints are passed through" \
    "postvec.grpc_endpoints=192.0.2.2:33333" \
    POSTVEC_GRPC_ENDPOINTS=192.0.2.2:33333 -- postgres
expect_arg "HTTP discovery endpoints are passed through" \
    "postvec.http_endpoints=https://192.0.2.2:22222" \
    POSTVEC_HTTP_ENDPOINTS=https://192.0.2.2:22222 -- postgres

out="$(run_entrypoint POSTVEC_MODE=embedded POSTVEC_PATH="${root}" -- postgres)"
if grep -q 'postvec.grpc_endpoints' <<<"${out}"; then
    bad "remote endpoints were configured in embedded mode"
else
    ok "embedded mode configures no remote endpoints"
fi

echo
echo "entrypoint: user overrides"

# Generated settings come first so a later -c from `docker run` still wins.
out="$(run_entrypoint -- postgres -c log_statement=all)"
generated="$(grep -n 'shared_preload_libraries=' <<<"${out}" | cut -d: -f1)"
override="$(grep -n 'log_statement=all' <<<"${out}" | cut -d: -f1)"
if [[ -n "${generated}" && -n "${override}" && "${override}" -gt "${generated}" ]]; then
    ok "user arguments come after the generated ones"
else
    bad "argument ordering would prevent a user override"
fi

echo
echo "the harness itself"

# CI passes a repository-relative path (`target/debug/postvec`). If that is not
# resolved before the symlink is made, the link points at nothing and every
# assertion above fails with "postvec: not found" — which is exactly how this
# suite was broken once. Re-run the whole thing that way to prove it.
#
# POSTVEC_TEST_NO_RECURSE stops the child from repeating this check forever.
relative="${POSTVEC_BIN#"${REPO_ROOT}/"}"
if [[ -n "${POSTVEC_TEST_NO_RECURSE:-}" ]]; then
    :
elif [[ "${relative}" == "${POSTVEC_BIN}" ]]; then
    printf '  skip  the binary is outside the repository; no relative path to test\n'
elif ( cd "${REPO_ROOT}" && POSTVEC_TEST_NO_RECURSE=1 \
       bash "${TESTS_DIR}/entrypoint-test.sh" "${relative}" >/dev/null 2>&1 ); then
    ok "a repository-relative binary path works too (${relative})"
else
    bad "the suite fails when given a relative path — CI passes one"
fi

echo
printf '%d passed, %d failed\n' "${passed}" "${failed}"
(( failed == 0 ))
