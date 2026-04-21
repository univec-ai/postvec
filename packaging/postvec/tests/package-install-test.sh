#!/usr/bin/env bash
# Install the built packages on a clean target OS and assert what must be true
# of a real system afterwards.
#
#   tests/package-install-test.sh --distro debian12 --pg 18
#   tests/package-install-test.sh --distro debian12 --pg 16 --pg 18   # coexistence
#   tests/package-install-test.sh --distro el9 --pg 18
#   tests/package-install-test.sh --distro debian12 --pg 18 --minimal  # no engine assets
#
# `verify-package.sh` already answers "is this a well-formed package" from the
# file alone. This answers the questions only a live system can:
#
#   * does the documented prerequisite bootstrap work, and do dependencies then
#     resolve from the archives it configured (PGDG, plus EPEL/CRB on EL9);
#   * do two PostgreSQL majors coexist;
#   * does installing change *nothing* about the running database;
#   * does the shipped library actually load into that distribution's
#     PostgreSQL — `CREATE EXTENSION`, `postvec.build_info()`, and, when the
#     engine assets are installed, a real embedding;
#   * do `postvec setup` / `doctor` / `uninstall` work against it;
#   * does removal leave user data alone.
#
# Both package families run the same assertions; only the package manager,
# the paths and the service layout differ.

set -Eeuo pipefail

TESTS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PKG_DIR="$(cd "${TESTS_DIR}/.." && pwd)"
# shellcheck source=../scripts/lib.sh
source "${PKG_DIR}/scripts/lib.sh"

DISTRO=debian12; RELEASE_ARCH=""; PG_MAJORS=(); MINIMAL=0
while (($#)); do
    case "$1" in
    --distro)  DISTRO="$2"; shift 2 ;;
    --pg)      PG_MAJORS+=("$2"); shift 2 ;;
    --arch)    RELEASE_ARCH="$2"; shift 2 ;;
    --minimal) MINIMAL=1; shift ;;
    -h|--help) sed -n '2,24p' "$0"; exit 0 ;;
    *) die "unknown argument: $1" ;;
    esac
done
((${#PG_MAJORS[@]})) || PG_MAJORS=(18)
RELEASE_ARCH="${RELEASE_ARCH:-$(host_release_arch)}"

load_versions
# The model's name, backend, dimension and package name are derived from the
# verified registry archive the packages were built from — not from a literal
# here, which would keep passing after the bundled model changed.
load_model_facts
distro_facts "${DISTRO}"
arch_facts "${RELEASE_ARCH}"
need docker

STAGE="$(mktemp_mountable_dir install-stage)"   # bind-mounted; /tmp does not propagate under snap docker
STAGE_DEBUG="${STAGE}.debug"
trap 'rm -rf "${STAGE}" "${STAGE_DEBUG}"' EXIT

# `--minimal` installs only the CLI and the extension. That is the dependency
# boundary a remote-mode user actually has, and installing the engine assets
# every time would hide a missing dependency in the extension package behind
# something the metapackage happened to pull in.
# Packages come from three roots: the per-(distribution, architecture) common
# cell, the per-distribution noarch root, and one per-(distribution, major,
# architecture) extension cell. A test that looked only in the major's own cell
# would find no CLI for PG 16 or 17 — and, worse, would pass on PG 18 where the
# two happen to hold the same files.
COMMON_DIR="$(common_dist_dir "${DISTRO}" "${RELEASE_ARCH}")"
NOARCH_DIR="$(noarch_dist_dir "${DISTRO}")"
[[ -d "${COMMON_DIR}" ]] || die "no shared packages at ${COMMON_DIR}
Build them: scripts/build-packages.sh --distro ${DISTRO} --pg 18 --arch ${RELEASE_ARCH} --only cli,onnxruntime"

# Release packages only. The globs are deliberately anchored on the version
# that follows the package name, because `postvec-cli-dbgsym_…` also matches a
# looser `postvec-cli*` — and a "minimal" install that quietly pulled in debug
# symbols would not be testing the minimal boundary.
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

mapfile -t cli_packages < <(release_only "${COMMON_DIR}"/postvec-cli[-_]*)
(( ${#cli_packages[@]} == 1 )) \
    || die "expected exactly one postvec-cli package in ${COMMON_DIR}, found ${#cli_packages[@]}"
cp "${cli_packages[@]}" "${STAGE}/"

for major in "${PG_MAJORS[@]}"; do
    cell="$(extension_dist_dir "${DISTRO}" "${major}" "${RELEASE_ARCH}")"
    [[ -d "${cell}" ]] || die "no extension package for PG ${major} at ${cell}"
    mapfile -t extension_packages < <(release_only "${cell}"/postgresql*postvec[-_]*)
    (( ${#extension_packages[@]} == 1 )) \
        || die "expected exactly one extension package in ${cell}, found ${#extension_packages[@]}"
    cp "${extension_packages[@]}" "${STAGE}/"
done

if (( ! MINIMAL )); then
    [[ -d "${NOARCH_DIR}" ]] || die "no architecture-independent packages at ${NOARCH_DIR}
Build them: scripts/build-packages.sh --distro ${DISTRO} --pg 18 --arch ${RELEASE_ARCH} --only model"
    # ONNX Runtime is architecture-specific and comes from the common cell; the
    # model bundle and the metapackage are `all`/`noarch` and come from the
    # per-distribution noarch root. Reading all three from one directory is what
    # made arm64 silently find nothing.
    mapfile -t engine_packages < <(release_only \
        "${COMMON_DIR}"/postvec-onnxruntime[-_]* \
        "${NOARCH_DIR}"/postvec-model-* \
        "${NOARCH_DIR}"/"${EXTRAS_METAPACKAGE}"[-_]*)
    (( ${#engine_packages[@]} == 3 )) \
        || die "expected the three extra packages across
  ${COMMON_DIR}  (postvec-onnxruntime)
  ${NOARCH_DIR}  (postvec-model-*, ${EXTRAS_METAPACKAGE})
found ${#engine_packages[@]}"
    cp "${engine_packages[@]}" "${STAGE}/"
fi
# The debug packages go to their own mount, deliberately not /packages: the
# release install must be exactly the release packages, and the body installs
# these afterwards so it can check that they land where a debugger looks. A
# minimal run does not stage them at all — that run exists to prove the minimal
# boundary.
mkdir -p "${STAGE_DEBUG}"
if (( ! MINIMAL )); then
    # `packages_only` for the same reason `release_only` exists: CI writes an
    # SBOM next to every artifact, and `postvec-cli-dbgsym_*` matches
    # `postvec-cli-dbgsym_….deb.spdx.json` just as happily as the package.
    packages_only() {
        local candidate
        for candidate in "$@"; do
            case "${candidate}" in *.deb|*.rpm) printf '%s\n' "${candidate}" ;; esac
        done
    }
    mapfile -t debug_packages < <(packages_only \
        "${COMMON_DIR}"/postvec-cli-dbgsym[-_]* "${COMMON_DIR}"/postvec-cli-debuginfo[-_]*)
    for major in "${PG_MAJORS[@]}"; do
        cell="$(extension_dist_dir "${DISTRO}" "${major}" "${RELEASE_ARCH}")"
        mapfile -t -O "${#debug_packages[@]}" debug_packages < <(packages_only \
            "${cell}"/postgresql*postvec-dbgsym[-_]* "${cell}"/postgresql*postvec-debuginfo[-_]*)
    done
    # One per major plus one for the CLI. Missing symbols in a release are a
    # defect, so this is an assertion rather than a best-effort copy.
    (( ${#debug_packages[@]} == ${#PG_MAJORS[@]} + 1 )) \
        || die "expected $(( ${#PG_MAJORS[@]} + 1 )) debug packages, found ${#debug_packages[@]}:
$(printf '  %s\n' "${debug_packages[@]}")"
    cp "${debug_packages[@]}" "${STAGE_DEBUG}/"
fi

shopt -u nullglob

log "installing PG ${PG_MAJORS[*]} packages on ${DIST_BASE_IMAGE}"
(( MINIMAL )) && log "  minimal: CLI and extension only"

docker run --rm \
    --platform "${OCI_PLATFORM}" \
    --volume "${STAGE}:/packages:ro" \
    --volume "${STAGE_DEBUG}:/debug-packages:ro" \
    --volume "${PKG_DIR}/tests/package-install-body.sh:/body.sh:ro" \
    --volume "${PKG_DIR}/scripts/postvec-prerequisites.sh:/postvec-prerequisites.sh:ro" \
    --env "PG_MAJORS=${PG_MAJORS[*]}" \
    --env "POSTVEC_VERSION=${POSTVEC_VERSION}" \
    --env "BUNDLED_MODEL_NAME=${MODEL_NAME}" \
    --env "BUNDLED_MODEL_BACKEND=${MODEL_BACKEND}" \
    --env "BUNDLED_MODEL_TARGET_DIM=${MODEL_TARGET_DIM}" \
    --env "MODEL_PKG_NAME=${MODEL_PKG_NAME}" \
    --env "DIST_FAMILY=${DIST_FAMILY}" \
    --env "MINIMAL=${MINIMAL}" \
    --env DEBIAN_FRONTEND=noninteractive \
    "${DIST_BASE_IMAGE}" \
    bash /body.sh
