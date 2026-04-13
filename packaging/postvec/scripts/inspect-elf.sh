#!/usr/bin/env bash
# Prove what the produced binaries actually depend on, instead of assuming it.
#
#   inspect-elf.sh --arch amd64 --library …/postvec.so --binary …/postvec
#
# The rules below encode the properties a postvec release artifact must have.
# The interesting one is the ONNX Runtime rule: embedded mode dlopen()s
# libonnxruntime at runtime from the engine root, so a *link-time* dependency
# on it would mean every installation — including every remote-mode one —
# needs the runtime present just to load the extension.

source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

RELEASE_ARCH=""; LIBRARY=""; BINARY=""; DISTRO="${POSTVEC_DISTRO:-}"
while (($#)); do
    case "$1" in
    --arch)    RELEASE_ARCH="$2"; shift 2 ;;
    --distro)  DISTRO="$2"; shift 2 ;;
    --library) LIBRARY="$2"; shift 2 ;;
    --binary)  BINARY="$2"; shift 2 ;;
    -h|--help) sed -n '2,12p' "$0"; exit 0 ;;
    *) die "unknown argument: $1" ;;
    esac
done

RELEASE_ARCH="${RELEASE_ARCH:-$(host_release_arch)}"
load_versions
arch_facts "${RELEASE_ARCH}"
# The glibc floor is a property of the target distribution. When the caller
# knows it (build-extension-stage.sh exports DIST_ID), that check runs; a
# standalone run without --distro reports the requirement instead.
[[ -n "${DISTRO}" ]] && distro_facts "${DISTRO}"
need readelf objdump file
[[ -n "${DISTRO}" ]] && need docker

fail=0
problem() { printf '  \033[1;31mFAIL\033[0m  %s\n' "$*" >&2; fail=1; }
pass()    { printf '  ok    %s\n' "$*"; }

# The oldest glibc each supported distribution provides. A binary that needs a
# newer symbol version installs cleanly and then fails to dlopen with
# "GLIBC_x.y not found" — at CREATE EXTENSION time, on the user's machine.
glibc_floor_for() {
    case "$1" in
    deb12)       echo 2.36 ;;   # Debian 12 bookworm
    ubuntu22.04) echo 2.35 ;;
    ubuntu24.04) echo 2.39 ;;
    el9)         echo 2.34 ;;   # RHEL 9
    *)           echo "" ;;
    esac
}

version_le() { [[ "$(printf '%s\n%s\n' "$1" "$2" | sort -V | head -1)" == "$1" ]]; }

# Resolve an artifact against the distribution it was built for, not against
# the release host. Architecture equality is not enough: an Ubuntu 24.04
# binary legitimately requiring GLIBC_2.39 cannot be loaded by ldd on an
# Ubuntu 22.04 release host. Copying into a stopped container avoids bind-mount
# differences between ordinary, rootless, and Snap-packaged Docker daemons.
inspect_target_dependencies() {
    local path="$1" container output="" run_status=0

    if ! container="$(docker create \
            --platform "${OCI_PLATFORM}" \
            --entrypoint /bin/sh \
            "${DIST_BASE_IMAGE}" \
            -c 'ldd /tmp/postvec-inspect')"; then
        problem "could not create the ${DIST_ID} dependency-inspection container"
        return 0
    fi

    if ! docker cp "${path}" "${container}:/tmp/postvec-inspect"; then
        docker rm --force "${container}" >/dev/null 2>&1 || true
        problem "could not copy the artifact into the ${DIST_ID} dependency-inspection container"
        return 0
    fi

    if ! output="$(docker start --attach "${container}" 2>&1)"; then
        run_status=1
    fi
    docker rm --force "${container}" >/dev/null 2>&1 || true

    if (( run_status )) || grep -q 'not found' <<<"${output}"; then
        printf '%s\n' "${output}" >&2
        problem "unresolved shared library dependencies in the ${DIST_ID} target"
    else
        pass "all shared libraries resolve in the ${DIST_ID} target"
    fi
}

inspect() {
    local path="$1" kind="$2"
    [[ -f "${path}" ]] || die "no such file: ${path}"
    printf '\n\033[1m%s\033[0m (%s)\n' "${path##*/}" "${kind}"

    # Architecture must match what the package will claim.
    local machine
    machine="$(readelf --file-header "${path}" | awk -F: '/Machine:/ {gsub(/^ +/,"",$2); print $2}')"
    if [[ "${machine}" == "${ELF_MACHINE}" ]]; then
        pass "machine ${machine}"
    else
        problem "machine is '${machine}', expected '${ELF_MACHINE}' for ${RELEASE_ARCH}"
    fi

    # A shared library that is not PIC, or an executable stack, is a packaging
    # bug serious enough to stop a release.
    if readelf --program-headers "${path}" | grep -E 'GNU_STACK.*RWE' >/dev/null; then
        problem "executable stack"
    else
        pass "no executable stack"
    fi
    if [[ "${kind}" == library ]] && ! file "${path}" | grep 'shared object' >/dev/null; then
        problem "not a shared object"
    fi

    # RPATH/RUNPATH pointing at a build tree is how a binary silently works on
    # the builder and nowhere else.
    local runpath
    runpath="$(objdump -p "${path}" | awk '/R(UN)?PATH/ {print $2}' | tr '\n' ' ')"
    if [[ -z "${runpath// /}" ]]; then
        pass "no RPATH/RUNPATH"
    elif [[ "${runpath}" == *'$ORIGIN'* && "${runpath}" != */src/* && "${runpath}" != */home/* ]]; then
        pass "relative RUNPATH (${runpath% })"
    else
        problem "build-path RPATH/RUNPATH: ${runpath% }"
    fi

    # ONNX Runtime must be dlopen()ed, never linked.
    if readelf -d "${path}" | grep -E 'NEEDED.*libonnxruntime' >/dev/null; then
        problem "links libonnxruntime at build time (it must be dlopen()ed from the engine root)"
    else
        pass "no link-time ONNX Runtime dependency"
    fi

    printf '  needed: '
    readelf -d "${path}" | awk -F'[][]' '/NEEDED/ {printf "%s ", $2}'
    printf '\n'

    # Highest required glibc symbol version versus the target's floor.
    local highest floor
    highest="$(readelf --version-info "${path}" 2>/dev/null \
        | grep -oE 'GLIBC_[0-9]+\.[0-9]+(\.[0-9]+)?' | sed 's/GLIBC_//' | sort -V | tail -1)"
    floor="$(glibc_floor_for "${DIST_ID:-}")"
    if [[ -z "${highest}" ]]; then
        pass "no versioned glibc symbols"
    elif [[ -z "${floor}" ]]; then
        pass "requires glibc ${highest} (no floor known for '${DIST_ID:-unset}')"
    elif version_le "${highest}" "${floor}"; then
        pass "requires glibc ${highest} ≤ ${DIST_ID} floor ${floor}"
    else
        problem "requires glibc ${highest}, above the ${DIST_ID} floor ${floor}"
    fi

    # A target distribution was supplied by the release build, so resolve in
    # that pinned target image. A standalone inspection without --distro may
    # use host ldd, but only when the architecture matches.
    if [[ -n "${DISTRO}" ]]; then
        inspect_target_dependencies "${path}"
    elif command -v ldd >/dev/null 2>&1 && [[ "${RELEASE_ARCH}" == "$(host_release_arch)" ]]; then
        if ldd "${path}" 2>/dev/null | grep 'not found' >/dev/null; then
            ldd "${path}" | grep 'not found' >&2
            problem "unresolved shared library dependencies"
        else
            pass "all shared libraries resolve on this host"
        fi
    fi

    # The PostgreSQL magic block is what makes a .so loadable as an extension;
    # its absence is a build that produced a plain library.
    if [[ "${kind}" == library ]]; then
        if readelf --dyn-syms "${path}" | grep -E 'Pg_magic_func|_PG_init' >/dev/null; then
            pass "exports Pg_magic_func / _PG_init"
        else
            problem "no PostgreSQL magic block — this is not a loadable extension"
        fi
    fi
}

[[ -n "${LIBRARY}" || -n "${BINARY}" ]] || die "nothing to inspect: pass --library and/or --binary"
if [[ -n "${LIBRARY}" ]]; then inspect "${LIBRARY}" library; fi
if [[ -n "${BINARY}"  ]]; then inspect "${BINARY}"  executable; fi

echo
if (( fail )); then die "ELF inspection failed"; fi
log "ELF inspection passed"
