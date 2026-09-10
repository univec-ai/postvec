#!/usr/bin/env bash
# Check the release directory against the packages' own metadata.
#
#   verify-release-metadata.sh [release-dir]
#
# `write-release-manifest.sh` reads a release out of file names. That is the
# right primary source: a file name is the only thing a user holding a
# download has, and it is what `sha256sum --check` and `apt install ./file.deb`
# work from. A package renamed after it was built still satisfies every
# check in the manifest generator and is a different package from the one
# the release says it is.
#
# This asks the packages instead. For each artifact the manifest lists, it
# reads the name, version, release and architecture that `dpkg-deb`/`rpm`
# report from inside the file, reconstructs the file name those fields imply,
# and requires it to be the name the file actually has. It also re-hashes
# every file against the manifest, and requires the directory and the
# manifest to contain exactly the same set of packages.
#
# Earlier jobs already inspected real metadata (`verify-package.sh` runs in
# the job that built each package). This runs at the boundary, over the
# flat directory that is about to be published, so the last thing that
# happens before publication is independently fail-closed.

source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

RELEASE_DIR="${1:-${PKG_DIR}/release}"
[[ "${RELEASE_DIR}" != -h && "${RELEASE_DIR}" != --help ]] || { sed -n '2,26p' "$0"; exit 0; }

load_versions
need python3 jq sha256sum

RELEASE_DIR="$(cd "${RELEASE_DIR}" && pwd)" || die "no such directory: ${1:-}"
MANIFEST="${RELEASE_DIR}/postvec-release.json"
[[ -f "${MANIFEST}" ]] || die "no postvec-release.json in ${RELEASE_DIR}"

fail=0
problem() { printf '    \033[1;31mFAIL\033[0m  %s\n' "$*" >&2; fail=1; }
pass()    { printf '    ok    %s\n' "$*"; }

# `rpm` is not installed on a Debian/Ubuntu release runner. Falling back to a
# container of the target distribution is both possible and the more faithful
# reader — the same choice verify-package.sh makes, for the same reason.
RPM_HOST_TOOL="$(type -P rpm || true)"
rpm_fields() {
    local file="$1"
    local format='%{NAME}\n%{VERSION}\n%{RELEASE}\n%{ARCH}\n'
    if [[ -n "${RPM_HOST_TOOL}" ]]; then
        "${RPM_HOST_TOOL}" -qp --qf "${format}" "${RELEASE_DIR}/${file}" 2>/dev/null
    else
        need docker
        timeout 120 docker run --rm --volume "${RELEASE_DIR}:/pkgs:ro" \
            "almalinux:9@${BUILD_BASE_EL9_DIGEST}" \
            rpm -qp --qf "${format}" "/pkgs/${file}" 2>/dev/null
    fi
}

log "verifying ${RELEASE_DIR} against the packages' own metadata"
[[ -n "${RPM_HOST_TOOL}" ]] || printf '    note  rpm is not installed; reading RPMs in a container\n'

# ------------------------------------------------- the set must match exactly

mapfile -t listed < <(jq -r '.artifacts[].name' "${MANIFEST}" | LC_ALL=C sort)
mapfile -t present < <(cd "${RELEASE_DIR}" && ls -1 ./*.deb ./*.rpm 2>/dev/null | sed 's|^\./||' | LC_ALL=C sort)

if [[ "${listed[*]}" == "${present[*]}" ]]; then
    pass "the manifest lists exactly the ${#listed[@]} package(s) in the directory"
else
    problem "the manifest and the directory disagree about which packages exist:"
    diff <(printf '%s\n' "${listed[@]}") <(printf '%s\n' "${present[@]}") >&2 || true
fi

# ----------------------------------------------------------- per package

for name in "${present[@]}"; do
    file="${RELEASE_DIR}/${name}"
    printf '\n\033[1m%s\033[0m\n' "${name}"

    record="$(jq -c --arg n "${name}" '.artifacts[] | select(.name == $n)' "${MANIFEST}")"
    if [[ -z "${record}" ]]; then
        problem "not described by the manifest"
        continue
    fi

    # The content, against what the manifest promises a downloader.
    actual_sha="$(sha256_of "${file}")"
    actual_size="$(stat -c %s "${file}")"
    [[ "${actual_sha}" == "$(jq -r .sha256 <<<"${record}")" ]] \
        && pass "sha256 matches the manifest" \
        || problem "sha256 is ${actual_sha}, the manifest says $(jq -r .sha256 <<<"${record}")"
    [[ "${actual_size}" == "$(jq -r .size <<<"${record}")" ]] \
        && pass "size matches the manifest" \
        || problem "size is ${actual_size}, the manifest says $(jq -r .size <<<"${record}")"

    # What the package says it is, and the file name that implies.
    case "${name}" in
    *.deb)
        need dpkg-deb
        meta_name="$(dpkg-deb --field "${file}" Package)"
        meta_version="$(dpkg-deb --field "${file}" Version)"
        meta_arch="$(dpkg-deb --field "${file}" Architecture)"
        expected_file="${meta_name}_${meta_version}_${meta_arch}.deb"
        # <upstream>-<revision>+<distro tag>
        meta_release="${meta_version##*-}"
        revision="${meta_release%%+*}"
        distro_tag="${meta_release#*+}"
        ;;
    *.rpm)
        mapfile -t fields < <(rpm_fields "${name}")
        if (( ${#fields[@]} < 4 )); then
            problem "could not read RPM metadata"
            continue
        fi
        meta_name="${fields[0]}"; meta_upstream="${fields[1]}"
        meta_release="${fields[2]}"; meta_arch="${fields[3]}"
        meta_version="${meta_upstream}-${meta_release}"
        expected_file="${meta_name}-${meta_upstream}-${meta_release}.${meta_arch}.rpm"
        revision="${meta_release%%.*}"
        distro_tag="${meta_release#*.}"
        ;;
    esac

    # The check this script exists for: the name on the tin, reconstructed from
    # what is in the tin.
    [[ "${expected_file}" == "${name}" ]] \
        && pass "the file name is what its metadata implies" \
        || problem "metadata implies ${expected_file}, the file is called ${name}"

    [[ "${meta_name}" == "$(jq -r .package <<<"${record}")" ]] \
        && pass "package name agrees with the manifest (${meta_name})" \
        || problem "the package calls itself ${meta_name}, the manifest says $(jq -r .package <<<"${record}")"

    canonical_arch="$(python3 -c '
import sys
print({"x86_64": "amd64", "amd64": "amd64", "aarch64": "arm64",
       "arm64": "arm64", "noarch": "all", "all": "all"}.get(sys.argv[1], sys.argv[1]))' "${meta_arch}")"
    [[ "${canonical_arch}" == "$(jq -r .arch <<<"${record}")" ]] \
        && pass "architecture agrees with the manifest (${meta_arch})" \
        || problem "the package is ${meta_arch}, the manifest says $(jq -r .arch <<<"${record}")"

    # This release's packaging revision and distribution, from the metadata
    # rather than from the file name.
    [[ "${revision}" == "${PACKAGE_RELEASE}" ]] \
        && pass "packaging revision ${revision}" \
        || problem "packaging revision is ${revision}, this release is ${PACKAGE_RELEASE}"
    [[ "${distro_tag}" == "$(jq -r .platform <<<"${record}")" ]] \
        && pass "built for ${distro_tag}" \
        || problem "the package says ${distro_tag}, the manifest says $(jq -r .platform <<<"${record}")"
done

echo
if (( fail )); then
    die "the release directory does not agree with its packages' own metadata"
fi
log "release metadata verified (${#present[@]} package(s))"
