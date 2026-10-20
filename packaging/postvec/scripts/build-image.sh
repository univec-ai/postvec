#!/usr/bin/env bash
# Assemble a postvec image for one PostgreSQL major and one architecture from
# already-built packages.
#
#   build-image.sh --pg 18 --variant local [--arch amd64] [--load|--push]
#
# The image installs the exact .deb files this commit produced. If they are
# not there, that is the error. It must never compile a second copy.

source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

PG_MAJOR=""; VARIANT=local; RELEASE_ARCH=""; OUTPUT=--load; EXTRA_TAGS=()
REPOSITORY=""
while (($#)); do
    case "$1" in
    --pg)      PG_MAJOR="$2"; shift 2 ;;
    --variant) VARIANT="$2"; shift 2 ;;
    --arch)    RELEASE_ARCH="$2"; shift 2 ;;
    # Repository for the versioned tag. A disposable rehearsal builds into a
    # throwaway namespace so the image it tests is the image it pushes.
    --repository) REPOSITORY="$2"; shift 2 ;;
    --tag)     EXTRA_TAGS+=("$2"); shift 2 ;;
    --load)    OUTPUT=--load; shift ;;
    # Accepted only so the refusal below can name it.
    --push)    OUTPUT=--push; shift ;;
    -h|--help) sed -n '2,8p' "$0"; exit 0 ;;
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
"${REMOTE_IMAGE_VARIANT}"|"${LOCAL_IMAGE_VARIANT}") ;;
complete|embedded)
    die "--variant ${VARIANT} is gone; use --variant ${LOCAL_IMAGE_VARIANT}
The image that includes ONNX Runtime and the bundled model is the local
image (inference in the container). Runtime mode is still
POSTVEC_MODE=embedded / postvec setup --embedded."
    ;;
*) die "--variant must be ${REMOTE_IMAGE_VARIANT} or ${LOCAL_IMAGE_VARIANT}" ;;
esac

# Only the local image carries a model. Facts come from the verified archive,
# not versions.env. The remote image builds without a model.
model_args=()
if [[ "${VARIANT}" == "${LOCAL_IMAGE_VARIANT}" ]]; then
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

# A small context. The image build must not see the source tree.
CONTEXT="${PKG_DIR}/build/image-context-pg${PG_MAJOR}-${RELEASE_ARCH}"
# Debian 12 packages live in three roots: the shared cell, the noarch root
# and the per-major extension cell.
COMMON_CELL="$(common_dist_dir debian12 "${RELEASE_ARCH}")"
EXTENSION_CELL="$(extension_dist_dir debian12 "${PG_MAJOR}" "${RELEASE_ARCH}")"
NOARCH_CELL="$(noarch_dist_dir debian12)"

build_first="Build them first:
  scripts/build-extension-stage.sh --distro debian12 --pg ${PG_MAJOR} --arch ${RELEASE_ARCH}
  scripts/build-packages.sh        --distro debian12 --pg ${PG_MAJOR} --arch ${RELEASE_ARCH}"
[[ -d "${COMMON_CELL}" ]]    || die "no shared Debian packages at ${COMMON_CELL}. ${build_first}"
[[ -d "${EXTENSION_CELL}" ]] || die "no PG ${PG_MAJOR} extension package at ${EXTENSION_CELL}. ${build_first}"

rm -rf "${CONTEXT}"
mkdir -p "${CONTEXT}/dist/packages" "${CONTEXT}/dist/local" \
         "${CONTEXT}/packaging/postvec/docker"

shopt -s nullglob
cli=("${COMMON_CELL}"/postvec-cli_*.deb)
extension=("${EXTENSION_CELL}"/postgresql-"${PG_MAJOR}"-postvec_*.deb)
(( ${#cli[@]} ))       || die "no postvec-cli package in ${COMMON_CELL}. ${build_first}"
(( ${#extension[@]} )) || die "no PG ${PG_MAJOR} extension package in ${EXTENSION_CELL}. ${build_first}"
cp "${cli[@]}" "${extension[@]}" "${CONTEXT}/dist/packages/"

if [[ "${VARIANT}" == "${LOCAL_IMAGE_VARIANT}" ]]; then
    # ONNX Runtime is per architecture. The model and the metapackage are
    # noarch and do not live in an architecture cell.
    extras=(
        "${COMMON_CELL}"/postvec-onnxruntime_*.deb
        "${NOARCH_CELL}"/postvec-model-*.deb
        "${NOARCH_CELL}"/"${EXTRAS_METAPACKAGE}"_*.deb
    )
    (( ${#extras[@]} == 3 )) || die "the local image needs the three extra packages; \
found ${#extras[@]} across
  ${COMMON_CELL}  (postvec-onnxruntime)
  ${NOARCH_CELL}  (postvec-model-*, ${EXTRAS_METAPACKAGE})
Build them first:
  scripts/build-onnxruntime-bundle.sh --arch ${RELEASE_ARCH}
  scripts/build-model-bundle.sh
  scripts/build-packages.sh --distro debian12 --pg ${PG_MAJOR} --arch ${RELEASE_ARCH}"
    cp "${extras[@]}" "${CONTEXT}/dist/local/"
fi
shopt -u nullglob

cp "${PKG_DIR}/docker/postvec-entrypoint.sh" \
   "${PKG_DIR}/docker/postvec-healthcheck.sh" \
   "${PKG_DIR}/docker/20-create-postvec.sh" \
   "${CONTEXT}/packaging/postvec/docker/"

TAG_SUFFIX="${REMOTE_IMAGE_SUFFIX}"
[[ "${VARIANT}" == "${LOCAL_IMAGE_VARIANT}" ]] && TAG_SUFFIX="${LOCAL_IMAGE_SUFFIX}"
# The versioned tag includes the packaging revision (0.1.0-1-pg18-local) so a
# rebuild does not overwrite a tag somebody is running.
# The moving tag is not applied here. The release job advances it last, after
# verification. A local build must not publish an untested moving tag.
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

# buildx when present, for push and cross builds. The classic builder otherwise.
# --push is refused. Publishing belongs to the release workflow.
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
