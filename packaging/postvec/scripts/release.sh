#!/usr/bin/env bash
# Build a complete postvec release for this machine's architecture, in order.
#
#   release.sh                              # every PG major, Debian 12, + images
#   release.sh --distro ubuntu2404 --pg 18  # one cell
#   release.sh --all-distros                # everything this architecture can build
#   release.sh --distro debian12 --distro el9
#   release.sh --skip-images
#
# `--distro` is repeatable, and `--all-distros` is every distribution the
# packaging supports. The manifest is written once, at the end, naming every
# distribution built — its closure check is a cross-product over
# (distribution × major × architecture), so a per-distribution manifest would
# reject the other distributions' artifacts sitting beside it in dist/.
#
# This is the local equivalent of the release workflow, and it exists so the
# published pipeline can be rehearsed end to end before a tag is pushed. CI
# runs the same scripts in the same order; nothing here is a shortcut that only
# works on a developer's machine.
#
# It builds only for the host architecture. Release artifacts for the other one
# come from a native runner — a pgrx extension is loaded into a live PostgreSQL
# process, and emulation is not a substitute for testing that.

source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

# Every distribution the packaging supports. Kept here rather than in lib.sh so
# that `distro_facts` stays the single authority on what each one *is*; this is
# only the set worth iterating.
ALL_DISTROS=(debian12 ubuntu2204 ubuntu2404 el9)

DISTROS=(); PG_MAJORS=(); SKIP_IMAGES=0; SKIP_TESTS=0
while (($#)); do
    case "$1" in
    --distro)      DISTROS+=("$2"); shift 2 ;;
    --all-distros) DISTROS+=("${ALL_DISTROS[@]}"); shift ;;
    --pg)          PG_MAJORS+=("$2"); shift 2 ;;
    --skip-images) SKIP_IMAGES=1; shift ;;
    --skip-tests)  SKIP_TESTS=1; shift ;;
    -h|--help)     sed -n '2,24p' "$0"; exit 0 ;;
    *) die "unknown argument: $1" ;;
    esac
done

load_versions
ARCH="$(host_release_arch)"
((${#DISTROS[@]}))   || DISTROS=(debian12)
((${#PG_MAJORS[@]})) || read -r -a PG_MAJORS <<<"$(supported_pg_majors)"

# `--distro debian12 --all-distros` must not build Debian 12 twice: the second
# pass would rebuild artifacts the first already verified, and the manifest
# would see one file name produced by two builds.
read -r -a DISTROS <<<"$(printf '%s\n' "${DISTROS[@]}" | awk '!seen[$0]++' | tr '\n' ' ')"
# Fail on an unknown name here rather than after the first hour of compiling.
for distro in "${DISTROS[@]}"; do distro_facts "${distro}"; done

step() { printf '\n\033[1;35m▸ %s\033[0m\n' "$*" >&2; }

step "release gate"
"${PKG_DIR}/scripts/assert-versions.sh"
# Seconds, and the only thing that exercises the closure checks themselves —
# everything below proves that packages build, not that a bad one would be
# caught. Cheap enough to run before an hour of compilation, so it does.
"${PKG_DIR}/tests/unit-test.sh"

step "engine assets (${ARCH})"
"${PKG_DIR}/scripts/build-onnxruntime-bundle.sh" --arch "${ARCH}"
# The model bundle is deliberately *not* here. It is acquired with
# `postvec model pull`, which needs a postvec binary — and this script must not
# require a Rust toolchain on the host. The first extension stage below builds
# one, so the model step runs immediately after it, once.
MODEL_BUNDLE_DONE=0

for DISTRO in "${DISTROS[@]}"; do
    # Per distribution, because DIST_FAMILY decides the package extension and
    # whether the coexistence test applies.
    distro_facts "${DISTRO}"
    COMMON_DIR="$(common_dist_dir "${DISTRO}" "${ARCH}")"
    NOARCH_DIR="$(noarch_dist_dir "${DISTRO}")"

    # The shared packages come out of one cell and are identical for every
    # major, so they are built and verified once per distribution. The release
    # matrix does the same, and for the same reason: building them per major
    # would produce several files with one name.
    FIRST_MAJOR="${PG_MAJORS[0]}"
    # `--with-server`: the shared cell is the one that compiles the inference
    # node, which is PostgreSQL-independent like the CLI it sits beside.
    step "${DISTRO}: shared packages via PostgreSQL ${FIRST_MAJOR}"
    "${PKG_DIR}/scripts/build-extension-stage.sh" \
        --distro "${DISTRO}" --pg "${FIRST_MAJOR}" --arch "${ARCH}" --with-server

    # The bundled model, once, with the CLI that stage just produced. Do not
    # assume every target distribution's binary runs on every release host:
    # build-model-bundle.sh executes `postvec --version` first and names
    # --cli / POSTVEC_CLI if it does not.
    if (( ! MODEL_BUNDLE_DONE )); then
        step "model bundle (registry)"
        "${PKG_DIR}/scripts/build-model-bundle.sh" \
            --cli "${PKG_DIR}/build/${DISTRO}-pg${FIRST_MAJOR}-${ARCH}/cli/postvec"
        MODEL_BUNDLE_DONE=1
    fi

    "${PKG_DIR}/scripts/build-packages.sh" \
        --distro "${DISTRO}" --pg "${FIRST_MAJOR}" --arch "${ARCH}"
    "${PKG_DIR}/scripts/verify-package.sh" \
        "${COMMON_DIR}"/*."${DIST_FAMILY}" "${NOARCH_DIR}"/*."${DIST_FAMILY}"

    for major in "${PG_MAJORS[@]}"; do
        extension_dir="$(extension_dist_dir "${DISTRO}" "${major}" "${ARCH}")"

        # The first major's extension came out of the shared build above.
        if [[ "${major}" != "${FIRST_MAJOR}" ]]; then
            step "${DISTRO}: PostgreSQL ${major}: compile"
            "${PKG_DIR}/scripts/build-extension-stage.sh" \
                --distro "${DISTRO}" --pg "${major}" --arch "${ARCH}"

            step "${DISTRO}: PostgreSQL ${major}: package"
            "${PKG_DIR}/scripts/build-packages.sh" \
                --distro "${DISTRO}" --pg "${major}" --arch "${ARCH}" --only extension
        fi

        step "${DISTRO}: PostgreSQL ${major}: verify"
        "${PKG_DIR}/scripts/verify-package.sh" "${extension_dir}"/*."${DIST_FAMILY}"
    done

    if (( ! SKIP_TESTS )); then
        for major in "${PG_MAJORS[@]}"; do
            # Minimal first — the dependency boundary a remote-mode user has —
            # then the full install, which starts a cluster and embeds.
            step "${DISTRO}: clean-host install test: PG ${major} (minimal)"
            "${PKG_DIR}/tests/package-install-test.sh" --minimal \
                --distro "${DISTRO}" --pg "${major}" --arch "${ARCH}"
            step "${DISTRO}: clean-host install test: PG ${major} (full)"
            "${PKG_DIR}/tests/package-install-test.sh" \
                --distro "${DISTRO}" --pg "${major}" --arch "${ARCH}"
            # PV-13: the packaged provider gate, on the review-mandated cells
            # only (Debian 12 and EL9, one major). The ordinary matrix above
            # already answers version compatibility; this answers whether an
            # external provider works through the shipped artifacts.
            if [[ "${major}" == 18 && ( "${DISTRO}" == debian12 || "${DISTRO}" == el9 ) ]]; then
                step "${DISTRO}: PV-13 provider gate: PG ${major} (package)"
                "${PKG_DIR}/tests/provider-e2e-test.sh" --target package \
                    --distro "${DISTRO}" --pg "${major}" --arch "${ARCH}"
            fi
        done
        if (( ${#PG_MAJORS[@]} > 1 )) && [[ "${DIST_FAMILY}" == deb ]]; then
            step "${DISTRO}: PostgreSQL majors coexist"
            args=(--minimal --distro "${DISTRO}" --arch "${ARCH}")
            for major in "${PG_MAJORS[@]}"; do args+=(--pg "${major}"); done
            "${PKG_DIR}/tests/package-install-test.sh" "${args[@]}"
        fi
    fi
done

# The images are assembled from the Debian 12 packages, so they are built when
# Debian 12 is part of this run and skipped — with a word about it — when it is
# not, rather than failing on packages that were never asked for.
IMAGES_WANTED=0
for distro in "${DISTROS[@]}"; do
    # `if`, not `[[ … ]] && x=1`: as the last command of a loop body a false
    # test makes the whole `for` return non-zero, and `set -e` ends the run —
    # the same trap that used to kill this script after the first image.
    if [[ "${distro}" == debian12 ]]; then IMAGES_WANTED=1; fi
done
if (( ! SKIP_IMAGES )) && (( ! IMAGES_WANTED )); then
    warn "skipping images: they are built from the Debian 12 packages, which this run did not build"
fi

if (( ! SKIP_IMAGES )) && (( IMAGES_WANTED )); then
    # The inference node's image first: it is composed from the shared and
    # noarch Debian 12 packages built above, it is a published artifact in its
    # own right, and the remote database images are tested against it.
    step "image: postvec-server"
    "${PKG_DIR}/scripts/build-server-image.sh" --arch "${ARCH}" --load
    SERVER_TAG="${SERVER_IMAGE_REPOSITORY}:${RELEASE_ID}"
    if (( ! SKIP_TESTS )); then
        step "image: postvec-server on its own"
        "${PKG_DIR}/tests/server-image-test.sh" "${SERVER_TAG}"
    fi

    for major in "${PG_MAJORS[@]}"; do
        for variant in remote "${COMPLETE_IMAGE_VARIANT}"; do
            step "image: PG ${major} ${variant}"
            "${PKG_DIR}/scripts/build-image.sh" \
                --pg "${major}" --variant "${variant}" --arch "${ARCH}" --load
            if (( ! SKIP_TESTS )); then
                # The suffix is computed as a statement, not inside a command
                # substitution: `x="$( [[ … ]] && echo … )"` takes the exit
                # status of the substitution, so under `set -e` the remote
                # variant — where the test is false — ends the script here,
                # silently, right after the image it just built said "ready".
                tag_suffix=""
                if [[ "${variant}" == "${COMPLETE_IMAGE_VARIANT}" ]]; then
                    tag_suffix="${COMPLETE_IMAGE_SUFFIX}"
                fi
                tag="${IMAGE_REPOSITORY}:${RELEASE_ID}-pg${major}${tag_suffix}"
                if [[ "${variant}" == remote ]]; then
                    # The remote image is only meaningfully tested against a
                    # real engine: the postvec-server image built above, from
                    # this release's own packages.
                    "${PKG_DIR}/tests/image-smoke-test.sh" --variant remote \
                        --server-image "${SERVER_TAG}" "${tag}"
                    if [[ "${major}" == 18 ]]; then
                        step "image: PV-13 provider gate (remote + postvec-server)"
                        "${PKG_DIR}/tests/provider-e2e-test.sh" --target image \
                            --variant remote --arch "${ARCH}" \
                            --server-image "${SERVER_TAG}" "${tag}"
                    fi
                else
                    "${PKG_DIR}/tests/image-smoke-test.sh" \
                        --variant "${COMPLETE_IMAGE_VARIANT}" "${tag}"
                    step "image: PG ${major} golden embeddings"
                    "${PKG_DIR}/tests/model-golden-test.sh" "${tag}"
                    if [[ "${major}" == 18 ]]; then
                        step "image: PV-13 provider gate (complete)"
                        "${PKG_DIR}/tests/provider-e2e-test.sh" --target image \
                            --variant complete --arch "${ARCH}" "${tag}"
                    fi
                fi
            fi
        done
    done
fi

step "release manifest"
majors="$(printf '%s ' "${PG_MAJORS[@]}")"
distros="$(printf '%s ' "${DISTROS[@]}")"
"${PKG_DIR}/scripts/write-release-manifest.sh" \
    --expect-distros "${distros% }" \
    --expect-majors "${majors% }" \
    --expect-arches "${ARCH}"

# The same boundary check the release runs: the manifest reads file names, this
# reads what the packages say about themselves.
step "packages agree with the manifest"
"${PKG_DIR}/scripts/verify-release-metadata.sh" "${PKG_DIR}/release"

printf '\n\033[1;32mrelease %s built for %s\033[0m\n' "${RELEASE_ID}" "${ARCH}" >&2
printf 'distros:    %s\n' "${DISTROS[*]}" >&2
printf 'majors:     %s\n' "${PG_MAJORS[*]}" >&2
printf 'artifacts:  %s/release\n' "${PKG_DIR}" >&2
printf 'manifest:   %s/release/postvec-release.json\n' "${PKG_DIR}" >&2
if (( ! SKIP_IMAGES )) && (( IMAGES_WANTED )); then
    printf 'node image: %s:%s (%s)\n' "${SERVER_IMAGE_REPOSITORY}" "${RELEASE_ID}" "${SERVER_LICENSE}" >&2
fi
printf '\nThis is one architecture. A publishable release also needs the other one,\n' >&2
printf 'built and tested on a native runner, before the manifest is final.\n' >&2
