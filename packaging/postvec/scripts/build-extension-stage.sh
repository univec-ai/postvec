#!/usr/bin/env bash
# Compile one release cell (distribution, PostgreSQL major and architecture)
# in a hermetic builder and stage it for packaging.
#
#   build-extension-stage.sh --distro debian12 --pg 18 --arch amd64
#   build-extension-stage.sh --distro debian12 --pg 18 --arch amd64 --with-server
#
# Writes build/<distro>-pg<major>-<arch>/: stage/, cli/postvec, debug/,
# build-info.txt and server/postvec-server when --with-server is set.
#
# Pass --with-server once per distribution and architecture. The node does
# not depend on a PostgreSQL major. The manifest will not accept that
# package twice.
#
# The container writes only the scratch export, so the toolchain and the
# cargo cache cannot reach an artifact.

source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

DISTRO=""; PG_MAJOR=""; RELEASE_ARCH=""; WITH_SERVER=0
while (($#)); do
    case "$1" in
    --distro) DISTRO="$2"; shift 2 ;;
    --pg)     PG_MAJOR="$2"; shift 2 ;;
    --arch)   RELEASE_ARCH="$2"; shift 2 ;;
    --with-server) WITH_SERVER=1; shift ;;
    -h|--help) sed -n '2,16p' "$0"; exit 0 ;;
    *) die "unknown argument: $1" ;;
    esac
done

: "${DISTRO:?--distro is required}"
: "${PG_MAJOR:?--pg is required}"
RELEASE_ARCH="${RELEASE_ARCH:-$(host_release_arch)}"

load_versions
require_pg_major "${PG_MAJOR}"
distro_facts "${DISTRO}"
arch_facts "${RELEASE_ARCH}"
need docker

# A pgrx extension loads into a live PostgreSQL. Cross builds run only when
# POSTVEC_ALLOW_EMULATION=1, never silently.
if [[ "${RELEASE_ARCH}" != "$(host_release_arch)" && "${POSTVEC_ALLOW_EMULATION:-0}" != "1" ]]; then
    die "refusing to build ${RELEASE_ARCH} on a $(host_release_arch) host.
Release artifacts are built on native runners. Set POSTVEC_ALLOW_EMULATION=1
for a local, non-release compile smoke test under QEMU."
fi

CELL="${DISTRO}-pg${PG_MAJOR}-${RELEASE_ARCH}"
OUT="${PKG_DIR}/build/${CELL}"
DOCKERFILE="${PKG_DIR}/builders/Dockerfile.${DIST_FAMILY}"

log "building ${CELL}"
log "  base        ${DIST_BASE_IMAGE}"
log "  rust        ${RUST_VERSION} / cargo-pgrx ${PGRX_VERSION}"
log "  features    pg${PG_MAJOR},embedded"
(( WITH_SERVER )) && log "  server      postvec-server (this is a shared-package cell)"
log "  epoch       ${SOURCE_DATE_EPOCH}"

rm -rf "${OUT}"
mkdir -p "${OUT}"

BUILD_ARGS=(
    --build-arg "BUILD_BASE=${DIST_BASE_IMAGE}"
    --build-arg "PG_MAJOR=${PG_MAJOR}"
    --build-arg "RUST_VERSION=${RUST_VERSION}"
    --build-arg "RUSTUP_VERSION=${RUSTUP_VERSION}"
    --build-arg "RUSTUP_INIT_SHA256=${RUSTUP_INIT_SHA256}"
    --build-arg "RUST_TRIPLE=${RUST_TRIPLE}"
    --build-arg "PGRX_VERSION=${PGRX_VERSION}"
    --build-arg "PROTOC_VERSION=${PROTOC_VERSION}"
    --build-arg "PROTOC_ARCH=${PROTOC_ARCH}"
    --build-arg "PROTOC_SHA256=${PROTOC_SHA256}"
    --build-arg "PGDG_KEY_FINGERPRINT=${PGDG_DEBIAN_KEY_FINGERPRINT}"
    --build-arg "PGDG_EL9_REPO_RPM_SHA256=${PGDG_EL9_REPO_RPM_SHA256}"
    --build-arg "SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH}"
    --build-arg "WITH_SERVER=${WITH_SERVER}"
)

if docker buildx version >/dev/null 2>&1; then
    DOCKER_BUILDKIT=1 docker build \
        --file "${DOCKERFILE}" \
        --platform "${OCI_PLATFORM}" \
        --target export \
        --output "type=local,dest=${OUT}" \
        "${BUILD_ARGS[@]}" \
        "${REPO_ROOT}"
else
    # No BuildKit means no cache mounts and no --output. Strip the mounts and
    # copy the export out. Slower, same bytes. Releases use BuildKit.
    warn "docker buildx is not installed — building without a compiler cache"
    warn "  local fallback only; releases build with BuildKit"
    plain="${OUT}/.Dockerfile.nocache"
    "${PKG_DIR}/scripts/strip-cache-mounts.py" "${DOCKERFILE}" "${plain}"
    image="postvec-builder-${CELL}"
    DOCKER_BUILDKIT=0 docker build \
        --file "${plain}" \
        --target builder \
        --tag "${image}" \
        "${BUILD_ARGS[@]}" \
        "${REPO_ROOT}"
    container="$(docker create "${image}" /bin/true)"
    docker cp "${container}:/out/." "${OUT}/"
    docker rm --force "${container}" >/dev/null
    rm -f "${plain}"
fi

mapfile -t STAGED_SO < <(find "${OUT}/stage" -name postvec.so -type f | LC_ALL=C sort)
(( ${#STAGED_SO[@]} )) || die "builder produced no postvec.so"
# Exactly one. Two means another distribution's staging tree leaked in.
(( ${#STAGED_SO[@]} == 1 )) || die "the builder produced ${#STAGED_SO[@]} copies of postvec.so:
$(printf '  %s\n' "${STAGED_SO[@]}")
This means build output leaked in from the build context."
SO="${STAGED_SO[0]}"
CONTROL="$(find "${OUT}/stage" -name postvec.control -type f | head -1)"
[[ -n "${CONTROL}" ]] || die "builder produced no postvec.control"
mapfile -t SQL < <(find "${OUT}/stage" -name 'postvec--*.sql' -type f)
((${#SQL[@]})) || die "builder produced no extension SQL"
[[ -f "${OUT}/cli/postvec" ]] || die "builder produced no CLI binary"
if (( WITH_SERVER )); then
    [[ -f "${OUT}/server/postvec-server" ]] || die "builder produced no postvec-server binary"
elif [[ -e "${OUT}/server" ]]; then
    # A scratch export has no node unless the builder emits one unconditionally.
    # The manifest would then see one package name 24 times.
    die "the builder emitted a postvec-server binary without --with-server"
fi

# cargo-pgrx must have substituted @CARGO_VERSION@. Otherwise CREATE EXTENSION
# fails later with "extension has no installation script".
grep -q "default_version = '${POSTVEC_VERSION}'" "${CONTROL}" \
    || die "control file version is not ${POSTVEC_VERSION}: $(grep default_version "${CONTROL}")"

EXTENSION_DIR="$(dirname "${CONTROL}")"
[[ -f "${EXTENSION_DIR}/postvec--${POSTVEC_VERSION}.sql" ]] \
    || die "no install script postvec--${POSTVEC_VERSION}.sql in ${EXTENSION_DIR}"

# Every upgrade script in postvec/sql must be in the stage. ALTER EXTENSION
# walks them after apt upgrade, and the worker parks until they are applied.
shopt -s nullglob
for upgrade in "${REPO_ROOT}"/postvec/sql/postvec--*--*.sql; do
    [[ -f "${EXTENSION_DIR}/$(basename "${upgrade}")" ]] \
        || die "upgrade script $(basename "${upgrade}") is missing from the staged extension in ${EXTENSION_DIR}"
done
shopt -u nullglob

# Split the line tables the release profile emits into the debug package.
# An overridden profile has nothing to split. Say so. Do not ship an empty package.
mkdir -p "${OUT}/debug"
split_debug() {
    local binary="$1" name="$2" debug="${OUT}/debug/${2}.debug"
    if ! objcopy --only-keep-debug "${binary}" "${debug}" 2>/dev/null; then
        warn "${name}: no separable debug information"
        return 0
    fi
    # Any .debug_* section. Line tables have .debug_line and no .debug_info.
    # Do not use grep -q on a pipe under pipefail: SIGPIPE looks like failure.
    if [[ ! -s "${debug}" ]] || ! readelf --sections "${debug}" 2>/dev/null | grep '\.debug_' >/dev/null; then
        rm -f "${debug}"
        log "${name}: nothing to split (this build carries no debug information)"
    else
        objcopy --add-gnu-debuglink="${debug}" "${binary}"
        log "${name}: split $(du -h "${debug}" | cut -f1) of debug information"
    fi
    strip --strip-unneeded "${binary}"

    # The stripped binary must link the debug file, and only the debug file
    # may still carry debug sections. A mismatched pair looks fine until a backtrace.
    if [[ -f "${debug}" ]]; then
        readelf -SW "${binary}" 2>/dev/null | grep '\.gnu_debuglink' >/dev/null \
            || die "${name}: the stripped binary carries no .gnu_debuglink"
        readelf -SW "${binary}" 2>/dev/null | grep -E '\.debug_(line|info)' >/dev/null \
            && die "${name}: the shipped binary still carries debug information"
        readelf -SW "${debug}" 2>/dev/null | grep -E '\.debug_(line|info)' >/dev/null \
            || die "${name}: the detached file carries no debug information"

        # The debuglink stores a file name and a CRC32. gdb refuses a mismatch,
        # which is what a later rewrite of the debug file produces.
        objcopy --dump-section=.gnu_debuglink="${TMPDIR:-/tmp}/debuglink.$$" "${binary}" \
            || die "${name}: cannot read back the .gnu_debuglink section"
        python3 - "${TMPDIR:-/tmp}/debuglink.$$" "${debug}" "${name}" <<'PY' \
            || die "${name}: the .gnu_debuglink CRC does not match the detached file"
import pathlib, struct, sys, zlib

link = pathlib.Path(sys.argv[1]).read_bytes()
name = link.split(b"\0", 1)[0].decode()
recorded, = struct.unpack("<I", link[-4:])
debug = pathlib.Path(sys.argv[2])
actual = zlib.crc32(debug.read_bytes()) & 0xFFFFFFFF
if name != debug.name:
    sys.exit("%s: debuglink names %s, not %s" % (sys.argv[3], name, debug.name))
if recorded != actual:
    sys.exit("%s: debuglink CRC %08x != %08x" % (sys.argv[3], recorded, actual))
PY
        rm -f "${TMPDIR:-/tmp}/debuglink.$$"

        # Same build id on both halves: that is what debuginfod and gdb match on.
        local binary_id debug_id
        binary_id="$(readelf -n "${binary}" 2>/dev/null | awk '/Build ID:/ {print $3}')"
        debug_id="$(readelf -n "${debug}" 2>/dev/null | awk '/Build ID:/ {print $3}')"
        if [[ -n "${binary_id}" || -n "${debug_id}" ]]; then
            [[ "${binary_id}" == "${debug_id}" ]] \
                || die "${name}: build id mismatch (${binary_id:-none} vs ${debug_id:-none})"
            log "${name}: build id ${binary_id}"
        fi
    fi
}

need objcopy strip readelf
split_debug "${SO}" postvec.so
split_debug "${OUT}/cli/postvec" postvec
# The node's symbols are a separate package. One postvec-server binary serves
# every PostgreSQL major installed beside it.
(( WITH_SERVER )) && split_debug "${OUT}/server/postvec-server" postvec-server

normalize_tree "${OUT}/stage"
chmod 0755 "${OUT}/cli/postvec"
(( WITH_SERVER )) && chmod 0755 "${OUT}/server/postvec-server"

"${PKG_DIR}/scripts/inspect-elf.sh" --arch "${RELEASE_ARCH}" --distro "${DISTRO}" \
    --library "${SO}" --binary "${OUT}/cli/postvec"
# Same gate for the node. A link-time ONNX Runtime dependency would make a
# provider-only node uninstallable without the runtime.
# libssl.so.3 and libcrypto.so.3 are the node's TLS dependency. The bare base
# image has no libssl, so name them as provided-by-package.
if (( WITH_SERVER )); then
    "${PKG_DIR}/scripts/inspect-elf.sh" --arch "${RELEASE_ARCH}" --distro "${DISTRO}" \
        --binary "${OUT}/server/postvec-server" \
        --provided-by-package libssl.so.3 --provided-by-package libcrypto.so.3
fi

log "staged ${CELL}"
find "${OUT}" -type f -printf '  %-72p %s bytes\n' | sort
