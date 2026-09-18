#!/usr/bin/env bash
# Upgrade a real installation from the previous release's packages to the
# packages just built, on a clean target OS, the way a user does it.
#
#   tests/package-upgrade-test.sh --distro debian12 --pg 18 --from-tag postvec-v0.1.0-1
#   tests/package-upgrade-test.sh --distro el9 --pg 17 --from-dir ~/prev-release/dist
#
# postvec/upgrade_test.sh proves the SQL side from source: catalog parity,
# data survival, the worker's version gate. This proves what only the packages
# can: that the previous release's packages install, that `apt`/`dnf` replace
# them in place, that the upgrade script ships in the distribution's own
# extension directory, that the new library loads against the old catalog, and
# that ALTER EXTENSION postvec UPDATE works on that distribution's PostgreSQL.
#
# The previous release comes either from a GitHub Release (--from-tag, needs
# `gh` and read access to the repository) or from a directory of its packages
# (--from-dir, searched recursively; a `dist/` tree from release.sh works).
# Only the CLI and the extension for one major are exercised: that is where
# an upgrade can break, and the minimal install is the boundary a remote-mode
# database host has.

set -Eeuo pipefail

TESTS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PKG_DIR="$(cd "${TESTS_DIR}/.." && pwd)"
# shellcheck source=../scripts/lib.sh
source "${PKG_DIR}/scripts/lib.sh"

DISTRO=debian12; RELEASE_ARCH=""; MAJOR=18; FROM_TAG=""; FROM_DIR=""; REPO=""
while (($#)); do
    case "$1" in
    --distro)   DISTRO="$2"; shift 2 ;;
    --pg)       MAJOR="$2"; shift 2 ;;
    --arch)     RELEASE_ARCH="$2"; shift 2 ;;
    --from-tag) FROM_TAG="$2"; shift 2 ;;
    --from-dir) FROM_DIR="$2"; shift 2 ;;
    --repo)     REPO="$2"; shift 2 ;;
    -h|--help)  sed -n '2,20p' "$0"; exit 0 ;;
    *) die "unknown argument: $1" ;;
    esac
done
[[ -n "${FROM_TAG}" || -n "${FROM_DIR}" ]] || die "name the previous release: --from-tag <tag> or --from-dir <dir>"
[[ -z "${FROM_TAG}" || -z "${FROM_DIR}" ]] || die "--from-tag and --from-dir are alternatives"
RELEASE_ARCH="${RELEASE_ARCH:-$(host_release_arch)}"

load_versions
require_pg_major "${MAJOR}"
distro_facts "${DISTRO}"
arch_facts "${RELEASE_ARCH}"
need docker

STAGE="$(mktemp_mountable_dir upgrade-stage)"   # bind-mounted; /tmp does not propagate under snap docker
trap 'rm -rf "${STAGE}"' EXIT
mkdir -p "${STAGE}/old" "${STAGE}/new" "${STAGE}/download"

# The two packages per side, by exact family naming. Anchoring on the version
# digit right after the name keeps debug-symbol packages (…-dbgsym_…,
# …-debuginfo-…) and SBOMs out.
case "${DIST_FAMILY}" in
deb)
    CLI_GLOB="postvec-cli_[0-9]*+${DIST_ID}_${DEB_ARCH}.deb"
    EXT_GLOB="postgresql-${MAJOR}-postvec_[0-9]*+${DIST_ID}_${DEB_ARCH}.deb" ;;
rpm)
    CLI_GLOB="postvec-cli-[0-9]*.${DIST_ID}.${RPM_ARCH}.rpm"
    EXT_GLOB="postgresql${MAJOR}-postvec-[0-9]*.${DIST_ID}.${RPM_ARCH}.rpm" ;;
esac

pick() {  # <dest> <search-root> — copy exactly one CLI and one extension package
    local dest="$1" root="$2" glob found
    for glob in "${CLI_GLOB}" "${EXT_GLOB}"; do
        mapfile -t found < <(find "${root}" -type f -name "${glob}" | LC_ALL=C sort)
        (( ${#found[@]} == 1 )) || die "expected one ${glob} under ${root}, found ${#found[@]}:
$(printf '  %s\n' "${found[@]}")"
        cp "${found[0]}" "${dest}/"
    done
}

# ------------------------------------------------------- the previous release
if [[ -n "${FROM_TAG}" ]]; then
    need gh
    REPO="${REPO:-${GH_REPO:-${SOURCE_REPOSITORY#https://github.com/}}}"
    REPO="${REPO%.git}"
    log "downloading ${FROM_TAG} packages for ${DISTRO} PG ${MAJOR} ${RELEASE_ARCH} from ${REPO}"
    gh release download "${FROM_TAG}" --repo "${REPO}" --dir "${STAGE}/download" \
        --pattern "${CLI_GLOB}" --pattern "${EXT_GLOB}" \
        || die "could not download ${FROM_TAG} from ${REPO}"
    pick "${STAGE}/old" "${STAGE}/download"
else
    [[ -d "${FROM_DIR}" ]] || die "no directory ${FROM_DIR}"
    pick "${STAGE}/old" "${FROM_DIR}"
fi

# ------------------------------------------------------------- this release
pick "${STAGE}/new" "${PKG_DIR}/dist"

version_in() {  # the product version inside a package filename
    sed -nE 's/.*[_-]([0-9]+\.[0-9]+\.[0-9]+)-[0-9]+[.+].*/\1/p' <<<"$(basename "$1")"
}
OLD_VERSION="$(version_in "$(find "${STAGE}/old" -type f -name "${EXT_GLOB}" | head -1)")"
[[ -n "${OLD_VERSION}" ]] || die "could not read the previous version from $(ls "${STAGE}/old")"
[[ "${OLD_VERSION}" != "${POSTVEC_VERSION}" ]] \
    || die "the previous packages are also ${POSTVEC_VERSION}: a packaging-only release has no extension upgrade"

log "upgrading PG ${MAJOR} on ${DIST_BASE_IMAGE}: postvec ${OLD_VERSION} -> ${POSTVEC_VERSION}"
docker run --rm \
    --platform "${OCI_PLATFORM}" \
    --volume "${STAGE}/old:/packages-old:ro" \
    --volume "${STAGE}/new:/packages-new:ro" \
    --volume "${PKG_DIR}/tests/package-upgrade-body.sh:/body.sh:ro" \
    --volume "${PKG_DIR}/scripts/postvec-prerequisites.sh:/postvec-prerequisites.sh:ro" \
    --env "MAJOR=${MAJOR}" \
    --env "OLD_VERSION=${OLD_VERSION}" \
    --env "NEW_VERSION=${POSTVEC_VERSION}" \
    --env "DIST_FAMILY=${DIST_FAMILY}" \
    --env DEBIAN_FRONTEND=noninteractive \
    "${DIST_BASE_IMAGE}" \
    bash /body.sh
