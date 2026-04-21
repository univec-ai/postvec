#!/usr/bin/env bash
# Compute the package dependencies a set of binaries actually needs, using the
# target distribution's own dependency generator.
#
#   elf-depends.sh --distro debian12 --arch amd64 file...   # -> deb relations
#   elf-depends.sh --distro el9 --arch amd64 file...        # -> rpm requires
#
# Prints one relation per line, sorted, ready to be spliced into a package
# description. Prints nothing and fails if there is nothing to report — a
# binary with no dependencies at all means the generator did not run.
#
# Why this exists: nFPM does not run `dpkg-shlibdeps` or RPM's dependency
# generators. It writes exactly the `depends` it is given. Without this step
# `postvec-onnxruntime` would declare no dependencies at all, apt would install
# it happily on a minimal host, and the engine would then fail to dlopen the
# runtime — an error that appears at the first embedding, not at install time.
#
# The generators must run *inside* the target distribution: `libstdc++6 (>= 13)`
# on one release is `libstdc++6 (>= 5.2)` on another, and the answer must match
# the archive the package will be installed from.

source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

DISTRO=""; RELEASE_ARCH=""; FILES=()
while (($#)); do
    case "$1" in
    --distro) DISTRO="$2"; shift 2 ;;
    --arch)   RELEASE_ARCH="$2"; shift 2 ;;
    -h|--help) sed -n '2,20p' "$0"; exit 0 ;;
    *)        FILES+=("$1"); shift ;;
    esac
done

: "${DISTRO:?--distro is required}"
((${#FILES[@]})) || die "no files given"
RELEASE_ARCH="${RELEASE_ARCH:-$(host_release_arch)}"

load_versions
distro_facts "${DISTRO}"
arch_facts "${RELEASE_ARCH}"
need docker

# Under build/, not /tmp: this directory is bind-mounted into a container,
# and a snap-installed docker silently mounts an EMPTY directory for host
# paths outside $HOME (/tmp included). The checkout is already the one place
# every other packaging step mounts from.
mkdir -p "${PKG_DIR}/build"
WORK="$(mktemp -d "${PKG_DIR}/build/elf-depends.XXXXXX")"
trap 'rm -rf "${WORK}"' EXIT

# Copy rather than bind-mount the originals: the generators only read, but a
# release step should not be able to touch a staged artifact at all.
mkdir -p "${WORK}/in"
for file in "${FILES[@]}"; do
    [[ -f "${file}" ]] || die "no such file: ${file}"
    cp "${file}" "${WORK}/in/$(basename "${file}")"
done
# Readable and executable: RPM's generator classifies a file partly by its
# mode, and a copy that lost the executable bit is silently skipped.
chmod -R a+rx "${WORK}"

case "${DIST_FAMILY}" in
deb)
    # dpkg-shlibdeps expects to run inside a source package, so it gets a
    # minimal one. `--ignore-missing-info` keeps a library without shlibs
    # metadata (there are none here, but the flag makes the failure mode a
    # warning rather than an abort on some releases).
    # The generator's stderr is kept (/work/shlibdeps.err) and replayed on
    # failure: apt or dpkg-shlibdeps failing inside the container was
    # otherwise invisible.
    docker run --rm \
        --platform "${OCI_PLATFORM}" \
        --volume "${WORK}:/work" \
        --env DEBIAN_FRONTEND=noninteractive \
        "${DIST_BASE_IMAGE}" \
        bash -euo pipefail -c '
            trap "chmod -R a+rwX /work" EXIT
            # An empty mount means the host directory did not propagate at
            # all (a snap-confined docker does this silently for paths it
            # may not read). Say so instead of failing three steps later.
            compgen -G "/work/in/*" >/dev/null || {
                echo "the /work bind mount is empty inside the container:" >&2
                echo "the host directory did not propagate (snap-confined" >&2
                echo "docker? a daemon on another machine?)" >&2
                exit 90
            }
            apt-get update -qq
            apt-get install -y -qq --no-install-recommends dpkg-dev libc-bin >/dev/null
            mkdir -p /work/src/debian
            cat > /work/src/debian/control <<EOF
Source: postvec-depends
Section: database
Priority: optional
Maintainer: build <build@localhost>

Package: postvec-depends
Architecture: any
Description: dependency probe
EOF
            cd /work/src
            # -O prints to stdout instead of writing debian/substvars. Raw
            # output; the host post-processes, so an empty result reaches the
            # "produced nothing" check below instead of dying opaquely here.
            dpkg-shlibdeps -O --ignore-missing-info /work/in/* \
                >/work/raw 2>/work/shlibdeps.err
        ' || {
        [[ -s "${WORK}/shlibdeps.err" ]] && cat "${WORK}/shlibdeps.err" >&2
        die "the deb dependency generator failed for ${FILES[*]}
(the container output above says why: an apt/network failure inside the
${DIST_BASE_IMAGE} container, dpkg-shlibdeps itself, or a mount that did
not propagate)"
    }
    sed -n 's/^shlibs:Depends=//p' "${WORK}/raw" \
        | tr ',' '\n' \
        | sed 's/^ *//; s/ *$//' \
        | grep -v '^$' \
        | LC_ALL=C sort -u > "${WORK}/out" || true
    ;;
rpm)
    docker run --rm \
        --platform "${OCI_PLATFORM}" \
        --volume "${WORK}:/work" \
        "${DIST_BASE_IMAGE}" \
        bash -euo pipefail -c '
            dnf -y -q install rpm-build >/dev/null
            # rpmdeps is the generator rpmbuild itself invokes; the lower-level
            # elfdeps skips anything it does not consider executable, which
            # makes it quietly wrong for a copied file.
            /usr/lib/rpm/rpmdeps --requires /work/in/* | LC_ALL=C sort -u
            chmod -R a+rwX /work
        ' > "${WORK}/out"
    ;;
esac

[[ -s "${WORK}/out" ]] || die "the ${DIST_FAMILY} dependency generator produced nothing for ${FILES[*]}
A binary with no dependencies at all means the generator did not run — that is
a broken release step, not a dependency-free binary."

# Sanity: a glibc binary must depend on libc, whatever the format spells it.
grep -qiE 'libc6|libc\.so|glibc' "${WORK}/out" \
    || die "no libc dependency was generated; the result is not trustworthy:
$(cat "${WORK}/out")"

cat "${WORK}/out"
