#!/usr/bin/env bash
# Fixture-based regression tests for the parts of the release chain that decide
# whether a build may be published.
#
#   tests/unit-test.sh [pattern]
#
# No Docker, no compiler, no network, no PostgreSQL — seconds, on any machine.
#
# The expensive matrix proves that packages build, install and work. It does not
# prove that the *checks* work, because a check only reports when something is
# wrong, and nothing is ever deliberately wrong in a passing matrix. Two real
# defects lived through a full green matrix for exactly that reason: the package
# closure compared an RPM's `x86_64` against the canonical `amd64` and rejected
# every RPM, and the debug closure was keyed on a package name alone, so one
# Debian/amd64 symbols package satisfied all eight CLI tuples.
#
# So each case here builds a release that is correct, asserts it passes, then
# breaks exactly one thing and asserts it fails — and asserts *why*, because a
# check that fails for the wrong reason is a check that is not testing what its
# name says.

set -Eeuo pipefail

TESTS_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PKG_DIR="$(cd "${TESTS_DIR}/.." && pwd)"

# Placeholder publication metadata is fine here: nothing this suite produces is
# publishable, and requiring a real maintainer address to run the tests would
# make the tests the last thing anyone runs.
export POSTVEC_ALLOW_PLACEHOLDER_METADATA=1

# shellcheck source=../scripts/lib.sh
source "${PKG_DIR}/scripts/lib.sh"
load_versions

FILTER="${1:-}"
WORK="$(mktemp -d)"
trap 'rm -rf "${WORK}"' EXIT

PASSED=0; FAILED=0; SKIPPED=0
CURRENT=""

ok()   { printf '    \033[32mok\033[0m    %s\n' "$*"; PASSED=$((PASSED + 1)); }
bad()  { printf '    \033[1;31mFAIL\033[0m  %s\n' "$*" >&2; FAILED=$((FAILED + 1)); }
case_() {
    CURRENT="$1"
    if [[ -n "${FILTER}" && "${CURRENT}" != *"${FILTER}"* ]]; then
        SKIPPED=$((SKIPPED + 1)); return 1
    fi
    printf '\n\033[1m%s\033[0m\n' "${CURRENT}"
}

# ---------------------------------------------------------------- the fixtures

DISTRO_IDS=(debian12 ubuntu2204 ubuntu2404 el9)
MAJORS=(16 17 18)
ARCHES=(amd64 arm64)

# The bundled model's package identity follows the reviewed pins, exactly as
# scripts/check-model-bundle.py derives it from the verified registry archive.
# Repeating a literal package name here is how a bundled-model change passes a
# green closure test while shipping a differently named package.
MODEL_PKG_NAME="postvec-model-${BUNDLED_MODEL_PKG_SUFFIX}"
MODEL_PKG_VERSION="${BUNDLED_MODEL_REGISTRY_REVISION}.${BUNDLED_MODEL_BUNDLE_VERSION}.0"

# The fixture facts a release *would* have if it had built this model. Written
# into every scenario's build root so the manifest reads them the way it reads
# a real payload — and so this suite still needs no network and no 90 MB
# download.
FIXTURE_ARCHIVE_SIZE=91195392
FIXTURE_TARGET_DIM=384
FIXTURE_SEQUENCE_LEN=256
FIXTURE_SOURCE="https://huggingface.co/example/model@0123456789abcdef0123456789abcdef01234567"

make_model_payload() {
    local payload="$1" doc="$1/usr/share/doc/${MODEL_PKG_NAME}"
    mkdir -p "${doc}"
    cat > "${payload}/model-facts.env" <<FACTS
MODEL_PKG_NAME=${MODEL_PKG_NAME}
MODEL_PKG_VERSION=${MODEL_PKG_VERSION}
MODEL_DOC_DIR=/usr/share/doc/${MODEL_PKG_NAME}
MODEL_NAME=${BUNDLED_MODEL_NAME}
MODEL_BACKEND=onnx-runtime
MODEL_REGISTRY_REVISION=${BUNDLED_MODEL_REGISTRY_REVISION}
MODEL_ARCHIVE_SHA256=${BUNDLED_MODEL_ARCHIVE_SHA256}
MODEL_ARCHIVE_SIZE=${FIXTURE_ARCHIVE_SIZE}
MODEL_LICENSE=Apache-2.0
MODEL_TARGET_DIM=${FIXTURE_TARGET_DIM}
MODEL_SEQUENCE_LEN=${FIXTURE_SEQUENCE_LEN}
MODEL_BUNDLE_VERSION=${BUNDLED_MODEL_BUNDLE_VERSION}
FACTS
    cat > "${doc}/SOURCE.json" <<JSON
{
  "component": "registry-model",
  "internal_name": "${BUNDLED_MODEL_NAME}",
  "source": "${FIXTURE_SOURCE}",
  "files_sha256": {
    "ninference.hub.json": "$(printf 'a%.0s' {1..64})"
  }
}
JSON
}

# A file with content, because the manifest schema requires a positive size and
# an empty artifact would pass a check nobody meant to relax.
artifact() {
    local path="$1"
    mkdir -p "$(dirname "${path}")"
    printf 'not a real package: %s\n' "$(basename "${path}")" > "${path}"
    # A publishable build requires an SBOM beside every artifact.
    printf '{"spdxVersion":"SPDX-2.3","name":"%s"}\n' "$(basename "${path}")" \
        > "${path}.spdx.json"
}

# `deb_name <package> <version> <distro-tag> <arch>` and its RPM counterpart:
# the two file-name grammars the manifest has to read, stated once.
deb_name() { printf '%s_%s-%s+%s_%s.deb' "$1" "$2" "${PACKAGE_RELEASE}" "$3" "$4"; }
rpm_name() { printf '%s-%s-%s.%s.%s.rpm' "$1" "$2" "${PACKAGE_RELEASE}" "$3" "$4"; }

distro_tag() {
    case "$1" in
    debian12)   echo deb12 ;;
    ubuntu2204) echo ubuntu22.04 ;;
    ubuntu2404) echo ubuntu24.04 ;;
    el9)        echo el9 ;;
    esac
}

# The complete, correct artifact closure for the full release matrix, laid out
# in the three roots the pipeline actually writes.
make_dist() {
    local root="$1" distro tag family arch major package
    for distro in "${DISTRO_IDS[@]}"; do
        tag="$(distro_tag "${distro}")"
        family=deb; [[ "${distro}" == el9 ]] && family=rpm

        for arch in "${ARCHES[@]}"; do
            local common="${root}/common/${distro}-${arch}"
            if [[ "${family}" == deb ]]; then
                artifact "${common}/$(deb_name postvec-cli "${POSTVEC_VERSION}" "${tag}" "${arch}")"
                artifact "${common}/$(deb_name postvec-cli-dbgsym "${POSTVEC_VERSION}" "${tag}" "${arch}")"
                artifact "${common}/$(deb_name postvec-onnxruntime "${ORT_VERSION}" "${tag}" "${arch}")"
            else
                local rpm_arch=x86_64; [[ "${arch}" == arm64 ]] && rpm_arch=aarch64
                artifact "${common}/$(rpm_name postvec-cli "${POSTVEC_VERSION}" "${tag}" "${rpm_arch}")"
                artifact "${common}/$(rpm_name postvec-cli-debuginfo "${POSTVEC_VERSION}" "${tag}" "${rpm_arch}")"
                artifact "${common}/$(rpm_name postvec-onnxruntime "${ORT_VERSION}" "${tag}" "${rpm_arch}")"
            fi

            for major in "${MAJORS[@]}"; do
                local cell="${root}/extension/${distro}-pg${major}-${arch}"
                if [[ "${family}" == deb ]]; then
                    package="postgresql-${major}-postvec"
                    artifact "${cell}/$(deb_name "${package}" "${POSTVEC_VERSION}" "${tag}" "${arch}")"
                    artifact "${cell}/$(deb_name "${package}-dbgsym" "${POSTVEC_VERSION}" "${tag}" "${arch}")"
                else
                    package="postgresql${major}-postvec"
                    local rpm_arch=x86_64; [[ "${arch}" == arm64 ]] && rpm_arch=aarch64
                    artifact "${cell}/$(rpm_name "${package}" "${POSTVEC_VERSION}" "${tag}" "${rpm_arch}")"
                    artifact "${cell}/$(rpm_name "${package}-debuginfo" "${POSTVEC_VERSION}" "${tag}" "${rpm_arch}")"
                fi
            done
        done

        # Architecture-independent: one per distribution, in the noarch root.
        local noarch="${root}/noarch/${distro}"
        if [[ "${family}" == deb ]]; then
            artifact "${noarch}/$(deb_name "${MODEL_PKG_NAME}" "${MODEL_PKG_VERSION}" "${tag}" all)"
            artifact "${noarch}/$(deb_name "${EXTRAS_METAPACKAGE}" "${POSTVEC_VERSION}" "${tag}" all)"
        else
            artifact "${noarch}/$(rpm_name "${MODEL_PKG_NAME}" "${MODEL_PKG_VERSION}" "${tag}" noarch)"
            artifact "${noarch}/$(rpm_name "${EXTRAS_METAPACKAGE}" "${POSTVEC_VERSION}" "${tag}" noarch)"
        fi
    done
}

# 24 extension cells plus the 8 shared-package builders. The recorded facts are
# realistic, because they are checked: `base` is `${ID}-${VERSION_ID}` as the
# builder images write it, and the resolved PostgreSQL must be the cell's major.
cell_base() {
    case "$1" in
    debian12)   echo debian-12 ;;
    ubuntu2204) echo ubuntu-22.04 ;;
    ubuntu2404) echo ubuntu-24.04 ;;
    el9)        echo almalinux-9.8 ;;
    esac
}

make_build_info() {
    local root="$1" distro major arch
    for distro in "${DISTRO_IDS[@]}"; do
        for arch in "${ARCHES[@]}"; do
            write_cell "${root}/common-${distro}-${arch}" 18 "$(cell_base "${distro}")"
            for major in "${MAJORS[@]}"; do
                write_cell "${root}/${distro}-pg${major}-${arch}" \
                    "${major}" "$(cell_base "${distro}")"
            done
        done
    done
}

write_cell() {
    local dir="$1" pg_major="$2" base="$3"
    mkdir -p "${dir}"
    cat > "${dir}/build-info.txt" <<INFO
rustc=rustc ${RUST_VERSION} (fixture)
cargo_pgrx=cargo-pgrx ${PGRX_VERSION}
pg_major=${pg_major}
pg_version=PostgreSQL ${pg_major}.4 (fixture)
base=${base}
glibc=glibc 2.36
[packages]
fixture=1.0
INFO
}

# Six image descriptors, each with one SBOM per child manifest.
make_images() {
    local out="$1"
    python3 - "$out" "${POSTVEC_VERSION}" "${PACKAGE_RELEASE}" "${IMAGE_REPOSITORY}" \
        "${POSTGRES_IMAGE_PG16_DIGEST}" "${POSTGRES_IMAGE_PG17_DIGEST}" \
        "${POSTGRES_IMAGE_PG18_DIGEST}" <<'PY'
import hashlib, json, sys

out, version, revision, repo = sys.argv[1:5]
bases = dict(zip((16, 17, 18), sys.argv[5:8]))

def digest(*parts):
    return "sha256:" + hashlib.sha256("/".join(parts).encode()).hexdigest()

images = []
for major in (16, 17, 18):
    for variant in ("remote", "complete"):
        suffix = "-complete" if variant == "complete" else ""
        tag = "%s-%s-pg%s%s" % (version, revision, major, suffix)
        images.append({
            "name": repo, "tag": tag, "digest": digest("index", tag),
            "variant": variant, "pg_major": major, "base_digest": bases[major],
            "platforms": [
                {"platform": "linux/%s" % arch,
                 "digest": digest("child", tag, arch),
                 "sbom": "image-%s-linux-%s.spdx.json" % (tag, arch),
                 "sbom_sha256": hashlib.sha256(
                     ("sbom/%s/%s" % (tag, arch)).encode()).hexdigest()}
                for arch in ("amd64", "arm64")
            ],
        })
open(out, "w").write(json.dumps(images))
PY
}

# ------------------------------------------------------------------ the runner

# Build a manifest from a fixture and report whether it was accepted. Output is
# captured so a case can assert *why* a rejection happened.
LAST_OUTPUT=""

# `run_manifest_with <extra args…> -- <dist> <build-info> [images]`
EXTRA_MANIFEST_ARGS=()
run_manifest_with() {
    EXTRA_MANIFEST_ARGS=()
    while (($#)); do
        [[ "$1" == -- ]] && { shift; break; }
        EXTRA_MANIFEST_ARGS+=("$1"); shift
    done
    run_manifest "$@"
    local status=$?
    EXTRA_MANIFEST_ARGS=()
    return "${status}"
}

run_manifest() {
    local dist="$1" build_info="$2" images="${3:-}"
    local args=(
        --dist "${dist}"
        --build-info "${build_info}"
        --out "${PKG_DIR}/build/.unit-test-release"
        --allow-dirty
        --expect-distros "${DISTRO_IDS[*]}"
        --expect-majors "${MAJORS[*]}"
        --expect-arches "${ARCHES[*]}"
    )
    if [[ -n "${images}" ]]; then
        args+=(--images "${images}" --require-images --expect-images "${IMAGE_REPOSITORY}")
    fi
    (( ${#EXTRA_MANIFEST_ARGS[@]} )) && args+=("${EXTRA_MANIFEST_ARGS[@]}")
    LAST_OUTPUT="$("${PKG_DIR}/scripts/write-release-manifest.sh" "${args[@]}" 2>&1)"
}

# `expect_accepted <label> <dist> <build-info> [images]`
expect_accepted() {
    local label="$1"; shift
    if run_manifest "$@"; then
        ok "${label}"
    else
        bad "${label} — rejected a correct release:"
        printf '%s\n' "${LAST_OUTPUT}" | sed 's/^/          /' >&2
    fi
}

# `expect_rejected <label> <expected-message-fragment> <dist> <build-info> [images]`
expect_rejected() {
    local label="$1" fragment="$2"; shift 2
    if run_manifest "$@"; then
        bad "${label} — accepted a release that should have been refused"
        return
    fi
    if grep -qF -- "${fragment}" <<<"${LAST_OUTPUT}"; then
        ok "${label}"
    else
        bad "${label} — refused, but not for the stated reason (wanted: ${fragment})"
        printf '%s\n' "${LAST_OUTPUT}" | sed 's/^/          /' >&2
    fi
}

# A private copy of the fixtures per case, so one mutation cannot leak into the
# next and turn a later assertion into a coincidence.
scenario() {
    local name="$1" dir="${WORK}/$1"
    rm -rf "${dir}"
    mkdir -p "${dir}"
    make_dist "${dir}/dist"
    make_build_info "${dir}/build"
    make_model_payload "${dir}/build/payload-common"
    make_images "${dir}/images.json"
    printf '%s' "${dir}"
}

# =============================================================== package closure

if case_ "package closure: a correct full matrix is accepted"; then
    s="$(scenario correct)"
    expect_accepted "the full 4×3×2 matrix passes with images required" \
        "${s}/dist" "${s}/build" "${s}/images.json"
fi

if case_ "package closure: RPM architectures are canonicalised"; then
    # The regression. Artifact discovery canonicalises x86_64 to amd64;
    # verification used to compare the raw file-name spelling against the
    # canonical one, which no RPM can ever satisfy. Every EL9 package in the
    # fixture is named the way rpm names them, so this case fails if the
    # canonicalisation is removed again.
    s="$(scenario rpm-arch)"
    expect_accepted "el9 x86_64/aarch64 packages satisfy amd64/arm64" \
        "${s}/dist" "${s}/build" "${s}/images.json"

    # …and canonicalising is not the same as accepting anything. A file name the
    # identity grammar cannot read is refused rather than waved through: the
    # closure is only as strong as its willingness to say "I cannot tell".
    s="$(scenario rpm-arch-unparseable)"
    name="$(deb_name postvec-cli "${POSTVEC_VERSION}" deb12 amd64)"
    rm "${s}/dist/common/debian12-amd64/${name}"*
    artifact "${s}/dist/common/debian12-amd64/postvec-cli_${POSTVEC_VERSION}-x+deb12_amd64.deb"
    expect_rejected "a file name the identity grammar cannot read is refused" \
        "unparseable file name" "${s}/dist" "${s}/build" "${s}/images.json"
fi

if case_ "package closure: identity is checked, not just presence"; then
    s="$(scenario stale-version)"
    old="${s}/dist/common/debian12-amd64/$(deb_name postvec-cli 0.0.9 deb12 amd64)"
    rm "${s}/dist/common/debian12-amd64/$(deb_name postvec-cli "${POSTVEC_VERSION}" deb12 amd64)"*
    artifact "${old}"
    expect_rejected "a package from an older version is refused" \
        "is version 0.0.9" "${s}/dist" "${s}/build" "${s}/images.json"

    s="$(scenario stale-revision)"
    name="$(deb_name postvec-cli "${POSTVEC_VERSION}" deb12 amd64)"
    rm "${s}/dist/common/debian12-amd64/${name}"*
    artifact "${s}/dist/common/debian12-amd64/${name/-${PACKAGE_RELEASE}+/-99+}"
    expect_rejected "a package from another packaging revision is refused" \
        "packaging revision 99" "${s}/dist" "${s}/build" "${s}/images.json"

    # Two builds of one file name. This is what a shared package rebuilt in
    # every matrix cell looks like, and an RPM records its build host, so the
    # two are not even the same bytes.
    s="$(scenario duplicate-build)"
    name="$(deb_name postvec-cli "${POSTVEC_VERSION}" deb12 amd64)"
    printf 'a second, different build of the same name\n' \
        > "${s}/dist/common/debian12-arm64/${name}"
    expect_rejected "two different builds of one file name are refused" \
        "two different builds of ${name}" "${s}/dist" "${s}/build" "${s}/images.json"
fi

if case_ "package closure: missing and unexpected artifacts"; then
    s="$(scenario missing)"
    rm "${s}/dist/noarch/debian12/"${EXTRAS_METAPACKAGE}_*
    expect_rejected "a missing metapackage is refused" \
        "missing: ${EXTRAS_METAPACKAGE}" "${s}/dist" "${s}/build" "${s}/images.json"

    s="$(scenario unexpected)"
    artifact "${s}/dist/common/debian12-amd64/$(deb_name postvec-surprise 1.0.0 deb12 amd64)"
    expect_rejected "a package nobody asked for is refused" \
        "unexpected: postvec-surprise" "${s}/dist" "${s}/build" "${s}/images.json"
fi

if case_ "package closure: the noarch packages live in the noarch root"; then
    # `all`/`noarch` content written into one architecture's cell is invisible
    # to the other architecture. The manifest walks the whole tree, so the file
    # is still found — what this asserts is that the *release* keeps exactly one
    # copy of it, which is what breaks the moment a second architecture's job
    # writes its own.
    s="$(scenario noarch-duplicated)"
    name="$(deb_name "${MODEL_PKG_NAME}" "${MODEL_PKG_VERSION}" deb12 all)"
    mkdir -p "${s}/dist/common/debian12-arm64"
    printf 'a different build of the same name\n' \
        > "${s}/dist/common/debian12-arm64/${name}"
    expect_rejected "two different builds of one noarch package are refused" \
        "two different builds of ${name}" "${s}/dist" "${s}/build" "${s}/images.json"
fi

# ================================================================= debug closure

if case_ "debug closure: one symbols package cannot cover every cell"; then
    # The fail-open regression. Keyed on the base package name alone, a single
    # Debian/amd64 CLI symbols package satisfied all eight CLI tuples.
    s="$(scenario debug-shared)"
    for distro in ubuntu2204 ubuntu2404; do
        rm "${s}/dist/common/${distro}-amd64/"postvec-cli-dbgsym_*
    done
    rm "${s}/dist/common/debian12-arm64/"postvec-cli-dbgsym_*
    expect_rejected "symbols missing for three of the eight CLI cells are refused" \
        "missing debug package for postvec-cli" \
        "${s}/dist" "${s}/build" "${s}/images.json"
fi

if case_ "debug closure: symbols carry the release's identity"; then
    s="$(scenario debug-stale)"
    name="$(deb_name postvec-cli-dbgsym "${POSTVEC_VERSION}" deb12 amd64)"
    rm "${s}/dist/common/debian12-amd64/${name}"*
    artifact "${s}/dist/common/debian12-amd64/$(deb_name postvec-cli-dbgsym 0.0.9 deb12 amd64)"
    expect_rejected "symbols from an older version are refused" \
        "is version 0.0.9" "${s}/dist" "${s}/build" "${s}/images.json"
fi

if case_ "debug closure: unexpected and duplicate symbols packages"; then
    s="$(scenario debug-unexpected)"
    artifact "${s}/dist/common/debian12-amd64/$(deb_name postvec-onnxruntime-dbgsym "${ORT_VERSION}" deb12 amd64)"
    expect_rejected "symbols for a package that owes none are refused" \
        "unexpected debug package: postvec-onnxruntime" \
        "${s}/dist" "${s}/build" "${s}/images.json"
fi

# ================================================================== build cells

if case_ "build cells: the exact 24 + 8 set is required"; then
    s="$(scenario cells-missing)"
    rm -r "${s}/build/el9-pg16-arm64"
    expect_rejected "a missing extension cell is refused" \
        "missing build record: el9-pg16-arm64" \
        "${s}/dist" "${s}/build" "${s}/images.json"

    s="$(scenario cells-extra)"
    cp -r "${s}/build/common-debian12-amd64" "${s}/build/common-debian13-amd64"
    expect_rejected "a build record nobody expected is refused" \
        "unexpected build record: common-debian13-amd64" \
        "${s}/dist" "${s}/build" "${s}/images.json"
fi

if case_ "build cells: recorded facts must agree with the cell's identity"; then
    # A cell-name set proves 32 files arrived. These are the fields that say
    # what is actually in them.
    s="$(scenario cells-wrong-major)"
    write_cell "${s}/build/debian12-pg16-amd64" 16 debian-12
    sed -i 's/^pg_version=.*/pg_version=PostgreSQL 17.4 (fixture)/' \
        "${s}/build/debian12-pg16-amd64/build-info.txt"
    expect_rejected "a pg16 cell that resolved PostgreSQL 17 is refused" \
        "is not PostgreSQL 16" "${s}/dist" "${s}/build" "${s}/images.json"

    s="$(scenario cells-wrong-base)"
    sed -i 's/^base=.*/base=ubuntu-24.04/' "${s}/build/debian12-pg18-arm64/build-info.txt"
    expect_rejected "a debian12 cell built on ubuntu is refused" \
        "but debian12 means debian-12" "${s}/dist" "${s}/build" "${s}/images.json"

    s="$(scenario cells-wrong-toolchain)"
    sed -i 's/^rustc=.*/rustc=rustc 1.70.0 (fixture)/' "${s}/build/el9-pg17-amd64/build-info.txt"
    expect_rejected "a cell built with an unpinned compiler is refused" \
        "expected the pinned ${RUST_VERSION}" "${s}/dist" "${s}/build" "${s}/images.json"
fi

# ================================================================ image closure

if case_ "image closure: every image needs an SBOM per architecture"; then
    # The multi-architecture SBOM regression: one document produced against a
    # manifest-list digest describes linux/amd64 and says nothing about arm64.
    s="$(scenario image-one-sbom)"
    python3 -c '
import json, sys
images = json.load(open(sys.argv[1]))
images[0]["platforms"] = [p for p in images[0]["platforms"] if p["platform"] == "linux/amd64"]
json.dump(images, open(sys.argv[1], "w"))
' "${s}/images.json"
    expect_rejected "an image documented only for amd64 is refused" \
        "has no linux/arm64 SBOM" "${s}/dist" "${s}/build" "${s}/images.json"

    s="$(scenario image-index-as-child)"
    python3 -c '
import json, sys
images = json.load(open(sys.argv[1]))
images[0]["platforms"][1]["digest"] = images[0]["digest"]
json.dump(images, open(sys.argv[1], "w"))
' "${s}/images.json"
    expect_rejected "a child recorded as the index itself is refused" \
        "a child manifest has the index's own digest" \
        "${s}/dist" "${s}/build" "${s}/images.json"
fi

if case_ "image closure: the base image must be the pinned one"; then
    s="$(scenario image-base)"
    python3 -c '
import json, sys
images = json.load(open(sys.argv[1]))
images[0]["base_digest"] = "sha256:" + "0" * 64
json.dump(images, open(sys.argv[1], "w"))
' "${s}/images.json"
    expect_rejected "an image built on an unpinned base is refused" \
        "but this release pins" "${s}/dist" "${s}/build" "${s}/images.json"
fi

if case_ "image closure: six distinct images, correctly tagged"; then
    s="$(scenario image-missing)"
    python3 -c '
import json, sys
images = [i for i in json.load(open(sys.argv[1])) if not (i["pg_major"] == 17 and i["variant"] == "complete")]
json.dump(images, open(sys.argv[1], "w"))
' "${s}/images.json"
    expect_rejected "a missing image is refused" \
        "missing image: PG 17 complete" "${s}/dist" "${s}/build" "${s}/images.json"

    s="$(scenario image-tag)"
    python3 -c '
import json, sys
images = json.load(open(sys.argv[1]))
images[0]["tag"] = "latest"
json.dump(images, open(sys.argv[1], "w"))
' "${s}/images.json"
    expect_rejected "an image whose tag is not this release's is refused" \
        "expected" "${s}/dist" "${s}/build" "${s}/images.json"
fi

# ================================================================== the manifest

if case_ "the generated manifest matches its published schema"; then
    s="$(scenario schema)"
    # The validator, not merely the package: jsonschema 3.x imports fine and has
    # no Draft202012Validator, which is the whole reason the release pins a
    # floor. Probing for the import alone would report a skip as a failure.
    if python3 -c 'from jsonschema import Draft202012Validator' 2>/dev/null; then
        if run_manifest "${s}/dist" "${s}/build" "${s}/images.json" \
            && grep -q "manifest validates against" <<<"${LAST_OUTPUT}"; then
            ok "postvec-release.json validates against release-manifest.schema.json"
        else
            bad "the manifest did not validate"
            printf '%s\n' "${LAST_OUTPUT}" | sed 's/^/          /' >&2
        fi
    else
        printf '    skip  jsonschema is not installed (pip install -r %s)\n' \
            "packaging/postvec/requirements-schema.txt"
        SKIPPED=$((SKIPPED + 1))
    fi

    # SHA256SUMS has to be checkable from the directory a user downloads into,
    # which is the whole reason the release directory is flat.
    if run_manifest "${s}/dist" "${s}/build" "${s}/images.json"; then
        if ( cd "${PKG_DIR}/build/.unit-test-release" && sha256sum --check --quiet SHA256SUMS ); then
            ok "SHA256SUMS verifies against the flat release directory"
        else
            bad "SHA256SUMS does not verify"
        fi
    fi
fi

# =============================================================== versions.env

# The pin parser, against fixture files. lib.sh derives PKG_DIR from its own
# path, so a fixture package directory with a symlinked lib.sh is all it takes
# to point `load_versions` at a different versions.env.
check_versions_env() {
    local label="$1" fragment="$2" mutation="$3" dir
    dir="${WORK}/versions-$(printf '%s' "${label}" | tr -cs 'a-zA-Z0-9' '-')"
    rm -rf "${dir}"; mkdir -p "${dir}/scripts"
    ln -s "${PKG_DIR}/scripts/lib.sh" "${dir}/scripts/lib.sh"
    sed "${mutation}" "${PKG_DIR}/versions.env" > "${dir}/versions.env"

    local output status=0
    output="$(bash -c "source '${dir}/scripts/lib.sh'; load_versions" 2>&1)" || status=$?
    if [[ -z "${fragment}" ]]; then
        if (( status == 0 )); then ok "${label}"; else
            bad "${label} — rejected an acceptable versions.env: ${output}"
        fi
        return
    fi
    if (( status == 0 )); then
        bad "${label} — accepted a versions.env that should have been refused"
    elif grep -qF -- "${fragment}" <<<"${output}"; then
        ok "${label}"
    else
        bad "${label} — refused, but not for the stated reason (wanted: ${fragment})"
        printf '%s\n' "${output}" | sed 's/^/          /' >&2
    fi
}

if case_ "release mode: which runs may publish, and where"; then
    # The whole mode × tag × override cross-product, against the real gate.
    # Validating the tag *grammar* elsewhere is not the same as proving that
    # `publish` refuses a rehearsal tag and `disposable-publication` refuses a
    # production one — that is a property of this decision, and it now lives
    # somewhere it can be run.
    #
    # `mode_is <label> <expected: publish|refuse> [KEY=VALUE …]`
    mode_is() {
        local label="$1" want="$2"; shift 2
        local output status=0
        output="$(env "$@" "${PKG_DIR}/scripts/release-mode.sh" 2>&1)" || status=$?
        case "${want}" in
        refuse)
            (( status != 0 )) && ok "${label}" \
                || bad "${label} — allowed:"$'\n'"$(sed 's/^/          /' <<<"${output}")" ;;
        publish|rehearse)
            if (( status != 0 )); then
                bad "${label} — refused:"$'\n'"$(sed 's/^/          /' <<<"${output}")"
            elif grep -qxF "publish=$([[ ${want} == publish ]] && echo true || echo false)" \
                 <<<"${output}"; then
                ok "${label}"
            else
                bad "${label} — wrong verdict:"$'\n'"$(sed 's/^/          /' <<<"${output}")"
            fi ;;
        esac
    }

    REAL_TAG="postvec-v${RELEASE_ID}"
    SCRATCH_TAG="postvec-rehearsal-v${POSTVEC_VERSION}-9001"
    OV=(POSTVEC_IMAGE_REPOSITORY=ghcr.io/x/y POSTVEC_IMAGE_STAGING_REPOSITORY=ghcr.io/x/y-staging)

    mode_is "a rehearsal of a branch pushes nothing" rehearse \
        POSTVEC_MODE=rehearse POSTVEC_RELEASE_REF=main POSTVEC_DISPATCH_REF=main
    mode_is "a rehearsal may not carry an override" refuse \
        POSTVEC_MODE=rehearse POSTVEC_RELEASE_REF=main POSTVEC_DISPATCH_REF=main "${OV[@]}"

    mode_is "publish accepts the release tag" publish \
        POSTVEC_MODE=publish POSTVEC_RELEASE_REF="${REAL_TAG}" POSTVEC_DISPATCH_REF="${REAL_TAG}"
    # The two that would do the wrong thing quietly.
    mode_is "publish refuses a rehearsal tag" refuse \
        POSTVEC_MODE=publish POSTVEC_RELEASE_REF="${SCRATCH_TAG}" POSTVEC_DISPATCH_REF="${SCRATCH_TAG}"
    mode_is "publish refuses a repository override" refuse \
        POSTVEC_MODE=publish POSTVEC_RELEASE_REF="${REAL_TAG}" POSTVEC_DISPATCH_REF="${REAL_TAG}" "${OV[@]}"
    mode_is "publish refuses a branch" refuse \
        POSTVEC_MODE=publish POSTVEC_RELEASE_REF=main POSTVEC_DISPATCH_REF=main

    mode_is "disposable-publication accepts a rehearsal tag with both overrides" publish \
        POSTVEC_MODE=disposable-publication POSTVEC_RELEASE_REF="${SCRATCH_TAG}" \
        POSTVEC_DISPATCH_REF="${SCRATCH_TAG}" "${OV[@]}"
    mode_is "disposable-publication refuses a production tag" refuse \
        POSTVEC_MODE=disposable-publication POSTVEC_RELEASE_REF="${REAL_TAG}" \
        POSTVEC_DISPATCH_REF="${REAL_TAG}" "${OV[@]}"
    mode_is "disposable-publication refuses half an override" refuse \
        POSTVEC_MODE=disposable-publication POSTVEC_RELEASE_REF="${SCRATCH_TAG}" \
        POSTVEC_DISPATCH_REF="${SCRATCH_TAG}" POSTVEC_IMAGE_REPOSITORY=ghcr.io/x/y
    mode_is "disposable-publication refuses no override at all" refuse \
        POSTVEC_MODE=disposable-publication POSTVEC_RELEASE_REF="${SCRATCH_TAG}" \
        POSTVEC_DISPATCH_REF="${SCRATCH_TAG}"

    # The dispatch ref decides which workflow file runs; an input cannot.
    mode_is "publishing from a branch that names a tag is refused" refuse \
        POSTVEC_MODE=publish POSTVEC_RELEASE_REF="${REAL_TAG}" POSTVEC_DISPATCH_REF=main

    # Absent is not "skip the check". A gate that passes when it cannot tell is
    # not a gate — and this one is only ever wired up by a workflow, so the
    # failure mode is a caller that forgets, not a user that lies.
    mode_is "publishing with no dispatch ref at all is refused" refuse \
        POSTVEC_MODE=publish POSTVEC_RELEASE_REF="${REAL_TAG}" POSTVEC_DISPATCH_REF=
    mode_is "disposable-publication with no dispatch ref is refused" refuse \
        POSTVEC_MODE=disposable-publication POSTVEC_RELEASE_REF="${SCRATCH_TAG}" \
        POSTVEC_DISPATCH_REF= "${OV[@]}"
    # …but a rehearsal never publishes, so it has nothing to bind and may run
    # without one.
    mode_is "a rehearsal does not need a dispatch ref" rehearse \
        POSTVEC_MODE=rehearse POSTVEC_RELEASE_REF=main POSTVEC_DISPATCH_REF=
    mode_is "an unknown mode is refused" refuse \
        POSTVEC_MODE=ship-it POSTVEC_RELEASE_REF="${REAL_TAG}" POSTVEC_DISPATCH_REF="${REAL_TAG}"

    # The remedy has to be complete, or it fails the next check and sends
    # somebody round the loop again.
    remedy="$(env POSTVEC_MODE=disposable-publication POSTVEC_RELEASE_REF="${SCRATCH_TAG}" \
                  POSTVEC_DISPATCH_REF=main "${OV[@]}" \
                  "${PKG_DIR}/scripts/release-mode.sh" 2>&1 || true)"
    if grep -q -- "--ref ${SCRATCH_TAG}" <<<"${remedy}" \
       && grep -q -- "-f image_repository=" <<<"${remedy}" \
       && grep -q -- "-f image_staging_repository=" <<<"${remedy}"; then
        ok "the ref-mismatch remedy names every input the mode requires"
    else
        bad "the remedy is not a runnable command:"$'\n'"$(sed 's/^/          /' <<<"${remedy}")"
    fi
fi

if case_ "moving tags: a failed write does not abandon the other five"; then
    # The defect this replaced: both workflows advanced the six tags in an
    # inline loop under `set -e`, so the first registry failure aborted the
    # step — the remaining tags were never attempted, the verification never
    # ran, and the recovery guidance never printed. An operator was left with a
    # red step and no idea which tags had moved.
    #
    # A fake registry client stands in for `docker buildx imagetools`, which is
    # what makes the failure paths testable at all.
    fake_registry() {
        local dir="$1"
        mkdir -p "${dir}"
        cat > "${dir}/imagetools" <<'FAKE'
#!/usr/bin/env bash
# create <...> --tag <tag> <source>   |   inspect <tag> --format ...
set -u
log() { printf '%s\n' "$*" >> "${FAKE_LOG}"; }
case "$1" in
create)
    shift
    tag=""; src=""
    while (($#)); do
        case "$1" in --tag) tag="$2"; shift 2 ;; *) src="$1"; shift ;; esac
    done
    log "create ${tag} ${src}"
    [[ "${tag}" == "${FAKE_FAIL_CREATE:-}" ]] && { echo "denied" >&2; exit 1; }
    # The registry now holds what was written: the digest half of the source.
    printf '%s\t%s\n' "${tag}" "${src#*@}" >> "${FAKE_STATE}"
    ;;
inspect)
    tag="$2"
    log "inspect ${tag}"
    [[ "${tag}" == "${FAKE_FAIL_INSPECT:-}" ]] && { echo "unreachable" >&2; exit 1; }
    # Last write wins, like a tag.
    digest="$(awk -F'\t' -v t="${tag}" '$1 == t {d=$2} END {print d}' "${FAKE_STATE}")"
    [[ -n "${digest}" ]] || { echo "manifest unknown" >&2; exit 1; }
    printf '%s\n' "${digest}"
    ;;
esac
FAKE
        chmod +x "${dir}/imagetools"
    }

    moving_plan() {
        local out="$1" repo=ghcr.io/example/postvec i
        : > "${out}"
        for i in 16 17 18; do
            for suffix in "" "-complete"; do
                printf '%s %s %s %s\n' \
                    "${repo}:pg${i}${suffix}" \
                    "${repo}@sha256:$(printf '%064d' "${i}${#suffix}")" \
                    "sha256:$(printf '%064d' "${i}${#suffix}")" \
                    "<none>" >> "${out}"
            done
        done
    }

    # `advance <label> <expected: pass|fail> [env…]`
    ADVANCE_OUTPUT=""
    advance() {
        local dir="${WORK}/moving-$1"
        rm -rf "${dir}"; mkdir -p "${dir}"
        fake_registry "${dir}/bin"
        moving_plan "${dir}/plan"
        : > "${dir}/log"; : > "${dir}/state"
        local status=0
        ADVANCE_OUTPUT="$(env FAKE_LOG="${dir}/log" FAKE_STATE="${dir}/state" \
            POSTVEC_IMAGETOOLS="${dir}/bin/imagetools" "${@:3}" \
            "${PKG_DIR}/scripts/advance-moving-tags.sh" "${dir}/plan" \
            --recovery-hint "Rerun the postvec-moving-tags workflow." 2>&1)" || status=$?
        ADVANCE_LOG="${dir}/log"
        return "${status}"
    }

    if advance happy pass; then
        ok "every tag written and verified: the command succeeds"
    else
        bad "a clean run failed:"$'\n'"$(sed 's/^/          /' <<<"${ADVANCE_OUTPUT}")"
    fi

    # The case that motivated all of this.
    advance one-write-fails fail FAKE_FAIL_CREATE=ghcr.io/example/postvec:pg17 \
        && bad "a failed write did not fail the command" \
        || ok "a failed write fails the command"

    creates="$(grep -c '^create ' "${ADVANCE_LOG}" || true)"
    [[ "${creates}" == 6 ]] \
        && ok "all six writes were attempted despite the failure" \
        || bad "only ${creates} of 6 writes were attempted"

    inspects="$(grep -c '^inspect ' "${ADVANCE_LOG}" || true)"
    [[ "${inspects}" == 6 ]] \
        && ok "all six tags were inspected afterwards" \
        || bad "only ${inspects} of 6 tags were inspected"

    grep -qF "Rerun the postvec-moving-tags workflow." <<<"${ADVANCE_OUTPUT}" \
        && ok "the recovery guidance was printed" \
        || bad "the recovery guidance never printed"

    grep -qF "pg17" <<<"${ADVANCE_OUTPUT}" \
        && ok "the failing tag is named in the report" \
        || bad "the report does not say which tag is wrong"

    # A tag whose state cannot be read is not 'probably fine'.
    advance inspect-fails fail FAKE_FAIL_INSPECT=ghcr.io/example/postvec:pg16 \
        && bad "an uninspectable tag was reported as correct" \
        || ok "a tag whose state is unknown fails the command"

    # The preflight guard everything else rests on. Its contract is not "reject
    # eventually" but "change nothing" — so each case asserts both the refusal
    # *and* that no write was attempted.
    #
    # `refuses_plan <label> <plan line>`
    refuses_plan() {
        local label="$1" line="$2" slug s
        slug="$(printf '%s' "${label}" | tr -cs 'a-zA-Z0-9' '-')"
        s="${WORK}/moving-refuse-${slug}"
        rm -rf "${s}"; mkdir -p "${s}"
        fake_registry "${s}/bin"
        printf '%s\n' "${line}" > "${s}/plan"
        : > "${s}/log"; : > "${s}/state"
        if env FAKE_LOG="${s}/log" FAKE_STATE="${s}/state" \
            POSTVEC_IMAGETOOLS="${s}/bin/imagetools" \
            "${PKG_DIR}/scripts/advance-moving-tags.sh" "${s}/plan" >/dev/null 2>&1; then
            bad "${label} — the plan was accepted"
            return
        fi
        [[ -s "${s}/log" ]] \
            && bad "${label} — refused, but only after writing: $(cat "${s}/log")" \
            || ok "${label}"
    }

    REPO=ghcr.io/example/postvec
    GOOD="sha256:$(printf '%064d' 1)"
    OTHER="sha256:$(printf '%064d' 2)"

    refuses_plan "a mutable tag as the source is refused, with nothing written" \
        "${REPO}:pg18 ${REPO}:0.1.0-1-pg18 ${GOOD} x"
    # `@sha256:` appearing *somewhere* is not a pinned reference. Both of these
    # satisfied the old substring test.
    refuses_plan "a source whose digest is not hexadecimal is refused" \
        "${REPO}:pg18 ${REPO}@sha256:not-a-digest ${GOOD} x"
    refuses_plan "a source with no repository is refused" \
        "${REPO}:pg18 @sha256:$(printf '%064d' 1) ${GOOD} x"
    refuses_plan "a source digest of the wrong length is refused" \
        "${REPO}:pg18 ${REPO}@sha256:$(printf '%063d' 1) ${GOOD} x"
    refuses_plan "an uppercase source digest is refused" \
        "${REPO}:pg18 ${REPO}@sha256:$(printf 'A%.0s' {1..64}) ${GOOD} x"
    # The one that would otherwise write first and complain afterwards: both
    # halves well-formed, and disagreeing.
    refuses_plan "a source digest that is not the expected digest is refused" \
        "${REPO}:pg18 ${REPO}@${OTHER} ${GOOD} x"
    refuses_plan "a malformed expected digest is refused" \
        "${REPO}:pg18 ${REPO}@${GOOD} sha256:nope x"
fi

if case_ "release identity: the tag states which kind of release this is"; then
    # The two publication modes must not be able to produce each other's
    # release. A scratch tag dispatched as `publish` would advance the
    # production moving tags; a real tag dispatched as `disposable-publication`
    # would occupy the GitHub release the real publication needs — and a release
    # is published once.
    #
    # The workflow refuses each combination by tag prefix; this asserts the
    # identity the prefixes are built from, which is what makes that refusal
    # meaningful rather than cosmetic.
    assert_tag() {
        local label="$1" tag="$2" want="$3" output status=0
        output="$(POSTVEC_RELEASE_TAG="${tag}" \
                  "${PKG_DIR}/scripts/assert-versions.sh" 2>&1)" || status=$?
        if [[ "${want}" == accept ]]; then
            (( status == 0 )) && ok "${label}" \
                || { bad "${label} — refused: $(grep -m1 FAIL <<<"${output}")"; }
        else
            (( status != 0 )) && ok "${label}" \
                || bad "${label} — accepted a tag that is not this release"
        fi
    }
    assert_tag "the release tag is accepted"   "postvec-v${RELEASE_ID}" accept
    assert_tag "the rehearsal tag is accepted" "postvec-rehearsal-v${RELEASE_ID}" accept
    assert_tag "a release tag for another revision is refused" \
        "postvec-v${POSTVEC_VERSION}-9001" refuse
    assert_tag "a rehearsal tag for another revision is refused" \
        "postvec-rehearsal-v${POSTVEC_VERSION}-9001" refuse

    # `workflow_dispatch` sets GITHUB_REF_NAME to the ref the *workflow file*
    # came from, which need not be what is being released. The explicit tag has
    # to win, or a dispatch from `main` would be validated as — and recorded
    # as — a release of `main`.
    output="$(GITHUB_REF_NAME=main POSTVEC_RELEASE_TAG="postvec-v${RELEASE_ID}" \
              "${PKG_DIR}/scripts/assert-versions.sh" 2>&1)" || output="FAILED: ${output}"
    grep -qF -- "ok    release tag" <<<"${output}" \
        && ok "POSTVEC_RELEASE_TAG wins over an ambient GITHUB_REF_NAME" \
        || bad "the ambient GITHUB_REF_NAME overrode the stated release tag"

    # …and the manifest records the tag it was told, not the ambient one.
    s="$(scenario git-tag)"
    if GITHUB_REF_NAME=main run_manifest_with --git-tag "postvec-v${RELEASE_ID}" \
        -- "${s}/dist" "${s}/build" "${s}/images.json"; then
        recorded="$(python3 -c '
import json, sys; print(json.load(open(sys.argv[1])).get("git_tag", ""))' \
            "${PKG_DIR}/build/.unit-test-release/postvec-release.json")"
        [[ "${recorded}" == "postvec-v${RELEASE_ID}" ]] \
            && ok "the manifest records the stated tag, not GITHUB_REF_NAME" \
            || bad "the manifest recorded git_tag=${recorded:-<none>}"
    else
        bad "the manifest could not be generated with an explicit --git-tag"
    fi
fi

if case_ "versions.env: the committed file is valid"; then
    check_versions_env "the committed pins load" "" ''
fi

if case_ "versions.env: a pin must be a pin"; then
    check_versions_env "a tag-only nfpm image is refused" \
        "NFPM_IMAGE must be" 's|^NFPM_IMAGE=.*|NFPM_IMAGE=ghcr.io/goreleaser/nfpm:v2.43.0|'
    check_versions_env "a truncated nfpm digest is refused" \
        "NFPM_IMAGE must be" 's|^NFPM_IMAGE=.*|NFPM_IMAGE=ghcr.io/goreleaser/nfpm@sha256:deadbeef|'
    check_versions_env "an uppercase nfpm digest is refused" \
        "NFPM_IMAGE must be" \
        "s|^NFPM_IMAGE=.*|NFPM_IMAGE=ghcr.io/goreleaser/nfpm@sha256:$(printf 'A%.0s' {1..64})|"
    check_versions_env "a tag-only BuildKit image is refused" \
        "BUILDKIT_IMAGE must be" 's|^BUILDKIT_IMAGE=.*|BUILDKIT_IMAGE=moby/buildkit:v0.31.2|'
    check_versions_env "a floating buildx version is refused" \
        "BUILDX_VERSION must be" 's|^BUILDX_VERSION=.*|BUILDX_VERSION=latest|'
    check_versions_env "a truncated ONNX Runtime digest is refused" \
        "ORT_LINUX_X64_SHA256 is not a sha256" 's|^ORT_LINUX_X64_SHA256=.*|ORT_LINUX_X64_SHA256=abc123|'
    check_versions_env "a malformed base-image digest is refused" \
        "POSTGRES_IMAGE_PG18_DIGEST is not an image digest" \
        's|^POSTGRES_IMAGE_PG18_DIGEST=.*|POSTGRES_IMAGE_PG18_DIGEST=latest|'
    check_versions_env "a zero packaging revision is refused" \
        "PACKAGE_RELEASE must be" 's|^PACKAGE_RELEASE=.*|PACKAGE_RELEASE=0|'
    check_versions_env "an extras package named after the mode is refused" \
        "must not contain embedded, engine, or complete" \
        's|^EXTRAS_METAPACKAGE=.*|EXTRAS_METAPACKAGE=postvec-embedded|'
    check_versions_env "an extras package named complete is refused" \
        "must not contain embedded, engine, or complete" \
        's|^EXTRAS_METAPACKAGE=.*|EXTRAS_METAPACKAGE=postvec-complete|'
    check_versions_env "an extras package that is not postvec-* is refused" \
        "is not a postvec-* package name" \
        's|^EXTRAS_METAPACKAGE=.*|EXTRAS_METAPACKAGE=extras|'
fi

if case_ "versions.env: publication metadata has a grammar"; then
    # These reach registry commands in jobs that hold package-write permission.
    # They are reviewed repository content rather than workflow input, so a
    # grammar is a second line — but "reviewed" is a process and this is a check.
    check_versions_env "a tagged image repository is refused" \
        "IMAGE_REPOSITORY is not a plain container repository" \
        's|^IMAGE_REPOSITORY=.*|IMAGE_REPOSITORY=ghcr.io/univec-ai/postvec:latest|'
    check_versions_env "an image repository with a shell metacharacter is refused" \
        "IMAGE_REPOSITORY is not a plain container repository" \
        's|^IMAGE_REPOSITORY=.*|IMAGE_REPOSITORY=ghcr.io/univec-ai/postvec;id|'
    check_versions_env "an image repository with whitespace is refused" \
        "IMAGE_REPOSITORY is not a plain container repository" \
        's|^IMAGE_REPOSITORY=.*|IMAGE_REPOSITORY="ghcr.io/univec-ai/post vec"|'
    check_versions_env "staging that is not separate is refused" \
        "must differ from IMAGE_REPOSITORY" \
        's|^IMAGE_STAGING_REPOSITORY=.*|IMAGE_STAGING_REPOSITORY=ghcr.io/univec-ai/postvec|'
    check_versions_env "a non-https source repository is refused" \
        "SOURCE_REPOSITORY must be a plain https URL" \
        's|^SOURCE_REPOSITORY=.*|SOURCE_REPOSITORY=git@github.com:univec-ai/postvec.git|'
    check_versions_env "a maintainer without an address is refused" \
        "MAINTAINER must be" 's|^MAINTAINER=.*|MAINTAINER=UniVec|'
fi

if case_ "the prerequisite bootstrap emits the right commands per distribution"; then
    # CodeReady Builder is the trap here. Every EL9 rebuild calls it `crb` and
    # enables it with dnf; subscribed RHEL calls it
    # `codeready-builder-for-rhel-9-<arch>-rpms` and enables it through
    # subscription-manager. A script tested only on AlmaLinux and advertised for
    # RHEL passes its own test suite and fails on the system it claims.
    #
    # `--print` generates the plan from /etc/os-release, so a fixture os-release
    # is all it takes to cover every declared target without a container each.
    plan_for() {
        local name="$1" content="$2"
        local osr="${WORK}/os-release-${name}"
        printf '%s\n' "${content}" > "${osr}"
        POSTVEC_OS_RELEASE="${osr}" \
            bash "${PKG_DIR}/scripts/postvec-prerequisites.sh" --print 2>/dev/null
    }
    # `expect_plan <label> <fixture name> <os-release> <must contain> <must not contain>`
    expect_plan() {
        local label="$1" name="$2" osr="$3" want="$4" unwanted="${5:-}" plan
        plan="$(plan_for "${name}" "${osr}")" || { bad "${label} — the plan could not be generated"; return; }
        if ! grep -qF -- "${want}" <<<"${plan}"; then
            bad "${label} — the plan does not contain: ${want}"
            return
        fi
        if [[ -n "${unwanted}" ]] && grep -qF -- "${unwanted}" <<<"${plan}"; then
            bad "${label} — the plan wrongly contains: ${unwanted}"
            return
        fi
        ok "${label}"
    }

    expect_plan "Debian 12 targets bookworm-pgdg" debian12 \
        $'ID=debian\nVERSION_ID="12"\nVERSION_CODENAME=bookworm\nPRETTY_NAME="Debian 12"' \
        "bookworm-pgdg main"
    expect_plan "Ubuntu 22.04 targets jammy-pgdg" ubuntu2204 \
        $'ID=ubuntu\nVERSION_ID="22.04"\nVERSION_CODENAME=jammy\nPRETTY_NAME="Ubuntu 22.04"' \
        "jammy-pgdg main"
    expect_plan "Ubuntu 24.04 targets noble-pgdg" ubuntu2404 \
        $'ID=ubuntu\nVERSION_ID="24.04"\nVERSION_CODENAME=noble\nPRETTY_NAME="Ubuntu 24.04"' \
        "noble-pgdg main"
    expect_plan "AlmaLinux 9 enables crb with dnf" almalinux9 \
        $'ID=almalinux\nVERSION_ID="9.8"\nPRETTY_NAME="AlmaLinux 9.8"' \
        "dnf config-manager --set-enabled crb" "subscription-manager"
    expect_plan "Rocky 9 enables crb with dnf" rocky9 \
        $'ID="rocky"\nVERSION_ID="9.6"\nPRETTY_NAME="Rocky Linux 9.6"' \
        "dnf config-manager --set-enabled crb" "subscription-manager"
    expect_plan "CentOS Stream 9 enables crb with dnf" centos9 \
        $'ID="centos"\nVERSION_ID="9"\nPRETTY_NAME="CentOS Stream 9"' \
        "dnf config-manager --set-enabled crb" "subscription-manager"
    # The one that motivated the split.
    expect_plan "RHEL 9 uses subscription-manager and the CRB repository id" rhel9 \
        $'ID="rhel"\nVERSION_ID="9.6"\nPRETTY_NAME="Red Hat Enterprise Linux 9.6"' \
        "subscription-manager repos --enable \"codeready-builder-for-rhel-9-" \
        "--set-enabled crb"
    # EPEL is the other name that is not the same everywhere. `dnf install
    # epel-release` resolves on the rebuilds, which carry it in `extras`; on
    # subscribed RHEL the package does not exist under that name and the
    # documented path is Fedora's release RPM by URL. CentOS Stream also wants
    # epel-next-release.
    expect_plan "AlmaLinux 9 installs epel-release by name" almalinux9-epel \
        $'ID=almalinux\nVERSION_ID="9.8"\nPRETTY_NAME="AlmaLinux 9.8"' \
        "dnf install -y epel-release" "epel-next-release"
    expect_plan "CentOS Stream 9 also installs epel-next-release" centos9-epel \
        $'ID="centos"\nVERSION_ID="9"\nPRETTY_NAME="CentOS Stream 9"' \
        "dnf install -y epel-release epel-next-release"
    expect_plan "RHEL 9 installs EPEL from Fedora's release RPM" rhel9-epel \
        $'ID="rhel"\nVERSION_ID="9.6"\nPRETTY_NAME="Red Hat Enterprise Linux 9.6"' \
        "dnf install -y https://dl.fedoraproject.org/pub/epel/epel-release-latest-9.noarch.rpm" \
        "dnf install -y epel-release"
    # CRB before EPEL: on RHEL, EPEL's own instructions require it.
    if [[ "$(plan_for rhel9-order $'ID="rhel"\nVERSION_ID="9.6"\nPRETTY_NAME="RHEL 9.6"' \
             | grep -nE 'subscription-manager repos|epel-release-latest' | cut -d: -f1 | paste -sd' ')" \
          == "$(plan_for rhel9-order $'ID="rhel"\nVERSION_ID="9.6"\nPRETTY_NAME="RHEL 9.6"' \
             | grep -nE 'subscription-manager repos|epel-release-latest' | cut -d: -f1 | sort -n | paste -sd' ')" ]]; then
        ok "CodeReady Builder is enabled before EPEL is installed"
    else
        bad "the RHEL plan installs EPEL before enabling CodeReady Builder"
    fi

    # Every RPM plan verifies the repository RPM against *only* the pinned key.
    # `rpm --checksig` against the host database proves the package is signed by
    # some key the host already trusts, which is a much weaker statement.
    expect_plan "the EL plan checks the repository RPM in an isolated rpmdb" almalinux9-sig \
        $'ID=almalinux\nVERSION_ID="9.8"\nPRETTY_NAME="AlmaLinux 9.8"' \
        "rpm --dbpath /tmp/pgdg-checkdb --checksig /tmp/pgdg-repo.rpm"
    # The printed plan must be the plan, not a differently-broken copy of it:
    # `producer | grep -q` dies of SIGPIPE under `set -o pipefail`, which is why
    # the implementation captures first. A pasteable plan that still shows the
    # fragile form teaches the wrong thing.
    expect_plan "the printed plan avoids the pipefail-fragile grep -q form" debian12-safe \
        $'ID=debian\nVERSION_ID="12"\nVERSION_CODENAME=bookworm\nPRETTY_NAME="Debian 12"' \
        'grep -qxF' '| grep -qx '
fi

if case_ "the prerequisite plan follows PGDG's per-architecture signing keys"; then
    # PGDG signs x86_64 and aarch64 with different keys. Verifying an aarch64
    # repository RPM against the x86_64 fingerprint fails closed — correctly,
    # and only on an arm64 runner twenty minutes into a packaging job. This
    # asserts it from either architecture, in the fast job.
    el9_osr="${WORK}/os-release-el9-arch"
    printf '%s\n' $'ID=almalinux\nVERSION_ID="9.8"\nPRETTY_NAME="AlmaLinux 9.8"' > "${el9_osr}"
    for want_arch in x86_64 aarch64; do
        if [[ "${want_arch}" == aarch64 ]]; then
            want_key="PGDG-RPM-GPG-KEY-AARCH64-RHEL"; want_fpr="${PGDG_RPM_KEY_AARCH64_FINGERPRINT}"
        else
            want_key="PGDG-RPM-GPG-KEY-RHEL"; want_fpr="${PGDG_RPM_KEY_FINGERPRINT}"
        fi
        arch_plan="$(POSTVEC_OS_RELEASE="${el9_osr}" POSTVEC_UNAME_M="${want_arch}" \
            bash "${PKG_DIR}/scripts/postvec-prerequisites.sh" --print 2>/dev/null)" || arch_plan=""
        if grep -qF "keys/${want_key}" <<<"${arch_plan}" \
            && grep -qF "${want_fpr}" <<<"${arch_plan}" \
            && grep -qF "EL-9-${want_arch}/" <<<"${arch_plan}"; then
            ok "the ${want_arch} plan fetches ${want_key} and checks ${want_fpr}"
        else
            bad "the ${want_arch} plan does not pair EL-9-${want_arch} with ${want_key}"
        fi
    done
fi

if case_ "the prerequisite bootstrap pins the same PGDG key as the builders"; then
    # The script ships to users standalone, so it carries the fingerprint as a
    # constant rather than reading versions.env. That is the right shape and it
    # is also a second copy, which is what this asserts away.
    script_fpr="$(sed -n 's/^PGDG_KEY_FINGERPRINT=//p' "${PKG_DIR}/scripts/postvec-prerequisites.sh")"
    if [[ "${script_fpr}" == "${PGDG_DEBIAN_KEY_FINGERPRINT}" ]]; then
        ok "postvec-prerequisites.sh verifies the Debian key ${script_fpr}"
    else
        bad "the bootstrap pins ${script_fpr:-<nothing>}, versions.env pins ${PGDG_DEBIAN_KEY_FINGERPRINT}"
    fi
    rpm_fpr="$(sed -n 's/^PGDG_RPM_KEY_FINGERPRINT=//p' "${PKG_DIR}/scripts/postvec-prerequisites.sh")"
    if [[ "${rpm_fpr}" == "${PGDG_RPM_KEY_FINGERPRINT}" ]]; then
        ok "postvec-prerequisites.sh verifies the RPM key ${rpm_fpr}"
    else
        bad "the bootstrap pins ${rpm_fpr:-<nothing>}, versions.env pins ${PGDG_RPM_KEY_FINGERPRINT}"
    fi
    arm_fpr="$(sed -n 's/^PGDG_RPM_KEY_AARCH64_FINGERPRINT=//p' "${PKG_DIR}/scripts/postvec-prerequisites.sh")"
    if [[ "${arm_fpr}" == "${PGDG_RPM_KEY_AARCH64_FINGERPRINT}" ]]; then
        ok "postvec-prerequisites.sh verifies the aarch64 RPM key ${arm_fpr}"
    else
        bad "the bootstrap pins ${arm_fpr:-<nothing>}, versions.env pins ${PGDG_RPM_KEY_AARCH64_FINGERPRINT}"
    fi
    # …and it must still refuse to run anything unattended without consent.
    if output="$(printf '' | bash "${PKG_DIR}/scripts/postvec-prerequisites.sh" --pg 18 2>&1)"; then
        bad "the bootstrap proceeded without --yes on a non-terminal"
    elif grep -qF -- "nothing was changed" <<<"${output}"; then
        ok "the bootstrap refuses to act without consent"
    else
        # A non-root or unsupported host refuses earlier, which is also correct;
        # what must never happen is silent action.
        ok "the bootstrap refused to act (${output##*error: })"
    fi

    # A distribution this project has never run against gets the commands
    # printed, not executed. Generating the right text and surviving execution
    # are different claims, and only one of them has evidence.
    #
    # Asserted with --yes, which is the case that matters: an unattended run on
    # an untested host must still stop.
    osr="${WORK}/os-release-untested"
    printf 'ID="rhel"\nVERSION_ID="9.6"\nPRETTY_NAME="Red Hat Enterprise Linux 9.6"\n' > "${osr}"
    # The exact refusal, not merely *a* refusal. The invariant is that the
    # unrehearsed check fires **before** the root check — otherwise a root
    # operator on RHEL, which is the case that matters, would sail past it. A
    # test that accepted "must run as root" would pass whether or not that
    # ordering held, which is the same as not testing it.
    if output="$(POSTVEC_OS_RELEASE="${osr}" \
                 bash "${PKG_DIR}/scripts/postvec-prerequisites.sh" --pg 18 --yes 2>&1)"; then
        bad "the bootstrap ran unattended on a distribution postvec does not test"
    elif grep -qF -- "has not rehearsed this bootstrap" <<<"${output}"; then
        ok "an untested distribution is printed, not executed"
    else
        bad "the untested-distribution refusal did not fire first: ${output##*error: }"
    fi
    # …and that the escape hatch is spelled out rather than implied.
    grep -qF -- "--force-untested" "${PKG_DIR}/scripts/postvec-prerequisites.sh" \
        && ok "the refusal names the deliberate override" \
        || bad "there is no documented way to proceed on an untested distribution"
fi

if case_ "versions.env: it is data, not shell"; then
    check_versions_env "a command substitution is refused" \
        "looks like shell, not data" 's|^VENDOR=.*|VENDOR=$(id)|'
    check_versions_env "a backtick is refused" \
        "looks like shell, not data" 's|^VENDOR=.*|VENDOR=`id`|'
    check_versions_env "a line that is not an assignment is refused" \
        "not a KEY=VALUE assignment" 's|^VENDOR=.*|rm -rf /|'
    check_versions_env "an unfilled placeholder is refused" \
        "still contains placeholders" 's|^SYFT_VERSION=.*|SYFT_VERSION=<SYFT_VERSION>|'
    check_versions_env "an empty required pin is refused" \
        "SYFT_VERSION is unset or empty" 's|^SYFT_VERSION=.*|SYFT_VERSION=|'
fi

if case_ "model facts: a payload built from different pins is refused"; then
    # `load_model_facts` is what stops one model being packaged under another
    # one's name or version. Checking only the model name and the archive
    # digest let a changed package suffix, registry revision or bundle version
    # through — same bytes, stale package identity, and a package that
    # installs cleanly and upgrades wrongly.
    facts_refused() {
        local label="$1" expected="$2" mutation="$3"
        local dir="${WORK}/facts-${RANDOM}${RANDOM}"
        mkdir -p "${dir}"
        make_model_payload "${dir}"
        sed -i "${mutation}" "${dir}/model-facts.env"
        local output status=0
        output="$(
            set +e
            # A subshell, because load_model_facts exports into the caller and
            # a mutated fact must not leak into the next case.
            ( source "${PKG_DIR}/scripts/lib.sh"; load_versions
              load_model_facts "${dir}" ) 2>&1
        )" || status=$?
        if (( status == 0 )); then
            bad "${label} — accepted a stale payload"
        elif grep -qF -- "${expected}" <<<"${output}"; then
            ok "${label}"
        else
            bad "${label} — refused, but not for the stated reason (wanted: ${expected})"
            printf '%s\n' "${output}" | sed 's/^/          /' >&2
        fi
    }

    facts_refused "a payload for a different model is refused" \
        "MODEL_NAME:" 's|^MODEL_NAME=.*|MODEL_NAME=some-other-model|'
    facts_refused "a payload from a different archive is refused" \
        "MODEL_ARCHIVE_SHA256:" \
        "s|^MODEL_ARCHIVE_SHA256=.*|MODEL_ARCHIVE_SHA256=$(printf 'b%.0s' {1..64})|"
    facts_refused "a payload from a superseded registry revision is refused" \
        "MODEL_REGISTRY_REVISION:" \
        's|^MODEL_REGISTRY_REVISION=.*|MODEL_REGISTRY_REVISION=99|'
    facts_refused "a payload from a different bundle version is refused" \
        "MODEL_BUNDLE_VERSION:" \
        's|^MODEL_BUNDLE_VERSION=.*|MODEL_BUNDLE_VERSION=7|'
    facts_refused "a payload naming a different package is refused" \
        "MODEL_PKG_NAME:" \
        's|^MODEL_PKG_NAME=.*|MODEL_PKG_NAME=postvec-model-something-else|'
    facts_refused "a package version that does not follow the pins is refused" \
        "MODEL_PKG_VERSION:" \
        's|^MODEL_PKG_VERSION=.*|MODEL_PKG_VERSION=1.1.0|'
    facts_refused "a doc directory that does not follow the package name is refused" \
        "MODEL_DOC_DIR:" \
        's|^MODEL_DOC_DIR=.*|MODEL_DOC_DIR=/usr/share/doc/postvec-model-stale|'

    # …and the unmutated fixture must still be accepted, or the six cases above
    # would pass against a helper that refuses everything.
    d="${WORK}/facts-good"
    mkdir -p "${d}"
    make_model_payload "${d}"
    if ( source "${PKG_DIR}/scripts/lib.sh"; load_versions; load_model_facts "${d}" ) >/dev/null 2>&1
    then
        ok "a payload built from the current pins is accepted"
    else
        bad "the current pins' own facts file was refused"
    fi
fi

# ======================================================= the bundled-model gate
#
# scripts/check-model-bundle.py is the trust decision between "the registry
# served us something" and "this may be packaged and redistributed". It is
# network-free by construction, so it can be driven from fixtures — which is
# the only way to test what it *refuses*, since a real pull is always correct.
#
# Each case starts from a valid installed model, breaks exactly one thing, and
# asserts both the refusal and the reason for it.

FIXTURE_DIGEST="${BUNDLED_MODEL_ARCHIVE_SHA256}"

# `make_installed_model <dir> [<python mutation>]`
#
# Writes an engine model directory the way `postvec model pull --path` leaves
# one: the archive's files, its descriptor, and the `.postvec-install.json`
# receipt. The mutation runs with the receipt as `r` and the descriptor as `d`,
# and with `model_dir` available for the cases that add a file.
make_installed_model() {
    local dir="$1" mutation="${2:-}"
    rm -rf "${dir}"
    MUTATION="${mutation}" python3 - "${dir}" "${BUNDLED_MODEL_NAME}" \
        "${FIXTURE_DIGEST}" "${BUNDLED_MODEL_REGISTRY_REVISION}" \
        "${FIXTURE_SOURCE}" "${FIXTURE_ARCHIVE_SIZE}" <<'PY'
import hashlib, json, os, pathlib, sys

model_dir = pathlib.Path(sys.argv[1])
name, digest, revision, source, size = sys.argv[2:7]

descriptor = {
    "name": name,
    "backend": "onnx-runtime",
    "enabled": True,
    "execution_providers": ["cuda", "cpu"],
    "file_path": "onnx/model.onnx",
    "executor": {
        "key": "transformer-sequence-embedding",
        "params": {"tokenizer": {"tokenizer_type": "huggingface-pretrained",
                                 "params": {"pretrained_vocab_file": "assets/tokenizer.json"}}},
    },
    "params": {"model_type": "embed", "sequence_len": 256, "target_dim": 384},
}
receipt = {
    "schema_version": 1,
    "name": name,
    "backend": "onnx-runtime",
    "model_type": "embed",
    "access": "public",
    "license": "apache-2.0",
    "source": source,
    "source_host": "univec-registry-public.s3.eu-west-1.amazonaws.com",
    "archive_digest": "sha256:%s" % digest,
    "archive_size": int(size),
    "revision": int(revision),
    "identity": {"target_dim": 384},
    "dependencies": [],
    "postvec_requires": [],
    "registry_schema_version": 1,
    "installed_at": "2026-08-14T19:01:07Z",
    "cli_version": "0.1.0",
}
r, d = receipt, descriptor
exec(os.environ.get("MUTATION", ""))

(model_dir / "onnx").mkdir(parents=True)
(model_dir / "assets").mkdir(parents=True)
(model_dir / "onnx/model.onnx").write_bytes(b"ONNX fixture graph, not a real model\n")
(model_dir / "assets/tokenizer.json").write_text('{"fixture": true}\n')
(model_dir / "LICENSE").write_text("Apache License 2.0 (fixture)\n")
(model_dir / "ninference.hub.json").write_text(json.dumps(d, indent=2) + "\n")

# `extra_graph` is a mutation hook: a file the descriptor never references,
# which is exactly what an untrimmed publication looks like.
for extra in json.loads(os.environ.get("EXTRA_FILES", "[]")):
    path = model_dir / extra
    path.parent.mkdir(parents=True, exist_ok=True)
    path.write_bytes(b"unreferenced fixture graph\n")

files = []
for path in sorted(model_dir.rglob("*")):
    if not path.is_file():
        continue
    body = path.read_bytes()
    files.append({"path": str(path.relative_to(model_dir)),
                  "size": len(body),
                  "sha256": hashlib.sha256(body).hexdigest()})
r["files"] = files
(model_dir / ".postvec-install.json").write_text(json.dumps(r, indent=2) + "\n")
PY
}

# `run_checker <model-dir> <metadata-dir>` with this release's real pins.
run_checker() {
    CHECKER_OUTPUT="$(
        BUNDLED_MODEL_NAME="${BUNDLED_MODEL_NAME}" \
        BUNDLED_MODEL_REGISTRY_REVISION="${BUNDLED_MODEL_REGISTRY_REVISION}" \
        BUNDLED_MODEL_ARCHIVE_SHA256="${FIXTURE_DIGEST}" \
        BUNDLED_MODEL_PKG_SUFFIX="${BUNDLED_MODEL_PKG_SUFFIX}" \
        BUNDLED_MODEL_BUNDLE_VERSION="${BUNDLED_MODEL_BUNDLE_VERSION}" \
        "${PKG_DIR}/scripts/check-model-bundle.py" "$1" "$2" 2>&1
    )"
}

# `expect_model_refused <label> <fragment> <python mutation>`
expect_model_refused() {
    local label="$1" fragment="$2" mutation="$3"
    local dir="${WORK}/model-${RANDOM}${RANDOM}"
    make_installed_model "${dir}/model" "${mutation}"
    if run_checker "${dir}/model" "${dir}/meta"; then
        bad "${label} — the checker accepted it"
        return
    fi
    if grep -qF -- "${fragment}" <<<"${CHECKER_OUTPUT}"; then
        ok "${label}"
    else
        bad "${label} — refused, but not for the stated reason (wanted: ${fragment})"
        printf '%s\n' "${CHECKER_OUTPUT}" | sed 's/^/          /' >&2
    fi
    # Nothing may be emitted for a model that was refused: a later stage that
    # found a facts file would package a model this one rejected.
    [[ -e "${dir}/meta/model-facts.env" ]] \
        && bad "${label} — wrote model-facts.env for a refused model"
    return 0
}

if case_ "bundled model: a valid public pull is accepted"; then
    d="${WORK}/model-happy"
    make_installed_model "${d}/model"
    if run_checker "${d}/model" "${d}/meta"; then
        ok "a public, pinned, single-graph embed model passes"
    else
        bad "a correct fixture was refused:"
        printf '%s\n' "${CHECKER_OUTPUT}" | sed 's/^/          /' >&2
    fi

    # The derived facts are the whole point of the pass: every later stage
    # reads its package name, version and dimension from them.
    ( set -a; source "${d}/meta/model-facts.env"; set +a
      [[ "${MODEL_PKG_NAME}" == "postvec-model-${BUNDLED_MODEL_PKG_SUFFIX}" ]] || exit 1
      [[ "${MODEL_PKG_VERSION}" == "${BUNDLED_MODEL_REGISTRY_REVISION}.${BUNDLED_MODEL_BUNDLE_VERSION}.0" ]] || exit 1
      [[ "${MODEL_LICENSE}" == "Apache-2.0" ]] || exit 1
      [[ "${MODEL_TARGET_DIM}" == 384 && "${MODEL_SEQUENCE_LEN}" == 256 ]] || exit 1
      [[ "${MODEL_BACKEND}" == onnx-runtime ]] || exit 1 ) \
        && ok "model-facts.env derives the package identity from the pins" \
        || bad "model-facts.env is wrong: $(cat "${d}/meta/model-facts.env")"

    # The free-form registry `source` must not reach a file that shell sources.
    grep -q 'https://' "${d}/meta/model-facts.env" \
        && bad "model-facts.env carries the registry's free-form source string" \
        || ok "model-facts.env carries no free-form registry text"

    grep -q "Upstream-Name: model" "${d}/meta/copyright" \
        && ok "the DEP-5 copyright names the upstream derived from the source URL" \
        || bad "the copyright's Upstream-Name is wrong: $(sed -n 2p "${d}/meta/copyright")"
    grep -q '/usr/share/common-licenses/Apache-2.0' "${d}/meta/copyright" \
        && ok "an Apache-2.0 model references Debian's common licence copy" \
        || bad "the copyright repeats the licence instead of referencing it"

    python3 -c "
import json,sys
d=json.load(open('${d}/meta/SOURCE.json'))
assert d['channel']=='public', d
assert d['registry_revision']==${BUNDLED_MODEL_REGISTRY_REVISION}, d
assert d['archive_sha256']=='${FIXTURE_DIGEST}', d
assert set(d['files_sha256']) >= {'LICENSE','onnx/model.onnx'}, d
assert 'installed_at' not in json.dumps(d), d
" && ok "SOURCE.json records the registry identity and no install timestamp" \
      || bad "SOURCE.json is wrong: $(cat "${d}/meta/SOURCE.json")"
fi

if case_ "bundled model: the pins and the policy are enforced"; then
    expect_model_refused "a different archive digest is refused" \
        "archive digest mismatch" \
        "r['archive_digest'] = 'sha256:' + '0'*64"

    expect_model_refused "a moved head is refused" \
        "registry revision mismatch" \
        "r['revision'] = r['revision'] + 1"

    expect_model_refused "a private model is refused" \
        "publicly redistributable" \
        "r['access'] = 'private'"

    expect_model_refused "local licence acknowledgement is not packaging authority" \
        "not authority to redistribute" \
        "r['license_accepted_at'] = '2026-08-14T19:00:00Z'; r['license_acceptance_method'] = 'interactive'"

    expect_model_refused "a dependency closure is refused" \
        "dependencies is not empty" \
        "r['dependencies'] = ['some-other-model']"

    expect_model_refused "a postvec_requires closure is refused" \
        "postvec_requires is not empty" \
        "r['postvec_requires'] = ['0.2.0']"

    expect_model_refused "a model under another name is refused" \
        "versions.env pins" \
        "r['name'] = 'someone-elses-model'; d['name'] = 'someone-elses-model'"

    expect_model_refused "a backend the shipped extension does not have is refused" \
        "onnx-runtime backend" \
        "r['backend'] = 'candle'; d['backend'] = 'candle'"

    expect_model_refused "a model that cannot be embedded directly is refused" \
        "directly embeddable" \
        "r['model_type'] = 'convert'; d['params']['model_type'] = 'convert'"

    expect_model_refused "a receipt with no identity block is refused" \
        "vector space this model produces is unknown" \
        "del r['identity']"

    expect_model_refused "a descriptor with no sequence_len is refused" \
        "sequence_len is absent" \
        "del d['params']['sequence_len']"

    expect_model_refused "an identity the engine will not agree with is refused" \
        "identity field target_dim disagrees" \
        "d['params']['target_dim'] = 768"

    expect_model_refused "a receipt with no licence is refused" \
        "records no \`license\`" \
        "del r['license']"

    expect_model_refused "a licence packaging has not reviewed is refused" \
        "approved set" \
        "r['license'] = 'some-bespoke-eula'"

    expect_model_refused "a source that is not one https URL is refused" \
        "single-line absolute https:// URL" \
        "r['source'] = 'internal notes\nabout where this came from'"

    expect_model_refused "a receipt from a newer CLI is refused" \
        "newer postvec CLI" \
        "r['schema_version'] = 2"

    expect_model_refused "an index from a newer registry is refused" \
        "newer registry" \
        "r['registry_schema_version'] = 2"
fi

if case_ "bundled model: an untrimmed archive is refused, not pruned"; then
    # Registry revision 1 of the bundled model shipped all nine ONNX graphs —
    # 499 MB where 91 MB is referenced. Packaging must refuse it and point at
    # the publisher's `exclude`, because pruning after extraction would
    # invalidate the pull's own per-file receipt.
    d="${WORK}/model-untrimmed"
    EXTRA_FILES='["onnx/model_qint8_avx512.onnx", "onnx/model_O2.onnx"]' \
        make_installed_model "${d}/model"
    if run_checker "${d}/model" "${d}/meta"; then
        bad "an archive carrying unreferenced ONNX graphs was accepted"
    elif grep -qF -- "--exclude 'onnx/model_*.onnx'" <<<"${CHECKER_OUTPUT}"; then
        ok "an unreferenced ONNX graph is refused, naming the publication fix"
    else
        bad "refused, but without naming the publication fix:"
        printf '%s\n' "${CHECKER_OUTPUT}" | sed 's/^/          /' >&2
    fi
fi

if case_ "bundled model: the payload is the archive, and only the archive"; then
    # The end-to-end shape of build-model-bundle.sh, driven with a stub CLI so
    # the assertions cost no network and no 90 MB download. What it proves:
    # the `model show --verify` gate runs before anything is copied, `--from`
    # pulls nothing, and the receipt itself never reaches the payload.
    d="${WORK}/model-bundle-e2e"
    mkdir -p "${d}/root/models/onnx-runtime" "${d}/bin"
    make_installed_model "${d}/root/models/onnx-runtime/${BUNDLED_MODEL_NAME}"

    cat > "${d}/bin/postvec" <<'STUB'
#!/usr/bin/env bash
printf '%s\n' "$*" >> "${STUB_LOG}"
case "$1" in
--version) echo "postvec 0.0.0-stub" ;;
model)
    case "$2" in
    show) echo "verify:    every file matches the receipt" ;;
    pull) echo "stub: the bundle step must not pull with --from" >&2; exit 9 ;;
    esac ;;
esac
STUB
    chmod +x "${d}/bin/postvec"

    # The real payload is set aside: this case writes a fixture model into the
    # same directory a release build uses, and a developer's built payload must
    # survive running the tests.
    payload="${PKG_DIR}/build/payload-common"
    saved=""
    if [[ -d "${payload}" ]]; then saved="${payload}.unit-test-saved"; rm -rf "${saved}"; mv "${payload}" "${saved}"; fi

    : > "${d}/stub.log"
    if STUB_LOG="${d}/stub.log" "${PKG_DIR}/scripts/build-model-bundle.sh" \
            --cli "${d}/bin/postvec" --from "${d}/root" > "${d}/out.log" 2>&1; then
        ok "build-model-bundle.sh --from produces a payload with no registry access"
    else
        bad "build-model-bundle.sh --from failed:"
        sed 's/^/          /' "${d}/out.log" >&2
    fi

    grep -q "model show --path ${d}/root --verify ${BUNDLED_MODEL_NAME}" "${d}/stub.log" \
        && ok "the CLI's own receipt verification runs before anything is copied" \
        || bad "model show --verify was not invoked: $(cat "${d}/stub.log")"
    grep -q 'model pull' "${d}/stub.log" \
        && bad "--from reached the registry" \
        || ok "--from pulls nothing"

    model_out="${payload}/opt/postvec/ninference/models/onnx-runtime/${BUNDLED_MODEL_NAME}"
    if [[ -d "${model_out}" ]]; then
        got="$( ( cd "${model_out}" && find . -type f -printf '%P\n' | LC_ALL=C sort ) | tr '\n' ' ')"
        want="LICENSE assets/tokenizer.json ninference.hub.json onnx/model.onnx "
        [[ "${got}" == "${want}" ]] \
            && ok "the payload is exactly the archive-owned files" \
            || bad "the payload is '${got}', expected '${want}'"
        [[ -e "${model_out}/.postvec-install.json" ]] \
            && bad "the CLI install receipt reached the payload" \
            || ok "the CLI install receipt is not package content"
        [[ -f "${payload}/usr/share/doc/postvec-model-${BUNDLED_MODEL_PKG_SUFFIX}/copyright" \
           && -f "${payload}/usr/share/doc/postvec-model-${BUNDLED_MODEL_PKG_SUFFIX}/SOURCE.json" \
           && -f "${payload}/usr/share/doc/postvec-model-${BUNDLED_MODEL_PKG_SUFFIX}/model-files.sha256" ]] \
            && ok "provenance lands in the package doc directory, not in the model directory" \
            || bad "the doc directory is incomplete: $(ls "${payload}/usr/share/doc/"* 2>&1)"
        [[ -f "${payload}/model-facts.env" ]] \
            && ok "model-facts.env is written last, as the completeness marker" \
            || bad "no model-facts.env"
    else
        bad "no model directory at ${model_out}"
    fi

    # Determinism, the offline half. Rebuild from a receipt whose timestamps
    # and CLI version differ and require identical payload bytes. That fails
    # the moment `.postvec-install.json` is copied, or anything derived from
    # `installed_at` lands in SOURCE.json or model-facts.env.
    first="$(find "${payload}" -type f -print0 | sort -z | xargs -0 sha256sum | sha256sum)"

    receipt="${d}/root/models/onnx-runtime/${BUNDLED_MODEL_NAME}/.postvec-install.json"
    python3 - "${receipt}" <<'PY'
import json, sys
path = sys.argv[1]
receipt = json.loads(open(path).read())
receipt["installed_at"] = "2027-01-02T03:04:05Z"
receipt["cli_version"] = "9.9.9"
open(path, "w").write(json.dumps(receipt, indent=2) + "\n")
PY
    if STUB_LOG="${d}/stub.log" "${PKG_DIR}/scripts/build-model-bundle.sh" \
            --cli "${d}/bin/postvec" --from "${d}/root" >> "${d}/out.log" 2>&1; then
        second="$(find "${payload}" -type f -print0 | sort -z | xargs -0 sha256sum | sha256sum)"
        [[ "${first}" == "${second}" ]] \
            && ok "a receipt's install timestamp and CLI version do not reach the payload" \
            || bad "the payload changed when only the receipt's timestamps did"
    else
        bad "the rebuild from a re-timestamped receipt failed:"
        tail -5 "${d}/out.log" >&2
    fi

    rm -rf "${payload}"
    [[ -n "${saved}" ]] && mv "${saved}" "${payload}"
fi

if case_ "bundled model: an interrupted cache promotion is recovered"; then
    # Promotion is two same-filesystem renames. A kill between them leaves the
    # canonical cache absent and a complete copy in `.replaced` — nothing
    # corrupt, but the next build would pull again, which is the one step that
    # needs the network. This is the offline-rebuild path, so it has to survive
    # that.
    d="${WORK}/model-cache-recovery"
    mkdir -p "${d}/bin"
    cat > "${d}/bin/postvec" <<'STUB'
#!/usr/bin/env bash
printf '%s\n' "$*" >> "${STUB_LOG}"
case "$1" in
--version) echo "postvec 0.0.0-stub" ;;
model)
    case "$2" in
    show) echo "verify:    every file matches the receipt" ;;
    pull) echo "stub: this build should not have needed the registry" >&2; exit 9 ;;
    esac ;;
esac
STUB
    chmod +x "${d}/bin/postvec"

    # The real cache and payload use the same paths a release build does, so
    # both are set aside and put back.
    cache="${PKG_DIR}/build/model-cache/sha256-${BUNDLED_MODEL_ARCHIVE_SHA256}"
    payload="${PKG_DIR}/build/payload-common"
    saved_cache=""; saved_payload=""
    if [[ -d "${cache}" ]]; then saved_cache="${cache}.unit-test-saved"; rm -rf "${saved_cache}"; mv "${cache}" "${saved_cache}"; fi
    if [[ -d "${payload}" ]]; then saved_payload="${payload}.unit-test-saved"; rm -rf "${saved_payload}"; mv "${payload}" "${saved_payload}"; fi

    # The interrupted state: no canonical cache, a complete one in `.replaced`.
    rm -rf "${cache}" "${cache}.replaced" "${cache}.incoming"
    mkdir -p "${cache}.replaced/models/onnx-runtime"
    make_installed_model "${cache}.replaced/models/onnx-runtime/${BUNDLED_MODEL_NAME}"

    : > "${d}/stub.log"
    if STUB_LOG="${d}/stub.log" "${PKG_DIR}/scripts/build-model-bundle.sh" \
            --cli "${d}/bin/postvec" > "${d}/out.log" 2>&1; then
        ok "an interrupted promotion is finished instead of pulling again"
    else
        bad "the build did not recover the interrupted promotion:"
        tail -6 "${d}/out.log" >&2
    fi
    grep -q 'model pull' "${d}/stub.log" \
        && bad "the recovered cache was not used — the build went to the registry" \
        || ok "the recovered cache is used, with no registry access"
    [[ -d "${cache}/models" ]] \
        && ok "the canonical cache is back in place" \
        || bad "the cache was not restored to ${cache}"
    [[ -e "${cache}.replaced" || -e "${cache}.incoming" ]] \
        && bad "promotion debris was left behind" \
        || ok "no .replaced or .incoming debris remains"

    # `.incoming` is the other half of the rule, and the opposite decision: an
    # interrupted cross-filesystem copy leaves a partial one and nothing can
    # tell it from a complete one, so it is debris and a fresh pull is correct.
    rm -rf "${cache}" "${cache}.replaced" "${cache}.incoming"
    mkdir -p "${cache}.incoming/models/onnx-runtime"
    make_installed_model "${cache}.incoming/models/onnx-runtime/${BUNDLED_MODEL_NAME}"
    : > "${d}/stub.log"
    STUB_LOG="${d}/stub.log" "${PKG_DIR}/scripts/build-model-bundle.sh" \
        --cli "${d}/bin/postvec" > "${d}/out2.log" 2>&1 || true
    grep -q 'model pull' "${d}/stub.log" \
        && ok "a possibly-partial .incoming is not promoted; the build pulls instead" \
        || bad "a .incoming directory was trusted as a cache"
    [[ -e "${cache}.incoming" ]] \
        && bad ".incoming debris was left behind" \
        || ok ".incoming is removed as debris"

    rm -rf "${cache}" "${cache}.replaced" "${cache}.incoming" "${payload}"
    [[ -n "${saved_cache}" ]] && mv "${saved_cache}" "${cache}"
    [[ -n "${saved_payload}" ]] && mv "${saved_payload}" "${payload}"
fi

if case_ "package verify: RPM directory entries are not stray model files"; then
    # The regression. dpkg lists directories with a trailing slash and the
    # check skipped those; rpm -qpl lists them without one, so a well-formed
    # model tree was reported as packaged outside models/<backend>/<name>/.
    # The expensive matrix never saw this on Ubuntu-only runs.
    model="${BUNDLED_MODEL_NAME}"
    rpm_listing="$(cat <<LIST
/opt/postvec/ninference/models/onnx-runtime
/opt/postvec/ninference/models/onnx-runtime/${model}
/opt/postvec/ninference/models/onnx-runtime/${model}/LICENSE
/opt/postvec/ninference/models/onnx-runtime/${model}/assets
/opt/postvec/ninference/models/onnx-runtime/${model}/assets/tokenizer.json
/opt/postvec/ninference/models/onnx-runtime/${model}/ninference.hub.json
/opt/postvec/ninference/models/onnx-runtime/${model}/onnx
/opt/postvec/ninference/models/onnx-runtime/${model}/onnx/model.onnx
/usr/share/doc/postvec-model-${BUNDLED_MODEL_PKG_SUFFIX}/SOURCE.json
LIST
)"
    deb_listing="$(cat <<LIST
./opt/postvec/ninference/models/
./opt/postvec/ninference/models/onnx-runtime/
./opt/postvec/ninference/models/onnx-runtime/${model}/
./opt/postvec/ninference/models/onnx-runtime/${model}/LICENSE
./opt/postvec/ninference/models/onnx-runtime/${model}/ninference.hub.json
./opt/postvec/ninference/models/onnx-runtime/${model}/onnx/
./opt/postvec/ninference/models/onnx-runtime/${model}/onnx/model.onnx
LIST
)"

    layout() {
        printf '%s\n' "$1" | "${PKG_DIR}/scripts/verify-package.sh" --assert-model-layout 2>&1
    }

    if out="$(layout "${rpm_listing}")"; then
        ok "an RPM-style listing of a well-formed model tree is accepted"
    else
        bad "an RPM-style listing was refused:"
        printf '%s\n' "${out}" | sed 's/^/          /' >&2
    fi
    if out="$(layout "${deb_listing}")"; then
        ok "a dpkg-style listing of the same tree is still accepted"
    else
        bad "a dpkg-style listing was refused:"
        printf '%s\n' "${out}" | sed 's/^/          /' >&2
    fi

    if out="$(layout "${rpm_listing}"$'\n'"/opt/postvec/ninference/models/README")"; then
        bad "a file sitting in models/ was accepted"
    elif grep -qF "packaged outside models/<backend>/<name>/: /opt/postvec/ninference/models/README" \
            <<<"${out}"; then
        ok "a file sitting in models/ is still refused"
    else
        bad "refused, but not for the stray file:"
        printf '%s\n' "${out}" | sed 's/^/          /' >&2
    fi

    if out="$(layout "${rpm_listing}"$'\n'"/opt/postvec/ninference/models/onnx-runtime/orphan.bin")"; then
        bad "a file sitting in the backend directory was accepted"
    elif grep -qF "packaged outside models/<backend>/<name>/: /opt/postvec/ninference/models/onnx-runtime/orphan.bin" \
            <<<"${out}"; then
        ok "a file sitting in the backend directory is still refused"
    else
        bad "refused, but not for the backend-level file:"
        printf '%s\n' "${out}" | sed 's/^/          /' >&2
    fi
fi

# The reviewed files that a `.gitignore` can silently swallow.
#
# The repository root ignores `*.txt`, so each of these needs an explicit
# negation in packaging/postvec/.gitignore to be committed at all — and when
# one is missing, nothing fails on the machine that wrote it. It fails in CI,
# in whichever job first reads the file: for the lintian list that was the
# twenty-minute `packages` job, reporting three dozen errors that looked like a
# regression in the packages themselves.
#
# Asserting their presence here puts that failure in the one-minute `lint` job
# that `packages` depends on, and names the actual cause.
if case_ "reviewed policy files are present and committed"; then
    for relative in lintian-exceptions.txt requirements-schema.txt \
                    "model/golden/${BUNDLED_MODEL_NAME}.json" model/copyright.in; do
        path="${PKG_DIR}/${relative}"
        if [[ ! -s "${path}" ]]; then
            bad "${relative} is missing or empty"
            continue
        fi
        # `git check-ignore` answers the question that matters: not "is it here"
        # — it is, that is why nobody noticed — but "would a commit include it".
        if git -C "${PKG_DIR}" check-ignore -q "${path}" 2>/dev/null; then
            bad "${relative} exists but is ignored by .gitignore, so CI will not have it"
        else
            ok "${relative} is present and not ignored"
        fi
    done
fi

# ---------------------------------------------------------------------- summary

rm -rf "${PKG_DIR}/build/.unit-test-release"

printf '\n'
if (( FAILED )); then
    printf '\033[1;31m%d failed\033[0m, %d passed, %d case(s) skipped\n' \
        "${FAILED}" "${PASSED}" "${SKIPPED}" >&2
    exit 1
fi
printf '\033[1;32m%d passed\033[0m, %d case(s) skipped\n' "${PASSED}" "${SKIPPED}"
