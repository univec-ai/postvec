#!/usr/bin/env bash
# Compile one release cell (distribution × PostgreSQL major × architecture) in
# a hermetic builder container and lay the result out for packaging.
#
#   build-extension-stage.sh --distro debian12 --pg 18 --arch amd64
#
# Produces build/<distro>-pg<major>-<arch>/ containing:
#   stage/        the pg_config-rooted extension tree, library stripped
#   cli/postvec   the CLI binary, stripped
#   debug/        the split debug objects (a separate -dbgsym/-debuginfo package)
#   build-info.txt what the builder actually used
#
# The container never writes outside the exported directory, and the export is
# a scratch stage, so no toolchain or cargo cache can reach an artifact.

source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

DISTRO=""; PG_MAJOR=""; RELEASE_ARCH=""
while (($#)); do
    case "$1" in
    --distro) DISTRO="$2"; shift 2 ;;
    --pg)     PG_MAJOR="$2"; shift 2 ;;
    --arch)   RELEASE_ARCH="$2"; shift 2 ;;
    -h|--help) sed -n '2,20p' "$0"; exit 0 ;;
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

# A pgrx extension is loaded into a live PostgreSQL process; emulated release
# builds are a false economy. Cross-architecture builds are allowed only when
# explicitly acknowledged, and never silently.
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
    # Without BuildKit there are no cache mounts and no `--output`. A release
    # always has buildx (a Rust build without a Cargo cache is unbearable), but
    # "you cannot build this at all" is a poor answer to a missing plugin — so
    # the mounts are stripped and the export stage is copied out of a container
    # instead. Slower; identical output.
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

# ---------------------------------------------------------------- sanity check

mapfile -t STAGED_SO < <(find "${OUT}/stage" -name postvec.so -type f | LC_ALL=C sort)
(( ${#STAGED_SO[@]} )) || die "builder produced no postvec.so"
# Exactly one, always: two would mean the build inherited another
# distribution's staging tree from the context.
(( ${#STAGED_SO[@]} == 1 )) || die "the builder produced ${#STAGED_SO[@]} copies of postvec.so:
$(printf '  %s\n' "${STAGED_SO[@]}")
This means build output leaked in from the build context."
SO="${STAGED_SO[0]}"
CONTROL="$(find "${OUT}/stage" -name postvec.control -type f | head -1)"
[[ -n "${CONTROL}" ]] || die "builder produced no postvec.control"
mapfile -t SQL < <(find "${OUT}/stage" -name 'postvec--*.sql' -type f)
((${#SQL[@]})) || die "builder produced no extension SQL"
[[ -f "${OUT}/cli/postvec" ]] || die "builder produced no CLI binary"

# cargo-pgrx substitutes @CARGO_VERSION@ at package time; if that did not
# happen, CREATE EXTENSION would fail on the user's machine with a confusing
# "extension has no installation script" long after this build succeeded.
grep -q "default_version = '${POSTVEC_VERSION}'" "${CONTROL}" \
    || die "control file version is not ${POSTVEC_VERSION}: $(grep default_version "${CONTROL}")"

EXTENSION_DIR="$(dirname "${CONTROL}")"
[[ -f "${EXTENSION_DIR}/postvec--${POSTVEC_VERSION}.sql" ]] \
    || die "no install script postvec--${POSTVEC_VERSION}.sql in ${EXTENSION_DIR}"

# ------------------------------------------------------------- split the debug

# The release profile asks for line tables (`debug = "line-tables-only"` in
# postvec/Cargo.toml), so a release build does have something to split: enough
# for a symbolised backtrace with file and line, without the tens of megabytes
# of type information a full `-g` build would ship. The split object becomes
# the -dbgsym/-debuginfo package, which a publishable release requires.
#
# The "nothing to split" branch below is still reachable — a build with the
# debug profile overridden, say — and says so rather than producing an empty
# package.
mkdir -p "${OUT}/debug"
split_debug() {
    local binary="$1" name="$2" debug="${OUT}/debug/${2}.debug"
    if ! objcopy --only-keep-debug "${binary}" "${debug}" 2>/dev/null; then
        warn "${name}: no separable debug information"
        return 0
    fi
    # Any `.debug_*` section, not `.debug_info` specifically: the release
    # profile asks for line tables only, which produce `.debug_line` and no
    # `.debug_info` at all. Checking for the latter is how a debug package
    # comes to be silently never built.
    #
    # No `grep -q` on a pipe under `set -o pipefail`: -q exits at the first
    # match, readelf dies of SIGPIPE, and the pipeline reports failure.
    if [[ ! -s "${debug}" ]] || ! readelf --sections "${debug}" 2>/dev/null | grep '\.debug_' >/dev/null; then
        rm -f "${debug}"
        log "${name}: nothing to split (this build carries no debug information)"
    else
        objcopy --add-gnu-debuglink="${debug}" "${binary}"
        log "${name}: split $(du -h "${debug}" | cut -f1) of debug information"
    fi
    strip --strip-unneeded "${binary}"

    # A detached debug file is only useful if the loader can find it *and* it
    # belongs to this binary. Both halves are asserted, because a mismatched
    # pair looks exactly like a working one until someone needs a backtrace.
    if [[ -f "${debug}" ]]; then
        readelf -SW "${binary}" 2>/dev/null | grep '\.gnu_debuglink' >/dev/null \
            || die "${name}: the stripped binary carries no .gnu_debuglink"
        readelf -SW "${binary}" 2>/dev/null | grep -E '\.debug_(line|info)' >/dev/null \
            && die "${name}: the shipped binary still carries debug information"
        readelf -SW "${debug}" 2>/dev/null | grep -E '\.debug_(line|info)' >/dev/null \
            || die "${name}: the detached file carries no debug information"

        # The debuglink is a file name *and* a CRC32 of the file it names, and
        # gdb refuses a debug file whose CRC does not match. objcopy computes
        # it correctly; what this catches is the debug file being rewritten
        # afterwards — by a later strip, a normalisation pass, or a packaging
        # step that thought it was being helpful.
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

normalize_tree "${OUT}/stage"
chmod 0755 "${OUT}/cli/postvec"

# ---------------------------------------------------------------- the ELF gate

"${PKG_DIR}/scripts/inspect-elf.sh" --arch "${RELEASE_ARCH}" --distro "${DISTRO}" \
    --library "${SO}" --binary "${OUT}/cli/postvec"

log "staged ${CELL}"
find "${OUT}" -type f -printf '  %-72p %s bytes\n' | sort
