#!/usr/bin/env bash
# Assemble a postvec image for one PostgreSQL major and one architecture from
# already-built packages.
#
#   build-image.sh --pg 18 --variant complete [--arch amd64] [--load|--push]
#
# The image is a composition step, not a build step: it installs the exact
# .deb files this commit produced. If they are not there, that is the error —
# the image must never quietly compile a second, unverified copy.

source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

PG_MAJOR=""; VARIANT=complete; RELEASE_ARCH=""; OUTPUT=--load; EXTRA_TAGS=()
REPOSITORY=""
while (($#)); do
    case "$1" in
    --pg)      PG_MAJOR="$2"; shift 2 ;;
    --variant) VARIANT="$2"; shift 2 ;;
    --arch)    RELEASE_ARCH="$2"; shift 2 ;;
    # The repository the versioned tag is built under. Defaults to
    # IMAGE_REPOSITORY; named explicitly so the disposable publication rehearsal
    # can build into a throwaway namespace and have the image it *tests* be the
    # image it pushes. Without this the release job would smoke-test a tag that
    # was never created.
    --repository) REPOSITORY="$2"; shift 2 ;;
    --tag)     EXTRA_TAGS+=("$2"); shift 2 ;;
    --load)    OUTPUT=--load; shift ;;
    # Accepted only to give a clear refusal; see below.
    --push)    OUTPUT=--push; shift ;;
    -h|--help) sed -n '2,10p' "$0"; exit 0 ;;
    *) die "unknown argument: $1" ;;
    esac
done

: "${PG_MAJOR:?--pg is required}"
RELEASE_ARCH="${RELEASE_ARCH:-$(host_release_arch)}"

load_versions
require_pg_major "${PG_MAJOR}"
arch_facts "${RELEASE_ARCH}"
need docker
case "${VARIANT}" in
remote|"${COMPLETE_IMAGE_VARIANT}") ;;
embedded)
    die "--variant embedded is gone; use --variant ${COMPLETE_IMAGE_VARIANT}
The image that includes ONNX Runtime and the bundled model is the complete
image (both inference modes), not an in-process-only build. Runtime mode is
still POSTVEC_MODE=embedded / postvec setup --embedded."
    ;;
*) die "--variant must be remote or ${COMPLETE_IMAGE_VARIANT}" ;;
esac

# Only the complete image carries a model, and the facts that describe it are
# derived from the verified registry archive rather than from versions.env.
# The remote image knows nothing about a model and is deliberately buildable
# without one.
model_args=()
if [[ "${VARIANT}" == "${COMPLETE_IMAGE_VARIANT}" ]]; then
    load_model_facts
    model_args=(
        --build-arg "BUNDLED_MODEL_NAME=${MODEL_NAME}"
        --build-arg "BUNDLED_MODEL_BACKEND=${MODEL_BACKEND}"
        --build-arg "BUNDLED_MODEL_REGISTRY_REVISION=${MODEL_REGISTRY_REVISION}"
        --build-arg "BUNDLED_MODEL_PACKAGE_VERSION=${MODEL_PKG_VERSION}"
    )
fi

digest_var="POSTGRES_IMAGE_PG${PG_MAJOR}_DIGEST"
POSTGRES_IMAGE_DIGEST="${!digest_var}"

# The build context is a small staging directory rather than the repository:
# an image build has no business seeing source code, and a narrow context is
# also a fast one.
CONTEXT="${PKG_DIR}/build/image-context-pg${PG_MAJOR}-${RELEASE_ARCH}"
# The images are built from the Debian 12 packages, which live in three roots:
# the shared per-architecture cell, the per-distribution noarch root, and the
# per-major extension cell.
COMMON_CELL="$(common_dist_dir debian12 "${RELEASE_ARCH}")"
EXTENSION_CELL="$(extension_dist_dir debian12 "${PG_MAJOR}" "${RELEASE_ARCH}")"
NOARCH_CELL="$(noarch_dist_dir debian12)"

build_first="Build them first:
  scripts/build-extension-stage.sh --distro debian12 --pg ${PG_MAJOR} --arch ${RELEASE_ARCH}
  scripts/build-packages.sh        --distro debian12 --pg ${PG_MAJOR} --arch ${RELEASE_ARCH}"
[[ -d "${COMMON_CELL}" ]]    || die "no shared Debian packages at ${COMMON_CELL}. ${build_first}"
[[ -d "${EXTENSION_CELL}" ]] || die "no PG ${PG_MAJOR} extension package at ${EXTENSION_CELL}. ${build_first}"

rm -rf "${CONTEXT}"
mkdir -p "${CONTEXT}/dist/packages" "${CONTEXT}/dist/complete" \
         "${CONTEXT}/packaging/postvec/docker"

shopt -s nullglob
cli=("${COMMON_CELL}"/postvec-cli_*.deb)
extension=("${EXTENSION_CELL}"/postgresql-"${PG_MAJOR}"-postvec_*.deb)
(( ${#cli[@]} ))       || die "no postvec-cli package in ${COMMON_CELL}. ${build_first}"
(( ${#extension[@]} )) || die "no PG ${PG_MAJOR} extension package in ${EXTENSION_CELL}. ${build_first}"
cp "${cli[@]}" "${extension[@]}" "${CONTEXT}/dist/packages/"

if [[ "${VARIANT}" == "${COMPLETE_IMAGE_VARIANT}" ]]; then
    # ONNX Runtime is architecture-specific; the model bundle and the
    # metapackage are `all` and live in the noarch root, not in whichever
    # architecture's cell happened to build them.
    extras=(
        "${COMMON_CELL}"/postvec-onnxruntime_*.deb
        "${NOARCH_CELL}"/postvec-model-*.deb
        "${NOARCH_CELL}"/"${EXTRAS_METAPACKAGE}"_*.deb
    )
    (( ${#extras[@]} == 3 )) || die "the complete image needs the three extra packages; \
found ${#extras[@]} across
  ${COMMON_CELL}  (postvec-onnxruntime)
  ${NOARCH_CELL}  (postvec-model-*, ${EXTRAS_METAPACKAGE})
Build them first:
  scripts/build-onnxruntime-bundle.sh --arch ${RELEASE_ARCH}
  scripts/build-model-bundle.sh
  scripts/build-packages.sh --distro debian12 --pg ${PG_MAJOR} --arch ${RELEASE_ARCH}"
    cp "${extras[@]}" "${CONTEXT}/dist/complete/"
fi
shopt -u nullglob

cp "${PKG_DIR}/docker/postvec-entrypoint.sh" \
   "${PKG_DIR}/docker/postvec-healthcheck.sh" \
   "${PKG_DIR}/docker/20-create-postvec.sh" \
   "${CONTEXT}/packaging/postvec/docker/"

TAG_SUFFIX=""
[[ "${VARIANT}" == "${COMPLETE_IMAGE_VARIANT}" ]] && TAG_SUFFIX="${COMPLETE_IMAGE_SUFFIX}"
# The versioned tag carries the packaging revision (0.1.0-1-pg18), so a
# packaging-only rebuild produces a new tag rather than overwriting one
# somebody is already running.
#
# The moving tag (`pg18`) is deliberately *not* applied here. It is advanced by
# the release job, last, after the release has been verified — a local build
# that tagged it could publish a moving tag for an image nothing has tested.
REPOSITORY="${REPOSITORY:-${IMAGE_REPOSITORY}}"
# Held to the same grammar as the pinned value: this ends up in `docker tag`.
[[ "${REPOSITORY}" =~ ^([a-z0-9]+([.-][a-z0-9]+)*(:[0-9]{1,5})?/)?[a-z0-9]+([._-][a-z0-9]+)*(/[a-z0-9]+([._-][a-z0-9]+)*)*$ ]] \
    || die "--repository is not a plain container repository reference: ${REPOSITORY}"
[[ "${REPOSITORY}" == "${IMAGE_REPOSITORY}" ]] \
    || warn "building into ${REPOSITORY}, not the pinned ${IMAGE_REPOSITORY}"

TAGS=("${REPOSITORY}:${RELEASE_ID}-pg${PG_MAJOR}${TAG_SUFFIX}")
TAGS+=("${EXTRA_TAGS[@]}")

tag_args=()
for tag in "${TAGS[@]}"; do tag_args+=(--tag "${tag}"); done

log "building the ${VARIANT} image for PG ${PG_MAJOR} (${RELEASE_ARCH})"
for tag in "${TAGS[@]}"; do log "  ${tag}"; done

# buildx where it exists (it is what a release runner has, and the only way to
# push or to build for another platform); the classic builder otherwise, so a
# developer without the plugin can still build and test an image locally. The
# image Dockerfile is deliberately plain enough for both.
# Pushing from here would bypass the release job's gates entirely — the
# preflight, the native per-architecture tests, the draft-and-verify sequence.
# The release pushes staging images itself; this script builds and loads.
if [[ "${OUTPUT}" == --push ]]; then
    die "build-image.sh does not push.
Publishing is the release workflow's job: it pushes per-architecture images to
the staging repository, tests them natively, and only then creates the
versioned manifests. A push from here would skip all of that."
fi

if docker buildx version >/dev/null 2>&1; then
    build=(docker buildx build "${OUTPUT}")
    export DOCKER_BUILDKIT=1
else
    [[ "${RELEASE_ARCH}" == "$(host_release_arch)" ]] \
        || die "building for ${RELEASE_ARCH} on $(host_release_arch) needs docker buildx"
    warn "docker buildx is not installed — using the classic builder (local build only)"
    build=(docker build)
    export DOCKER_BUILDKIT=0
fi

"${build[@]}" \
    --file "${PKG_DIR}/docker/Dockerfile" \
    --target "${VARIANT}" \
    --platform "${OCI_PLATFORM}" \
    "${tag_args[@]}" \
    --build-arg "PG_MAJOR=${PG_MAJOR}" \
    --build-arg "POSTGRES_IMAGE_DIGEST=${POSTGRES_IMAGE_DIGEST}" \
    --build-arg "POSTVEC_VERSION=${POSTVEC_VERSION}" \
    "${model_args[@]}" \
    --build-arg "ORT_VERSION=${ORT_VERSION}" \
    --build-arg "IMAGE_SOURCE=${SOURCE_REPOSITORY}" \
    --build-arg "GIT_REVISION=$(git -C "${REPO_ROOT}" rev-parse HEAD)" \
    --build-arg "BUILD_DATE=$(date -u -d "@${SOURCE_DATE_EPOCH}" +%Y-%m-%dT%H:%M:%SZ)" \
    --build-arg "SOURCE_DATE_EPOCH=${SOURCE_DATE_EPOCH}" \
    "${CONTEXT}"

log "image ready: ${TAGS[0]}"
