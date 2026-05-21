#!/usr/bin/env bash
# Assemble the postvec-server image for one architecture from already-built
# Debian 12 packages.
#
#   build-server-image.sh [--arch amd64] [--repository <repo>] [--load]
#
# postvec-server is the inference node postvec's *remote* mode dials. Like the
# database images, this is a composition step, not a build step: it installs
# the exact postvec-server, postvec-cli, postvec-onnxruntime and model .deb
# files this commit produced, so the image a fleet runs is byte-for-byte the
# packages a host installs — same binary, same runtime, same model, same
# licence file — and the two are tested by the same install.
#
# It used to compile the node from source inside a builder stage, as a stand-in
# for the remote smoke test. That produced a second, unverified copy of the
# binary that no package carried and no manifest recorded. Now the image is
# published: the release manifest records it under `variant: server` in
# SERVER_IMAGE_REPOSITORY, tagged `<release id>` with a `latest` moving tag.
#
# The remote-mode smoke test still uses it — `--server-image` on
# tests/image-smoke-test.sh — and tests/server-image-test.sh exercises the
# image on its own.

source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

RELEASE_ARCH=""; REPOSITORY=""; OUTPUT=--load; EXTRA_TAGS=()
while (($#)); do
    case "$1" in
    --arch)       RELEASE_ARCH="$2"; shift 2 ;;
    # The repository the versioned tag is built under. Defaults to
    # SERVER_IMAGE_REPOSITORY; a disposable publication rehearsal passes its
    # throwaway namespace so the image it tests is the image it pushes.
    --repository) REPOSITORY="$2"; shift 2 ;;
    --tag)        EXTRA_TAGS+=("$2"); shift 2 ;;
    --load)       OUTPUT=--load; shift ;;
    --push)       OUTPUT=--push; shift ;;
    -h|--help) sed -n '2,24p' "$0"; exit 0 ;;
    *) die "unknown argument: $1" ;;
    esac
done
RELEASE_ARCH="${RELEASE_ARCH:-$(host_release_arch)}"

load_versions
arch_facts "${RELEASE_ARCH}"
distro_facts debian12
need docker

# The image is built from the Debian 12 packages, which live in two roots: the
# per-architecture common cell (node, CLI, ONNX Runtime) and the
# per-distribution noarch root (model, metapackage).
COMMON_CELL="$(common_dist_dir debian12 "${RELEASE_ARCH}")"
NOARCH_CELL="$(noarch_dist_dir debian12)"
build_first="Build them first:
  scripts/build-onnxruntime-bundle.sh --arch ${RELEASE_ARCH}
  scripts/build-extension-stage.sh --distro debian12 --pg 18 --arch ${RELEASE_ARCH} --with-server
  scripts/build-model-bundle.sh --cli build/debian12-pg18-${RELEASE_ARCH}/cli/postvec
  scripts/build-packages.sh --distro debian12 --pg 18 --arch ${RELEASE_ARCH}"
[[ -d "${COMMON_CELL}" ]] || die "no shared Debian packages at ${COMMON_CELL}. ${build_first}"
[[ -d "${NOARCH_CELL}" ]] || die "no architecture-independent packages at ${NOARCH_CELL}. ${build_first}"

# Release packages only: the globs are anchored on the version that follows
# the name, so `postvec-server-dbgsym_…` and `….deb.spdx.json` stay out.
shopt -s nullglob
server=("${COMMON_CELL}"/postvec-server_[0-9]*.deb)
cli=("${COMMON_CELL}"/postvec-cli_[0-9]*.deb)
runtime=("${COMMON_CELL}"/postvec-onnxruntime_[0-9]*.deb)
model=("${NOARCH_CELL}"/postvec-model-*_[0-9]*.deb)
extras=("${NOARCH_CELL}"/"${EXTRAS_METAPACKAGE}"_[0-9]*.deb)
shopt -u nullglob
for pair in "server:${#server[@]}" "cli:${#cli[@]}" "runtime:${#runtime[@]}" \
            "model:${#model[@]}" "extras:${#extras[@]}"; do
    (( ${pair#*:} == 1 )) || die "expected exactly one ${pair%%:*} package for the server image, found ${pair#*:}
  ${COMMON_CELL}  (postvec-server, postvec-cli, postvec-onnxruntime)
  ${NOARCH_CELL}  (postvec-model-*, ${EXTRAS_METAPACKAGE})
${build_first}"
done

# A narrow context: the packages and the entrypoint, nothing else. An image
# build has no business seeing source code.
CONTEXT="${PKG_DIR}/build/server-image-context-${RELEASE_ARCH}"
rm -rf "${CONTEXT}"
mkdir -p "${CONTEXT}/dist"
cp "${server[@]}" "${cli[@]}" "${runtime[@]}" "${model[@]}" "${extras[@]}" "${CONTEXT}/dist/"
install -m 0755 "${PKG_DIR}/docker/postvec-server-entrypoint.sh" \
    "${CONTEXT}/postvec-server-entrypoint.sh"

REPOSITORY="${REPOSITORY:-${SERVER_IMAGE_REPOSITORY}}"
# Held to the same grammar as the pinned value: this ends up in `docker tag`.
[[ "${REPOSITORY}" =~ ^([a-z0-9]+([.-][a-z0-9]+)*(:[0-9]{1,5})?/)?[a-z0-9]+([._-][a-z0-9]+)*(/[a-z0-9]+([._-][a-z0-9]+)*)*$ ]] \
    || die "--repository is not a plain container repository reference: ${REPOSITORY}"
[[ "${REPOSITORY}" == "${SERVER_IMAGE_REPOSITORY}" ]] \
    || warn "building into ${REPOSITORY}, not the pinned ${SERVER_IMAGE_REPOSITORY}"

# The versioned tag carries the packaging revision, like the database images:
# a packaging-only rebuild is a new tag, never an overwrite. The moving tag
# (`latest`) is advanced by the release job, last, after verification.
TAGS=("${REPOSITORY}:${RELEASE_ID}")
TAGS+=("${EXTRA_TAGS[@]}")
tag_args=()
for tag in "${TAGS[@]}"; do tag_args+=(--tag "${tag}"); done

log "building the postvec-server image (${RELEASE_ARCH}, ${SERVER_LICENSE})"
for tag in "${TAGS[@]}"; do log "  ${tag}"; done
log "  from $(basename "${server[0]}")"

# Pushing from here would bypass the release job's gates. The release pushes
# per-architecture staging images itself; this script builds and loads.
if [[ "${OUTPUT}" == --push ]]; then
    die "build-server-image.sh does not push.
Publishing is the release workflow's job: it pushes the per-architecture image
to the staging repository, tests it natively, and only then creates the
versioned manifest. A push from here would skip all of that."
fi

# `--load`, because this image exists to be `docker run` on this machine.
# Under the `docker-container` driver — what CI provisions — a build with no
# output flag writes to the builder's cache and leaves *nothing* in the local
# image store; the test then fails much later with "pull access denied" for an
# image that was never meant to be pulled.
if docker buildx version >/dev/null 2>&1; then
    build=(docker buildx build --load)
    export DOCKER_BUILDKIT=1
else
    [[ "${RELEASE_ARCH}" == "$(host_release_arch)" ]] \
        || die "building for ${RELEASE_ARCH} on $(host_release_arch) needs docker buildx"
    warn "docker buildx is not installed — using the classic builder (local build only)"
    build=(docker build)
    export DOCKER_BUILDKIT=0
fi

"${build[@]}" \
    --file "${PKG_DIR}/docker/Dockerfile.postvec-server" \
    --platform "${OCI_PLATFORM}" \
    "${tag_args[@]}" \
    --build-arg "BUILD_BASE=${DIST_BASE_IMAGE}" \
    --build-arg "POSTVEC_VERSION=${POSTVEC_VERSION}" \
    --build-arg "RELEASE_ID=${RELEASE_ID}" \
    --build-arg "SERVER_LICENSE=${SERVER_LICENSE}" \
    --build-arg "IMAGE_SOURCE=${SOURCE_REPOSITORY}" \
    --build-arg "GIT_REVISION=$(git -C "${REPO_ROOT}" rev-parse HEAD)" \
    --build-arg "BUILD_DATE=$(date -u -d "@${SOURCE_DATE_EPOCH}" +%Y-%m-%dT%H:%M:%SZ)" \
    --build-arg "SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH}" \
    "${CONTEXT}"

rm -rf "${CONTEXT}"

# A build that reported success but left no image is a real failure mode —
# see the `--load` note above. Assert the postcondition, so whatever the
# builder or its flags do, the failure names itself here.
docker image inspect "${TAGS[0]}" >/dev/null 2>&1 || die "the build succeeded but ${TAGS[0]} is not in the local image store.
A buildx build with no --load writes to the builder's cache and loads nothing;
check the output flag above and the driver in use (docker buildx ls)."

log "postvec-server image ready: ${TAGS[0]}"
