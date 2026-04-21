#!/usr/bin/env bash
# Build a postvec-server container image from this commit.
#
#   build-server-image.sh [--arch amd64]
#
# postvec-server is the inference node postvec's *remote* mode dials. This
# image carries the same ONNX Runtime and the same bundled model the complete
# image runs in-process, so a container of each loads byte-identical model
# assets — which is what makes a passing remote test and a passing embedded
# test mean the same thing about the model.
#
# The remote-mode smoke test uses it. Without a reachable engine that path is
# only ever exercised in its degraded state — worker beating, jobs queueing,
# search falling back to full text — which proves the failure mode works and
# says nothing about the success one.
#
# This replaces the old fixture builder, which built a stand-in
# (`fixtures/inference-server`) because no real server existed yet. The test
# now runs against the artifact rather than an imitation of it.
#
# The image is local. Nothing publishes it and no release manifest references
# it; the tag is `postvec-server:<arch>`.

source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

RELEASE_ARCH=""
while (($#)); do
    case "$1" in
    --arch) RELEASE_ARCH="$2"; shift 2 ;;
    -h|--help) sed -n '2,22p' "$0"; exit 0 ;;
    *) die "unknown argument: $1" ;;
    esac
done
RELEASE_ARCH="${RELEASE_ARCH:-$(host_release_arch)}"

load_versions
arch_facts "${RELEASE_ARCH}"
distro_facts debian12
need docker

ORT_PAYLOAD="${PKG_DIR}/build/payload-${RELEASE_ARCH}/opt/postvec"
MODEL_PAYLOAD="${PKG_DIR}/build/payload-common/opt/postvec"
[[ -d "${ORT_PAYLOAD}/libs" ]] \
    || die "no ONNX Runtime payload; run scripts/build-onnxruntime-bundle.sh --arch ${RELEASE_ARCH}"
[[ -d "${MODEL_PAYLOAD}/models" ]] \
    || die "no model payload; run scripts/build-model-bundle.sh"

CONTEXT="${PKG_DIR}/build/postvec-server-${RELEASE_ARCH}"
rm -rf "${CONTEXT}"
mkdir -p "${CONTEXT}/src/payload"

# The engine root: exactly the reviewed payloads, so this image and the
# complete image load byte-identical model assets.
cp -a "${ORT_PAYLOAD}/libs"     "${CONTEXT}/src/payload/"
cp -a "${MODEL_PAYLOAD}/models" "${CONTEXT}/src/payload/"

# Stage the workspace sources explicitly: cargo parses every workspace
# member's manifest, so all members travel, but the extension crate
# (workspace-excluded) and any build output do not.
for path in Cargo.toml Cargo.lock proto engine shared \
            postvec-cli registry providers postvec-server; do
    cp -a "${REPO_ROOT}/${path}" "${CONTEXT}/src/${path}"
done

IMAGE="postvec-server:${RELEASE_ARCH}"
log "building postvec-server (${RELEASE_ARCH})"

cp "${PKG_DIR}/docker/Dockerfile.postvec-server" "${CONTEXT}/src/Dockerfile"
install -m 0755 "${PKG_DIR}/docker/postvec-server-entrypoint.sh" \
    "${CONTEXT}/src/postvec-server-entrypoint.sh"

# `--load`, because this image exists to be `docker run` on this machine.
# Under the `docker-container` driver — which is what `docker/setup-buildx-action`
# provisions, and therefore what CI uses — a build with no output flag writes
# to the builder's cache and leaves *nothing* in the local image store. The
# build then reports success and the test fails much later with "pull access
# denied" for an image that was never meant to be pulled. (Locally, where the
# plugin is often absent, the classic-builder fallback below loads the image
# implicitly, so this divergence only ever shows up in CI.) With the plain
# `docker` driver `--load` is what already happens, so it is safe either way.
build=(docker buildx build --load)
docker buildx version >/dev/null 2>&1 || {
    warn "docker buildx is not installed — using the classic builder"
    "${PKG_DIR}/scripts/strip-cache-mounts.py" \
        "${CONTEXT}/src/Dockerfile" "${CONTEXT}/src/Dockerfile.nocache"
    mv "${CONTEXT}/src/Dockerfile.nocache" "${CONTEXT}/src/Dockerfile"
    build=(docker build)
    export DOCKER_BUILDKIT=0
}

"${build[@]}" \
    --file "${CONTEXT}/src/Dockerfile" \
    --target server \
    --tag "${IMAGE}" \
    --build-arg "BUILD_BASE=${DIST_BASE_IMAGE}" \
    --build-arg "RUST_VERSION=${RUST_VERSION}" \
    --build-arg "RUSTUP_VERSION=${RUSTUP_VERSION}" \
    --build-arg "RUSTUP_INIT_SHA256=${RUSTUP_INIT_SHA256}" \
    --build-arg "RUST_TRIPLE=${RUST_TRIPLE}" \
    --build-arg "PROTOC_VERSION=${PROTOC_VERSION}" \
    --build-arg "PROTOC_ARCH=${PROTOC_ARCH}" \
    --build-arg "PROTOC_SHA256=${PROTOC_SHA256}" \
    "${CONTEXT}/src"

rm -rf "${CONTEXT}/src"

# A build that reported success but left no image is a real failure mode: the
# smoke test then tries to `docker run` a tag that does not exist, Docker
# treats it as a registry reference, and the run dies with "pull access denied
# for postvec-server" — an error about permissions on an image nobody ever
# intended to publish. Assert the postcondition instead, so whatever the
# builder or its flags do, the failure names itself here.
docker image inspect "${IMAGE}" >/dev/null 2>&1 || die "the build succeeded but ${IMAGE} is not in the local image store.
A buildx build with no --load writes to the builder's cache and loads nothing;
check the output flag above and the driver in use (docker buildx ls)."

log "postvec-server image ready: ${IMAGE}"
