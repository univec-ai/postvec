#!/usr/bin/env bash
# Turn the pinned upstream ONNX Runtime release into a packageable payload.
#
#   build-onnxruntime-bundle.sh --arch amd64
#
# Writes build/payload-<arch>/opt/postvec/libs/onnxruntime/ in the
# layout engine::initialize_onnx() expects, plus the licence, third-party
# notices and a SOURCE.json recording exactly where the bytes came from.
#
# Only the runtime is kept: headers, CMake files and pkg-config belong to a
# build environment, not to a database host.

source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

RELEASE_ARCH=""
while (($#)); do
    case "$1" in
    --arch) RELEASE_ARCH="$2"; shift 2 ;;
    -h|--help) sed -n '2,12p' "$0"; exit 0 ;;
    *) die "unknown argument: $1" ;;
    esac
done

RELEASE_ARCH="${RELEASE_ARCH:-$(host_release_arch)}"
load_versions
arch_facts "${RELEASE_ARCH}"
need curl tar sha256sum find

PAYLOAD="${PKG_DIR}/build/payload-${RELEASE_ARCH}"
DEST="${PAYLOAD}/opt/postvec/libs/onnxruntime"
DOC="${PAYLOAD}/usr/share/doc/postvec-onnxruntime"
CACHE="${PKG_DIR}/build/downloads"
ARCHIVE_NAME="onnxruntime-linux-${ORT_ARCH}-${ORT_VERSION}.tgz"
ARCHIVE="${CACHE}/${ARCHIVE_NAME}"
URL="${ORT_URL_BASE}/${ARCHIVE_NAME}"

mkdir -p "${CACHE}"

# A cached archive is only reused when it still matches the reviewed digest;
# anything else is re-downloaded rather than trusted.
if [[ -f "${ARCHIVE}" ]] && [[ "$(sha256_of "${ARCHIVE}")" == "${ORT_SHA256}" ]]; then
    log "using cached ${ARCHIVE_NAME}"
else
    fetch_verified "${URL}" "${ARCHIVE}" "${ORT_SHA256}"
fi

rm -rf "${DEST}" "${DOC}"
mkdir -p "${DEST}/lib" "${DOC}"

extracted="$(mktemp -d)"
trap 'rm -rf "${extracted}"' EXIT
tar --extract --gzip --file "${ARCHIVE}" --directory "${extracted}"
top="${extracted}/onnxruntime-linux-${ORT_ARCH}-${ORT_VERSION}"
[[ -d "${top}" ]] || die "unexpected archive layout: no ${top##*/} inside ${ARCHIVE_NAME}"

# The shared objects, with their symlink chain intact: the engine looks for
# `libonnxruntime.so` and the loader follows it to the versioned file.
cp --archive "${top}"/lib/libonnxruntime.so* "${DEST}/lib/"
cp --archive "${top}"/lib/libonnxruntime_providers_shared.so "${DEST}/lib/" 2>/dev/null || true
cp "${top}/LICENSE" "${DEST}/LICENSE"
cp "${top}/ThirdPartyNotices.txt" "${DEST}/ThirdPartyNotices.txt"
[[ -f "${top}/VERSION_NUMBER" ]] && cp "${top}/VERSION_NUMBER" "${DEST}/VERSION_NUMBER"

[[ -e "${DEST}/lib/libonnxruntime.so" ]] \
    || die "no libonnxruntime.so in the payload — the engine would not find the runtime"

cp "${DEST}/LICENSE" "${DOC}/LICENSE"
cp "${DEST}/ThirdPartyNotices.txt" "${DOC}/ThirdPartyNotices.txt"

# Provenance, next to the bytes. `files` records what was kept so a later audit
# can tell a pruned bundle from a corrupted one.
cat > "${DOC}/SOURCE.json" <<JSON
{
  "component": "onnxruntime",
  "version": "${ORT_VERSION}",
  "execution_providers": ["cpu"],
  "upstream_url": "${URL}",
  "archive_sha256": "${ORT_SHA256}",
  "architecture": "${RELEASE_ARCH}",
  "license": "MIT",
  "installed_root": "/opt/postvec/libs/onnxruntime",
  "pruned": ["include/", "lib/cmake/", "lib/pkgconfig/"],
  "files": {
$(manifest_of_tree "${DEST}" | awk '{printf "    \"%s\": \"%s\",\n", $2, $1}' | sed '$ s/,$//')
  }
}
JSON

# The payload is package-manager-owned and read-only: PostgreSQL only needs to
# read it, and nothing in the running system should be able to replace a
# runtime library in place. `normalize_tree` also pins mtimes for
# reproducibility — but the .so must stay executable-bit-free and yet loadable,
# which is fine: dlopen needs read, not execute.
normalize_tree "${PAYLOAD}"

log "ONNX Runtime ${ORT_VERSION} (${RELEASE_ARCH}) payload ready"
du -sh "${DEST}" | sed 's/^/  /'
find "${DEST}" -type f -o -type l | sort | sed 's|^|  |'
