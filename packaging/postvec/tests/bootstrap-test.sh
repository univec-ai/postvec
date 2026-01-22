#!/usr/bin/env bash
# Run the published prerequisite bootstrap on a clean container of one of the
# distributions it claims to support, and assert it did what it says.
#
#   tests/bootstrap-test.sh                  # every tested distribution
#   tests/bootstrap-test.sh --distro rocky9
#
# `postvec-prerequisites.sh` refuses to execute on a distribution this project
# has not rehearsed. That refusal is only honest if "rehearsed" means something,
# and the thing it has to mean is *this*: the script ran, on that distribution,
# and the packages postvec depends on resolved afterwards.
#
# It is separate from tests/package-install-test.sh because it answers a
# different question much more cheaply. The install test proves a built package
# installs and loads on Debian 12 and EL9 — minutes per cell, and it needs
# packages to have been built. This proves the *first command a user runs* works
# on every distribution the script will execute on, needs nothing built, and is
# the only evidence behind the `TESTED=1` list in that script.

set -Eeuo pipefail

TESTS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PKG_DIR="$(cd "${TESTS_DIR}/.." && pwd)"
BOOTSTRAP="${PKG_DIR}/scripts/postvec-prerequisites.sh"

# The distributions postvec-prerequisites.sh marks TESTED=1, and the image that
# stands in for each. Rocky is here for the same reason it is TESTED there: it
# is claimed, so it is run — "same rebuild as AlmaLinux" is a reason to expect
# it to work, not evidence that it does.
#
# These are tags rather than digests on purpose: the point is that the bootstrap
# keeps working against what these distributions are *now*, including a PGDG or
# EPEL change upstream. A digest-pinned base would freeze exactly the thing this
# test exists to notice.
DISTROS=(
    "debian12:debian:12"
    "ubuntu2204:ubuntu:22.04"
    "ubuntu2404:ubuntu:24.04"
    "almalinux9:almalinux:9"
    "rocky9:rockylinux:9"
)

PG_MAJOR=18
ONLY=""
while (($#)); do
    case "$1" in
    --distro) ONLY="$2"; shift 2 ;;
    --pg)     PG_MAJOR="$2"; shift 2 ;;
    -h|--help) sed -n '2,20p' "$0"; exit 0 ;;
    *) printf 'unknown argument: %s\n' "$1" >&2; exit 2 ;;
    esac
done

command -v docker >/dev/null || { echo "docker is required" >&2; exit 2; }
[[ -f "${BOOTSTRAP}" ]] || { echo "no ${BOOTSTRAP}" >&2; exit 2; }

passed=0; failed=0
ok()  { printf '  \033[32mok\033[0m    %s\n' "$*"; passed=$((passed + 1)); }
bad() { printf '  \033[1;31mFAIL\033[0m  %s\n' "$*" >&2; failed=$((failed + 1)); }

for entry in "${DISTROS[@]}"; do
    name="${entry%%:*}"
    image="${entry#*:}"
    [[ -z "${ONLY}" || "${ONLY}" == "${name}" ]] || continue

    printf '\n\033[1m%s (%s)\033[0m\n' "${name}" "${image}"
    log="$(mktemp)"

    # Twice, in one container: the second run is the idempotency claim, and it
    # is the one an operator actually relies on when they are unsure whether the
    # bootstrap has already been run on a host.
    if docker run --rm \
        --env DEBIAN_FRONTEND=noninteractive \
        --volume "${BOOTSTRAP}:/postvec-prerequisites.sh:ro" \
        "${image}" \
        bash -c "set -e
                 bash /postvec-prerequisites.sh --pg ${PG_MAJOR} --yes
                 echo '### second run ###'
                 bash /postvec-prerequisites.sh --pg ${PG_MAJOR} --yes" \
        >"${log}" 2>&1
    then
        ok "the bootstrap ran, twice, and PG ${PG_MAJOR} resolves afterwards"
    else
        bad "the bootstrap failed on ${image}"
        tail -25 "${log}" | sed 's/^/        /' >&2
        rm -f "${log}"
        continue
    fi

    # It has to have *verified* something, not merely succeeded. A bootstrap
    # that stopped checking fingerprints would still exit 0.
    if grep -q "fingerprint verified" "${log}"; then
        ok "a signing key fingerprint was verified"
    else
        bad "no key fingerprint was verified — the trust root check did not run"
    fi
    case "${image}" in
    almalinux*|rocky*)
        grep -q "signature verified against" "${log}" \
            && ok "the repository RPM's signature was checked against the pinned key alone" \
            || bad "the repository RPM signature check did not run"
        ;;
    esac

    # The second run must be quiet about doing work it has already done.
    if sed -n '/### second run ###/,$p' "${log}" | grep -q "already"; then
        ok "the second run recognised the existing configuration"
    else
        bad "the second run did not report the configuration as already present"
        sed -n '/### second run ###/,$p' "${log}" | sed 's/^/        /' >&2
    fi
    rm -f "${log}"
done

printf '\n'
if (( failed )); then
    printf '\033[1;31m%d failed\033[0m, %d passed\n' "${failed}" "${passed}" >&2
    exit 1
fi
(( passed )) || { echo "no distribution matched --distro ${ONLY}" >&2; exit 2; }
printf '\033[1;32m%d passed\033[0m\n' "${passed}"
