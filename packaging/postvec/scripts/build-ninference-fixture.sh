#!/usr/bin/env bash
# Build the inference test fixture: a real inference node serving the same
# engine and the same bundled model the embedded image runs in-process.
#
#   build-ninference-fixture.sh [--arch amd64]
#
# It exists so the *remote* image can be tested against something that actually
# answers. Without it, the remote path is only ever exercised in its degraded
# state — worker beating, jobs queueing, search falling back to full text —
# which proves the failure mode works and says nothing about the success one.
#
# The fixture binary is `fixtures/inference-server` in this repository: the
# engine plus the gRPC handler bodies and error taxonomy lifted from postvec's
# embedded loopback server, compiled against the canonical proto/ contract.
#
# This is a test fixture. It is never published, never referenced by a release
# manifest, and carries no compatibility promise.

source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

RELEASE_ARCH=""
while (($#)); do
    case "$1" in
    --arch) RELEASE_ARCH="$2"; shift 2 ;;
    -h|--help) sed -n '2,17p' "$0"; exit 0 ;;
    *) die "unknown argument: $1" ;;
    esac
done
RELEASE_ARCH="${RELEASE_ARCH:-$(host_release_arch)}"

load_versions
arch_facts "${RELEASE_ARCH}"
distro_facts debian12
need docker

ORT_PAYLOAD="${PKG_DIR}/build/payload-${RELEASE_ARCH}/opt/postvec/ninference"
MODEL_PAYLOAD="${PKG_DIR}/build/payload-common/opt/postvec/ninference"
[[ -d "${ORT_PAYLOAD}/libs" ]] \
    || die "no ONNX Runtime payload; run scripts/build-onnxruntime-bundle.sh --arch ${RELEASE_ARCH}"
[[ -d "${MODEL_PAYLOAD}/models" ]] \
    || die "no model payload; run scripts/build-model-bundle.sh"

CONTEXT="${PKG_DIR}/build/ninference-fixture-${RELEASE_ARCH}"
rm -rf "${CONTEXT}"
mkdir -p "${CONTEXT}/src/payload"

# The engine root: exactly the reviewed payloads, so the fixture and the
# embedded image load byte-identical model assets.
cp -a "${ORT_PAYLOAD}/libs"     "${CONTEXT}/src/payload/"
cp -a "${MODEL_PAYLOAD}/models" "${CONTEXT}/src/payload/"

# The fixture is built from this repository, so it speaks exactly the protocol
# this commit's postvec was compiled against — the proto-drift test guarantees
# the vendored and canonical copies agree, and building from the same tree
# keeps that guarantee true. Stage the workspace sources explicitly: cargo
# parses every workspace member's manifest, so all members travel, but the
# extension crate (workspace-excluded) and any build output do not.
for path in Cargo.toml Cargo.lock proto engine shared \
            postvec-cli registry fixtures; do
    cp -a "${REPO_ROOT}/${path}" "${CONTEXT}/src/${path}"
done

IMAGE="postvec-ninference-fixture:${RELEASE_ARCH}"
log "building the inference fixture (${RELEASE_ARCH})"

cp "${PKG_DIR}/docker/Dockerfile.ninference-fixture" "${CONTEXT}/src/Dockerfile"

# `--load`, because the fixture exists only to be `docker run` by the smoke
# test on this machine. Under the `docker-container` driver — which is what
# `docker/setup-buildx-action` provisions, and therefore what CI uses — a build
# with no output flag writes to the builder's cache and leaves *nothing* in the
# local image store. The build then reports success and the test fails much
# later with "pull access denied" for an image that was never meant to be
# pulled. (Locally, where the plugin is often absent, the classic-builder
# fallback below loads the image implicitly, so this divergence only ever shows
# up in CI.) With the plain `docker` driver `--load` is what already happens, so
# it is safe either way.
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
    --target fixture \
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

# A build that reported success but left no image is the failure this fixture
# had: the smoke test then tries to `docker run` a tag that does not exist,
# Docker treats it as a registry reference, and the run dies with "pull access
# denied for postvec-ninference-fixture" — an error about permissions on an
# image nobody ever intended to publish. Assert the postcondition instead, so
# whatever the builder or its flags do, the failure names itself here.
docker image inspect "${IMAGE}" >/dev/null 2>&1 || die "the build succeeded but ${IMAGE} is not in the local image store.
A buildx build with no --load writes to the builder's cache and loads nothing;
check the output flag above and the driver in use (docker buildx ls)."

log "fixture ready: ${IMAGE}"
