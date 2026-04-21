#!/usr/bin/env bash
# Shared helpers for the postvec packaging scripts. Sourced, never executed.
#
# Everything here is deliberately small and boring: the packaging chain's job
# is to fail closed, not to be clever.

# shellcheck shell=bash

set -Eeuo pipefail

PKG_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
REPO_ROOT="$(cd "${PKG_DIR}/../.." && pwd)"
export PKG_DIR REPO_ROOT

# Deterministic build environment. SOURCE_DATE_EPOCH comes from the commit, so
# a rebuild of the same commit produces the same timestamps.
export TZ=UTC
export LC_ALL=C.UTF-8
export CARGO_INCREMENTAL=0
if [[ -z "${SOURCE_DATE_EPOCH:-}" ]]; then
    SOURCE_DATE_EPOCH="$(git -C "${REPO_ROOT}" show -s --format=%ct HEAD 2>/dev/null || echo 0)"
    export SOURCE_DATE_EPOCH
fi

log()  { printf '\033[1;36m==>\033[0m %s\n' "$*" >&2; }
warn() { printf '\033[1;33mwarning:\033[0m %s\n' "$*" >&2; }
die()  { printf '\033[1;31merror:\033[0m %s\n' "$*" >&2; exit 1; }

need() {
    for tool in "$@"; do
        command -v "${tool}" >/dev/null 2>&1 || die "required tool not found: ${tool}"
    done
}

# ---------------------------------------------------------------- versions.env

# Load the reviewed pins and refuse to proceed on an unfilled one.
#
# versions.env is *data*, so it is parsed rather than sourced: a release input
# file has no business being able to run commands, and parsing also catches a
# typo that `source` would silently accept as a shell construct.

# Parse a KEY=VALUE file into the environment. ${1} is the path, ${2} the label
# used in diagnostics. Shared by `load_versions` and `load_model_facts`,
# because both files are read by scripts that build what users install and
# neither has any business being able to run a command.
parse_env_file() {
    local file="$1" label="$2"
    local line key value lineno=0
    while IFS= read -r line || [[ -n "${line}" ]]; do
        lineno=$((lineno + 1))
        [[ "${line}" =~ ^[[:space:]]*(#|$) ]] && continue
        if [[ ! "${line}" =~ ^([A-Z][A-Z0-9_]*)=(.*)$ ]]; then
            die "${label}:${lineno}: not a KEY=VALUE assignment: ${line}"
        fi
        key="${BASH_REMATCH[1]}"
        value="${BASH_REMATCH[2]}"
        # One optional layer of quoting, so a value with spaces reads naturally.
        if [[ "${value}" =~ ^\"(.*)\"$ || "${value}" =~ ^\'(.*)\'$ ]]; then
            value="${BASH_REMATCH[1]}"
        fi
        if [[ "${value}" == *'$('* || "${value}" == *'`'* ]]; then
            die "${label}:${lineno}: ${key} looks like shell, not data"
        fi
        printf -v "${key}" '%s' "${value}"
        export "${key?}"
    done < "${file}"
}

load_versions() {
    local file="${PKG_DIR}/versions.env"
    [[ -f "${file}" ]] || die "missing ${file}"

    parse_env_file "${file}" versions.env

    if grep -Eq '=<[A-Z0-9_]+>' "${file}"; then
        grep -En '=<[A-Z0-9_]+>' "${file}" >&2
        die "versions.env still contains placeholders (lines above)"
    fi

    local required=(
        POSTVEC_VERSION PACKAGE_RELEASE RUST_VERSION PGRX_VERSION NFPM_IMAGE
        RUSTUP_VERSION RUSTUP_INIT_X86_64_SHA256 RUSTUP_INIT_AARCH64_SHA256
        SYFT_VERSION SYFT_LINUX_AMD64_SHA256
        BUILDX_VERSION BUILDKIT_IMAGE
        ACTIONLINT_VERSION ACTIONLINT_LINUX_AMD64_SHA256
        PROTOC_VERSION PROTOC_LINUX_X86_64_SHA256 PROTOC_LINUX_AARCH64_SHA256
        PGDG_DEBIAN_KEY_FINGERPRINT PGDG_EL9_REPO_RPM_X86_64_SHA256
        PGDG_EL9_REPO_RPM_AARCH64_SHA256
        PGDG_RPM_KEY_FINGERPRINT PGDG_RPM_KEY_URL
        PGVECTOR_MIN_VERSION ORT_VERSION ORT_URL_BASE
        ORT_LINUX_X64_SHA256 ORT_LINUX_AARCH64_SHA256
        BUNDLED_MODEL_NAME BUNDLED_MODEL_ARCHIVE_SHA256
        BUNDLED_MODEL_REGISTRY_REVISION BUNDLED_MODEL_PKG_SUFFIX
        BUNDLED_MODEL_BUNDLE_VERSION
        POSTGRES_IMAGE_PG16_DIGEST POSTGRES_IMAGE_PG17_DIGEST POSTGRES_IMAGE_PG18_DIGEST
        BUILD_BASE_DEBIAN12_DIGEST BUILD_BASE_UBUNTU2204_DIGEST
        BUILD_BASE_UBUNTU2404_DIGEST BUILD_BASE_EL9_DIGEST
        IMAGE_REPOSITORY IMAGE_STAGING_REPOSITORY SOURCE_REPOSITORY MAINTAINER VENDOR
        EXTRAS_METAPACKAGE
    )
    local name
    for name in "${required[@]}"; do
        [[ -n "${!name:-}" ]] || die "versions.env: ${name} is unset or empty"
    done

    for name in ORT_LINUX_X64_SHA256 ORT_LINUX_AARCH64_SHA256 \
                BUNDLED_MODEL_ARCHIVE_SHA256 \
                SYFT_LINUX_AMD64_SHA256 ACTIONLINT_LINUX_AMD64_SHA256 \
                PGDG_EL9_REPO_RPM_X86_64_SHA256 \
                PGDG_EL9_REPO_RPM_AARCH64_SHA256; do
        [[ "${!name}" =~ ^[0-9a-f]{64}$ ]] || die "versions.env: ${name} is not a sha256"
    done
    for name in POSTGRES_IMAGE_PG16_DIGEST POSTGRES_IMAGE_PG17_DIGEST \
                POSTGRES_IMAGE_PG18_DIGEST BUILD_BASE_DEBIAN12_DIGEST \
                BUILD_BASE_UBUNTU2204_DIGEST BUILD_BASE_UBUNTU2404_DIGEST \
                BUILD_BASE_EL9_DIGEST; do
        [[ "${!name}" =~ ^sha256:[0-9a-f]{64}$ ]] || die "versions.env: ${name} is not an image digest"
    done

    # Every remaining pin, checked for shape. A digest that is quietly the empty
    # string, or a truncated fingerprint, verifies nothing — and the failure
    # would surface as a build that succeeded against the wrong input.
    for name in RUSTUP_INIT_X86_64_SHA256 RUSTUP_INIT_AARCH64_SHA256 \
                PROTOC_LINUX_X86_64_SHA256 PROTOC_LINUX_AARCH64_SHA256; do
        [[ "${!name}" =~ ^[0-9a-f]{64}$ ]] || die "versions.env: ${name} is not a sha256"
    done
    for name in PGDG_DEBIAN_KEY_FINGERPRINT PGDG_RPM_KEY_FINGERPRINT; do
        [[ "${!name}" =~ ^[0-9A-F]{40}$ ]] \
            || die "versions.env: ${name} must be 40 uppercase hex digits"
    done
    # An image reference without a digest is a tag, and a tag is a pointer
    # somebody else can move. The *whole* suffix is checked, not just the
    # presence of the marker: `…@sha256:` and `…@sha256:deadbeef` both contain
    # it and neither pins anything.
    for name in NFPM_IMAGE BUILDKIT_IMAGE; do
        [[ "${!name}" =~ ^[^[:space:]@]+@sha256:[0-9a-f]{64}$ ]] \
            || die "versions.env: ${name} must be <reference>@sha256:<64 lowercase hex>,
not a tag and not a truncated digest (got: ${!name})"
    done
    [[ "${BUILDX_VERSION}" =~ ^v[0-9]+\.[0-9]+\.[0-9]+$ ]] \
        || die "versions.env: BUILDX_VERSION must be an exact vMAJOR.MINOR.PATCH"
    [[ "${PACKAGE_RELEASE}" =~ ^[1-9][0-9]*$ ]] \
        || die "versions.env: PACKAGE_RELEASE must be a positive integer"
    [[ "${POSTVEC_VERSION}" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] \
        || die "versions.env: POSTVEC_VERSION must be MAJOR.MINOR.PATCH"
    for name in BUNDLED_MODEL_REGISTRY_REVISION BUNDLED_MODEL_BUNDLE_VERSION; do
        [[ "${!name}" =~ ^[1-9][0-9]*$ ]] || die "versions.env: ${name} must be a positive integer"
    done
    # The suffix becomes a package name — `postvec-model-<suffix>` — which apt
    # and dnf resolve upgrades by, and which is therefore forever. Debian's
    # grammar is the stricter of the two, so it is the one checked.
    [[ "${BUNDLED_MODEL_PKG_SUFFIX}" =~ ^[a-z0-9][a-z0-9.+-]*$ ]] \
        || die "versions.env: BUNDLED_MODEL_PKG_SUFFIX must be a Debian-safe package-name
token (lowercase alphanumerics, '.', '+' and '-', starting alphanumeric)
(got: ${BUNDLED_MODEL_PKG_SUFFIX})"
    [[ "${BUNDLED_MODEL_NAME}" =~ ^[a-z0-9][a-z0-9._-]*$ ]] \
        || die "versions.env: BUNDLED_MODEL_NAME is not a registry model name
(got: ${BUNDLED_MODEL_NAME})"

    # The two container repositories are interpolated into registry commands in
    # jobs that hold package-write permission. They are reviewed repository
    # content rather than workflow input, so this is a second line rather than
    # the first — but "reviewed" is a process, and a grammar is a check.
    #
    # An OCI repository reference: an optional registry host (with an optional
    # port), then one or more lowercase path components. No tag, no digest, no
    # whitespace, and nothing a shell would find interesting.
    local oci_ref='^([a-z0-9]+([.-][a-z0-9]+)*(:[0-9]{1,5})?/)?[a-z0-9]+([._-][a-z0-9]+)*(/[a-z0-9]+([._-][a-z0-9]+)*)*$'
    for name in IMAGE_REPOSITORY IMAGE_STAGING_REPOSITORY; do
        [[ "${!name}" =~ ${oci_ref} ]] \
            || die "versions.env: ${name} is not a plain container repository reference
(got: ${!name})
It must be <host>[:<port>]/<path>, lowercase, with no tag, digest or whitespace:
the release interpolates it into registry commands that can write packages."
    done
    [[ "${IMAGE_REPOSITORY}" != "${IMAGE_STAGING_REPOSITORY}" ]] \
        || die "versions.env: IMAGE_STAGING_REPOSITORY must differ from IMAGE_REPOSITORY.
Staging exists so an untested image never appears where a user would find it."
    [[ "${SOURCE_REPOSITORY}" =~ ^https://[a-z0-9.-]+(:[0-9]{1,5})?(/[A-Za-z0-9._~-]+)+$ ]] \
        || die "versions.env: SOURCE_REPOSITORY must be a plain https URL (got: ${SOURCE_REPOSITORY})"
    # `Name <address>` — the form Debian and RPM both expect, and the form that
    # ends up in every package's metadata. Held in a variable because `[[ ]]`
    # parses a bare `<` as a comparison operator before it ever reaches `=~`.
    local maintainer_re='^[^<>]+ <[^[:space:]<>@]+@[^[:space:]<>@]+>$'
    [[ "${MAINTAINER}" =~ ${maintainer_re} ]] \
        || die "versions.env: MAINTAINER must be \"Name <address@example.org>\" (got: ${MAINTAINER})"

    # Contact and publication metadata reaches every published package and
    # image, so an unreplaced placeholder must not be able to ship. Local
    # experimentation opts out explicitly.
    if [[ "${MAINTAINER}${IMAGE_REPOSITORY}${SOURCE_REPOSITORY}" == *example* ]]; then
        if [[ "${POSTVEC_ALLOW_PLACEHOLDER_METADATA:-0}" != 1 ]]; then
            die "versions.env still carries placeholder publication metadata:
  MAINTAINER        = ${MAINTAINER}
  IMAGE_REPOSITORY  = ${IMAGE_REPOSITORY}
  SOURCE_REPOSITORY = ${SOURCE_REPOSITORY}
These end up in every package and image. Replace them before building anything
that could be published, or set POSTVEC_ALLOW_PLACEHOLDER_METADATA=1 for a
local build you will throw away."
        fi
        warn "building with placeholder publication metadata — not publishable"
    fi

    # The add-on metapackage is a pin: its name reaches every nfpm description,
    # the release closure, and user-facing install lines. It must look like a
    # Debian package name and must not reuse the mode word, the rejected
    # "engine" name, or the all-in-one image suffix.
    [[ "${EXTRAS_METAPACKAGE}" =~ ^postvec-[a-z0-9]+(-[a-z0-9]+)*$ ]] \
        || die "versions.env: EXTRAS_METAPACKAGE is not a postvec-* package name (got: ${EXTRAS_METAPACKAGE})"
    case "${EXTRAS_METAPACKAGE}" in
    *embedded*|*engine*|*complete*)
        die "versions.env: EXTRAS_METAPACKAGE must not contain embedded, engine, or complete
(got: ${EXTRAS_METAPACKAGE})
embedded is the inference mode; complete is the all-in-one image suffix; the
add-on package is extras, not a one-shot install."
        ;;
    esac

    # Public tag suffix and Dockerfile target for the image that *includes*
    # those extras. Different word from EXTRAS_METAPACKAGE on purpose: that
    # image is a complete container; the package is not a complete install.
    COMPLETE_IMAGE_VARIANT=complete
    COMPLETE_IMAGE_SUFFIX=-complete
    export COMPLETE_IMAGE_VARIANT COMPLETE_IMAGE_SUFFIX

    NEXT_BREAKING_VERSION="$(next_breaking_version "${POSTVEC_VERSION}")"
    export NEXT_BREAKING_VERSION

    # The release identity, used for the git tag and for image tags. It carries
    # the packaging revision because a packaging-only rebuild is a different
    # release: same source, different bytes, and it must not be able to
    # overwrite what the previous one published.
    RELEASE_ID="${POSTVEC_VERSION}-${PACKAGE_RELEASE}"
    RELEASE_TAG="postvec-v${RELEASE_ID}"
    # A disposable publication rehearsal lives in its own tag namespace, so the
    # *tag* decides which kind of release a run may produce. Sharing `postvec-v*`
    # between the two meant a real tag dispatched in rehearsal mode would publish
    # a GitHub release under the real identity — after which the real
    # publication is refused, because that release already exists.
    REHEARSAL_TAG="postvec-rehearsal-v${RELEASE_ID}"
    export RELEASE_ID RELEASE_TAG REHEARSAL_TAG

    export SOURCE_DATE_EPOCH
}

# The first version that may break compatibility with ${1}, Cargo-style: for
# 0.x a minor bump breaks, from 1.0 onwards a major bump does. Used for the
# extension package's upper bound on postvec-cli, so a CLI from a future
# incompatible line can never satisfy the dependency.
next_breaking_version() {
    local major minor
    IFS=. read -r major minor _ <<<"$1"
    if [[ "${major}" == 0 ]]; then
        printf '0.%d.0\n' "$((minor + 1))"
    else
        printf '%d.0.0\n' "$((major + 1))"
    fi
}

# ---------------------------------------------------------- the bundled model

# Facts derived from the *verified* registry archive by
# scripts/check-model-bundle.py and written last by build-model-bundle.sh, so
# their presence means the payload beside them is complete.
#
# They are parsed, not sourced, for the same reason versions.env is: this file
# decides package names and paths, and a release input has no business being
# able to run a command. (The registry's free-form `source` string is
# deliberately not in it — see check-model-bundle.py.)
MODEL_FACTS=(
    MODEL_PKG_NAME MODEL_PKG_VERSION MODEL_DOC_DIR MODEL_NAME MODEL_BACKEND
    MODEL_REGISTRY_REVISION MODEL_ARCHIVE_SHA256 MODEL_ARCHIVE_SIZE
    MODEL_LICENSE MODEL_TARGET_DIM MODEL_SEQUENCE_LEN MODEL_BUNDLE_VERSION
)

# ${1} is the payload root, so a caller reading a downloaded or fixture payload
# names it; it defaults to the one build-model-bundle.sh writes.
load_model_facts() {
    local payload="${1:-${PKG_DIR}/build/payload-common}"
    local file="${payload}/model-facts.env"
    [[ -f "${file}" ]] || die "no model facts at ${file}
The bundled model has not been acquired yet. Build it first:
  scripts/build-model-bundle.sh"
    parse_env_file "${file}" model-facts.env
    local name
    for name in "${MODEL_FACTS[@]}"; do
        [[ -n "${!name:-}" ]] || die "model-facts.env: ${name} is unset or empty
Rebuild it: scripts/build-model-bundle.sh"
    done
    [[ "${MODEL_ARCHIVE_SHA256}" =~ ^[0-9a-f]{64}$ ]] \
        || die "model-facts.env: MODEL_ARCHIVE_SHA256 is not a sha256"
    for name in MODEL_TARGET_DIM MODEL_SEQUENCE_LEN MODEL_REGISTRY_REVISION \
                MODEL_ARCHIVE_SIZE MODEL_BUNDLE_VERSION; do
        [[ "${!name}" =~ ^[1-9][0-9]*$ ]] \
            || die "model-facts.env: ${name} must be a positive integer"
    done
    # The facts must describe the model this release pins — *all five* reviewed
    # values, plus the package identity derived from them.
    #
    # Checking only the name and the digest is not enough, and the gap is not
    # theoretical: bumping BUNDLED_MODEL_PKG_SUFFIX or
    # BUNDLED_MODEL_REGISTRY_REVISION without rebuilding leaves the same bytes
    # under a stale package name or a stale version, which is a package that
    # installs cleanly, upgrades wrongly, and says the wrong thing about itself.
    # The facts are *generated* from the pins, so any disagreement means the
    # payload predates an edit to versions.env.
    #
    # `${VAR:-<fact>}` throughout, because a caller that has not run
    # `load_versions` (nothing does today, but the helper must not depend on
    # that) compares a value with itself rather than with the empty string.
    local stale=()
    _facts_agree() {  # <fact name> <fact value> <pin value>
        [[ "$2" == "$3" ]] || stale+=("$1: payload has '$2', versions.env implies '$3'")
    }
    _facts_agree MODEL_NAME "${MODEL_NAME}" \
        "${BUNDLED_MODEL_NAME:-${MODEL_NAME}}"
    _facts_agree MODEL_ARCHIVE_SHA256 "${MODEL_ARCHIVE_SHA256}" \
        "${BUNDLED_MODEL_ARCHIVE_SHA256:-${MODEL_ARCHIVE_SHA256}}"
    _facts_agree MODEL_REGISTRY_REVISION "${MODEL_REGISTRY_REVISION}" \
        "${BUNDLED_MODEL_REGISTRY_REVISION:-${MODEL_REGISTRY_REVISION}}"
    _facts_agree MODEL_BUNDLE_VERSION "${MODEL_BUNDLE_VERSION}" \
        "${BUNDLED_MODEL_BUNDLE_VERSION:-${MODEL_BUNDLE_VERSION}}"
    # The package suffix reaches nothing but the package name, so it is checked
    # through it — and the other two derived values are then checked against
    # the facts' *own* revision, bundle version and package name, which the
    # four comparisons above have already tied to the pins. A facts file whose
    # halves disagree was hand-edited, and is worth refusing on its own.
    _facts_agree MODEL_PKG_NAME "${MODEL_PKG_NAME}" \
        "postvec-model-${BUNDLED_MODEL_PKG_SUFFIX:-${MODEL_PKG_NAME#postvec-model-}}"
    _facts_agree MODEL_PKG_VERSION "${MODEL_PKG_VERSION}" \
        "${MODEL_REGISTRY_REVISION}.${MODEL_BUNDLE_VERSION}.0"
    _facts_agree MODEL_DOC_DIR "${MODEL_DOC_DIR}" \
        "/usr/share/doc/${MODEL_PKG_NAME}"
    unset -f _facts_agree
    if (( ${#stale[@]} )); then
        printf '  %s\n' "${stale[@]}" >&2
        die "the model payload is stale (above): it was built from a different set
of pins than versions.env now holds. Rebuild it:
  scripts/build-model-bundle.sh"
    fi
}

# The postvec CLI refuses to *mutate* a managed tree whose ancestry any other
# account could rename out from under it (postvec-cli/src/config/owned.rs).
# That is the right rule and it is not negotiable from here — so the packaging
# scripts pick a directory that satisfies it rather than discovering the
# refusal three minutes in.
#
# Prints the first offending ancestor and returns 1; prints nothing and returns
# 0 when the whole chain is trustworthy.
untrusted_ancestor() {
    local path; path="$(cd "$(dirname "$1")" && pwd)/$(basename "$1")"
    local euid; euid="$(id -u)"
    local dir mode owner child_owner
    child_owner="$(stat -c '%u' "${path}")"
    dir="$(dirname "${path}")"
    while :; do
        mode="$(stat -c '%a' "${dir}")"
        owner="$(stat -c '%u' "${dir}")"
        if [[ "${owner}" != 0 && "${owner}" != "${euid}" ]]; then
            printf '%s (owned by uid %s, which can replace it whatever its mode)\n' \
                   "${dir}" "${owner}"
            return 1
        fi
        # Group- or world-writable is refused unless the directory is sticky
        # *and* the entry below it belongs to root or to us — /tmp semantics,
        # where nobody else may rename our entry.
        if (( $((8#${mode})) & 0022 )); then
            if (( ! ($((8#${mode})) & 01000) )) \
               || [[ "${child_owner}" != 0 && "${child_owner}" != "${euid}" ]]; then
                printf '%s (mode %s: writable by other accounts)\n' "${dir}" "${mode}"
                return 1
            fi
        fi
        [[ "${dir}" == / ]] && break
        child_owner="${owner}"
        dir="$(dirname "${dir}")"
    done
    return 0
}

# A scratch path docker can bind-mount. A snap-confined docker (or a daemon
# on another machine) silently mounts an EMPTY directory for host paths it
# cannot read — /tmp included — so anything destined for `--volume` must live
# in the checkout, where every other packaging mount already comes from.
mktemp_mountable_dir() {
    mkdir -p "${PKG_DIR}/build"
    mktemp -d "${PKG_DIR}/build/${1}.XXXXXX"
}
mktemp_mountable_file() {
    mkdir -p "${PKG_DIR}/build"
    mktemp "${PKG_DIR}/build/${1}.XXXXXX"
}

# A temporary directory the CLI will accept as an engine root's parent.
# `${PKG_DIR}/build` first — it shares a filesystem with the model cache, so
# promoting a finished pull is a rename — and TMPDIR when the checkout itself
# is group-writable, which an umask of 002 makes it. Prints the path.
mktemp_trusted_dir() {
    local basename="$1" quiet="${2:-}" dir problem
    mkdir -p "${PKG_DIR}/build"
    dir="$(mktemp -d "${PKG_DIR}/build/${basename}.XXXXXX")"
    if problem="$(untrusted_ancestor "${dir}")"; then
        printf '%s\n' "${dir}"
        return 0
    fi
    rmdir "${dir}"
    dir="$(mktemp -d "${TMPDIR:-/tmp}/postvec-${basename}.XXXXXX")"
    if problem="$(untrusted_ancestor "${dir}")"; then
        # The caller says whether this is worth a line: the reason is the same
        # for every scratch directory in one run, and repeating it buries the
        # build's real output.
        [[ "${quiet}" == quiet ]] || warn "using ${TMPDIR:-/tmp} rather than \
${PKG_DIR}/build for the model
  scratch directories: the checkout is writable by other accounts, which the
  postvec CLI refuses to manage a model tree under."
        printf '%s\n' "${dir}"
        return 0
    fi
    rm -rf "${dir}"
    die "neither ${PKG_DIR}/build nor ${TMPDIR:-/tmp} is a directory the postvec CLI
will manage a model tree under: ${problem}
Fix that directory's ownership/mode, or set TMPDIR to a private directory."
}

# The one place packaging talks to the registry, so the policy is stated once.
#
#   anonymous_model_pull <cli> <engine-root> <model-name>
#
# Anonymous, deliberately. The receipt's `access == "public"` check is the
# authoritative gate (scripts/check-model-bundle.py); this is defence in depth,
# so that a credential which happens to be in the build environment cannot
# quietly put a private model inside a public package.
#
# For a non-root build, unsetting POSTVEC_API_KEY and pointing XDG_CONFIG_HOME
# at a scratch directory hides the effective user's store. Root is different:
# the CLI intentionally reads /var/lib/postvec/auth.json regardless of XDG, so
# there is no scratch that hides it — and moving, deleting or reading around an
# operator's credential file is not something a build script may do. It refuses
# instead.
anonymous_model_pull() {
    local cli="$1" root="$2" name="$3"
    [[ "${root}" == /* ]] || die "anonymous_model_pull: the engine root must be absolute"
    if [[ "$(id -u)" == 0 && -e /var/lib/postvec/auth.json ]]; then
        die "refusing to pull the bundled model as root while /var/lib/postvec/auth.json exists.
The postvec CLI uses root's fixed credential store regardless of XDG_CONFIG_HOME,
so this build cannot prove the pull was anonymous — and a private model inside a
public package would be a redistribution decision made by accident.
Run the model bundle step as an ordinary user, or `sudo postvec logout` first."
    fi

    # The engine root has to exist, and `models/` with it: `--path` names an
    # engine root, not a directory to create one in.
    mkdir -p "${root}/models"
    chmod go-w "${root}" "${root}/models"

    local config
    config="$(mktemp_trusted_dir model-auth quiet)"
    # The scratch store lives outside the engine root so it can never be
    # mistaken for model content or copied into a payload.
    local status=0
    env -u POSTVEC_API_KEY XDG_CONFIG_HOME="${config}" \
        "${cli}" model pull --path "${root}" --yes "${name}" || status=$?
    rm -rf "${config}"
    (( status == 0 )) || die "postvec model pull failed for ${name} (exit ${status})"
}

# ------------------------------------------------------------------- platforms

# Facts about a supported target distribution. Exports, for ${1}:
#   DIST_ID          short tag used in artifact filenames  (deb12, ubuntu22.04, el9)
#   DIST_FAMILY      deb | rpm
#   DIST_BASE_IMAGE  digest-pinned builder base
#   DIST_CODENAME    apt codename (deb family only)
distro_facts() {
    case "$1" in
    debian12)
        DIST_ID=deb12; DIST_FAMILY=deb; DIST_CODENAME=bookworm
        DIST_BASE_IMAGE="debian:bookworm-slim@${BUILD_BASE_DEBIAN12_DIGEST}" ;;
    ubuntu2204)
        DIST_ID=ubuntu22.04; DIST_FAMILY=deb; DIST_CODENAME=jammy
        DIST_BASE_IMAGE="ubuntu:22.04@${BUILD_BASE_UBUNTU2204_DIGEST}" ;;
    ubuntu2404)
        DIST_ID=ubuntu24.04; DIST_FAMILY=deb; DIST_CODENAME=noble
        DIST_BASE_IMAGE="ubuntu:24.04@${BUILD_BASE_UBUNTU2404_DIGEST}" ;;
    el9)
        DIST_ID=el9; DIST_FAMILY=rpm; DIST_CODENAME=""
        DIST_BASE_IMAGE="almalinux:9@${BUILD_BASE_EL9_DIGEST}" ;;
    *)
        die "unknown distro '$1' (expected: debian12 ubuntu2204 ubuntu2404 el9)" ;;
    esac
    export DIST_ID DIST_FAMILY DIST_CODENAME DIST_BASE_IMAGE
}

# Release architecture names, from the repo-wide spelling (amd64 / arm64) to
# each ecosystem's own.
arch_facts() {
    case "$1" in
    amd64)
        DEB_ARCH=amd64; RPM_ARCH=x86_64; OCI_PLATFORM=linux/amd64
        ORT_ARCH=x64; ORT_SHA256="${ORT_LINUX_X64_SHA256}"
        RUST_TRIPLE=x86_64-unknown-linux-gnu
        RUSTUP_INIT_SHA256="${RUSTUP_INIT_X86_64_SHA256:-}"
        PROTOC_ARCH=linux-x86_64
        PROTOC_SHA256="${PROTOC_LINUX_X86_64_SHA256:-}"
        PGDG_EL9_REPO_RPM_SHA256="${PGDG_EL9_REPO_RPM_X86_64_SHA256:-}"
        ELF_MACHINE="Advanced Micro Devices X86-64" ;;
    arm64)
        DEB_ARCH=arm64; RPM_ARCH=aarch64; OCI_PLATFORM=linux/arm64
        ORT_ARCH=aarch64; ORT_SHA256="${ORT_LINUX_AARCH64_SHA256}"
        RUST_TRIPLE=aarch64-unknown-linux-gnu
        RUSTUP_INIT_SHA256="${RUSTUP_INIT_AARCH64_SHA256:-}"
        PROTOC_ARCH=linux-aarch_64
        PROTOC_SHA256="${PROTOC_LINUX_AARCH64_SHA256:-}"
        PGDG_EL9_REPO_RPM_SHA256="${PGDG_EL9_REPO_RPM_AARCH64_SHA256:-}"
        ELF_MACHINE="AArch64" ;;
    *)
        die "unknown architecture '$1' (expected: amd64 arm64)" ;;
    esac
    export DEB_ARCH RPM_ARCH OCI_PLATFORM ORT_ARCH ORT_SHA256 \
           RUST_TRIPLE RUSTUP_INIT_SHA256 PROTOC_ARCH PROTOC_SHA256 \
           PGDG_EL9_REPO_RPM_SHA256 ELF_MACHINE
}

host_release_arch() {
    case "$(uname -m)" in
    x86_64)          echo amd64 ;;
    aarch64|arm64)   echo arm64 ;;
    *)               die "unsupported host architecture $(uname -m)" ;;
    esac
}

# The PostgreSQL majors this release line supports.
supported_pg_majors() { echo "16 17 18"; }

require_pg_major() {
    case "$1" in
    16|17|18) : ;;
    *) die "unsupported PostgreSQL major '$1' (expected: $(supported_pg_majors))" ;;
    esac
}

# Where built packages live.
#
# Three roots, because the packages have three different identities.
# `postvec-cli` and ONNX Runtime are native code identical for every PostgreSQL
# major — building them per major would produce several files with the same
# name — so they belong to a (distribution, architecture) cell. The model bundle
# and the metapackage are `all`/`noarch` and belong to a distribution alone
# (`noarch_dist_dir`). The extension packages belong to a (distribution, major,
# architecture) cell.
#
# Everything that *consumes* packages has to read all three. A PG 16 install
# test looking only in its own cell would find no CLI at all, and would silently
# have been testing nothing but PG 18 (where the cells happen to coincide if you
# flatten them).
common_dist_dir() {
    printf '%s/dist/common/%s-%s\n' "${PKG_DIR}" "$1" "$2"
}

extension_dist_dir() {
    printf '%s/dist/extension/%s-pg%s-%s\n' "${PKG_DIR}" "$1" "$2" "$3"
}

# The architecture-independent packages — the model bundle and the metapackage —
# get a root of their own, keyed by distribution alone.
#
# They used to be written into an arbitrary architecture's common cell, which
# reads as tidy and is not: an arm64 consumer looks in `<distro>-arm64`, finds
# no model, and either fails or (worse) quietly tests a release that is missing
# a package. `all`/`noarch` content belongs to a distribution, so that is the
# key the directory uses, and every consumer combines three roots.
noarch_dist_dir() {
    printf '%s/dist/noarch/%s\n' "${PKG_DIR}" "$1"
}

# Package name for the per-major extension package. Debian and PGDG-RPM use
# different conventions and both are load-bearing for dependency resolution.
extension_package_name() {
    local family="$1" pg_major="$2"
    case "${family}" in
    deb) printf 'postgresql-%s-postvec\n' "${pg_major}" ;;
    rpm) printf 'postgresql%s-postvec\n' "${pg_major}" ;;
    *)   die "extension_package_name: unknown family ${family}" ;;
    esac
}

# ------------------------------------------------------------------- utilities

# The variables the nfpm package descriptions may reference. Listed explicitly
# rather than swept up by a prefix match, so a description can never silently
# depend on an unrelated variable that happens to be in the environment.
#
# The descriptions are rendered by `render_nfpm_config` below rather than by
# nfpm's own environment expansion, which does not reach every field (notably
# `contents[].src`). Rendering here also means an unset variable is a hard
# error instead of an empty string that silently packages the wrong path.
NFPM_ENV=(
    SOURCE_DATE_EPOCH SOURCE_DATE_ISO
    POSTVEC_VERSION PACKAGE_RELEASE NEXT_BREAKING_VERSION
    PKG_RELEASE PKG_ARCH NOARCH PKG_DIR REPO_ROOT
    CELL_DIR ORT_PAYLOAD_DIR MODEL_PAYLOAD_DIR
    PKGLIBDIR EXTENSIONDIR EXTENSION_PACKAGE DEBUG_PACKAGE DEBUG_SUFFIX
    PG_MAJOR PGVECTOR_MIN_VERSION
    LICENSE_DST LICENSE_NAME SHLIB_DEPENDS
    CHANGELOG_FILE CHANGELOG_NAME MODEL_LICENSE_SRC
    ORT_VERSION
    MODEL_PKG_NAME MODEL_PKG_VERSION MODEL_DOC_DIR MODEL_NAME MODEL_BACKEND
    MODEL_LICENSE MODEL_TARGET_DIM MODEL_SEQUENCE_LEN
    MODEL_REGISTRY_REVISION MODEL_ARCHIVE_SHA256
    MAINTAINER VENDOR SOURCE_REPOSITORY
    EXTRAS_METAPACKAGE
)

# Expand ${VAR} in an nfpm package description, from the allowlist above only.
# An unset or unlisted variable is an error: a package description that
# silently referenced an empty path would produce a package missing a file, and
# that is exactly the class of defect this chain exists to prevent.
render_nfpm_config() {
    local template="$1" rendered="$2"
    need python3
    NFPM_ENV_NAMES="${NFPM_ENV[*]}" python3 - "${template}" "${rendered}" <<'PY'
import os, re, sys

template, rendered = sys.argv[1], sys.argv[2]
allowed = os.environ["NFPM_ENV_NAMES"].split()
text = open(template).read()
missing, unknown = [], []

def substitute(match):
    name = match.group(1)
    if name not in allowed:
        unknown.append(name)
        return match.group(0)
    value = os.environ.get(name)
    if value is None or value == "":
        missing.append(name)
        return match.group(0)
    return value

out = re.sub(r"\$\{([A-Za-z_][A-Za-z0-9_]*)\}", substitute, text)
for label, names in (("not set", missing), ("not in the allowlist", unknown)):
    if names:
        print("%s: %s" % (template, label), file=sys.stderr)
        for name in sorted(set(names)):
            print("  ${%s}" % name, file=sys.stderr)
        sys.exit(1)
open(rendered, "w").write(out)
PY
}

# Render the changelog every package ships, and compress it.
#
# Debian Policy §12.7 requires /usr/share/doc/<package>/changelog.Debian.gz in
# every non-native package; lintian reports its absence as an error, and it is
# where `apt changelog` and every Debian administrator looks first.
#
# nFPM has a `changelog:` field and it is deliberately not used for this. Its
# .deb output omits the `<distribution>; urgency=` header that the format
# requires, so dpkg and lintian both reject the result as "not a Debian
# changelog" — which would trade a missing file for a malformed one. The source
# is therefore kept in the format it is published in, and rendered here.
#
# `gzip -n` because the file name and timestamp gzip would otherwise embed are
# build-time facts: with them, the same source produces different bytes on every
# run and the packages stop being reproducible.
render_changelog() {
    local template="$1" dest="$2" plain="${2%.gz}"
    need python3
    need gzip
    render_nfpm_config "${template}" "${plain}"
    gzip -9 -n -c "${plain}" > "${dest}"
    rm -f "${plain}"
}

# Run nfpm. Prefers a host binary (NFPM_BIN, or `nfpm` on PATH) and otherwise
# uses the pinned container, so no script here ever installs anything on the
# host. The repository is bind-mounted at its own path and the working
# directory is preserved, so absolute `src:` paths resolve identically either
# way.
# Fixed hostname for the packaging container; see the `--hostname` note below.
NFPM_BUILD_HOST=postvec-build

nfpm() {
    if [[ -n "${NFPM_BIN:-}" ]] || [[ -n "$(type -P nfpm || true)" ]]; then
        # A host binary cannot have its hostname controlled, so an RPM built
        # this way is not byte-reproducible across machines. Fine for local
        # iteration, never for a release.
        if [[ "${PACKAGER:-}" == rpm && "${POSTVEC_RELEASE:-0}" == 1 ]]; then
            die "a release RPM must be built through the pinned nfpm container
(a host nfpm stamps the build machine's hostname into the package). Unset
NFPM_BIN and remove nfpm from PATH, or build elsewhere."
        fi
    fi
    if [[ -n "${NFPM_BIN:-}" ]]; then
        "${NFPM_BIN}" "$@"
        return
    fi
    # `type -P`, not `command -v`: the latter would find this very function and
    # send us straight back into it.
    local host_nfpm
    host_nfpm="$(type -P nfpm || true)"
    if [[ -n "${host_nfpm}" ]]; then
        "${host_nfpm}" "$@"
        return
    fi
    need docker
    local envs=() name
    for name in "${NFPM_ENV[@]}"; do
        [[ -n "${!name:-}" ]] && envs+=(--env "${name}=${!name}")
    done
    # `--hostname`: nfpm stamps an RPM's buildhost from the machine's hostname,
    # so without this the same package built on two runners differs. There is
    # no nfpm setting for it — the container's hostname *is* the setting.
    docker run --rm \
        --user "$(id -u):$(id -g)" \
        --hostname "${NFPM_BUILD_HOST}" \
        --volume "${REPO_ROOT}:${REPO_ROOT}:rw" \
        --workdir "${PWD}" \
        "${envs[@]}" \
        "${NFPM_IMAGE}" "$@"
}

sha256_of() { sha256sum "$1" | awk '{print $1}'; }

# Download ${1} to ${2} and require it to hash to ${3}. A mismatch leaves no
# file behind: a partially trusted input must not be reachable by a later step.
fetch_verified() {
    local url="$1" dest="$2" want="$3" got
    need curl
    log "fetch ${url}"
    curl --fail --silent --show-error --location --retry 3 --retry-delay 2 \
         --output "${dest}.part" "${url}" || die "download failed: ${url}"
    got="$(sha256_of "${dest}.part")"
    if [[ "${got}" != "${want}" ]]; then
        rm -f "${dest}.part"
        die "checksum mismatch for ${url}"$'\n'"  expected ${want}"$'\n'"  actual   ${got}"
    fi
    mv "${dest}.part" "${dest}"
}

# Normalise a payload tree for packaging: directories 0755, files 0644, and
# every mtime pinned to SOURCE_DATE_EPOCH. Package-manager ownership plus
# read-only permissions are what stop the engine from rewriting its own weights.
normalize_tree() {
    local root="$1"
    [[ -d "${root}" ]] || die "normalize_tree: ${root} is not a directory"
    find "${root}" -type d -exec chmod 0755 {} +
    find "${root}" -type f -exec chmod 0644 {} +
    find "${root}" -exec touch --no-dereference --date="@${SOURCE_DATE_EPOCH}" {} +
}

# Write "sha256␠␠relative-path" for every file under ${1}, sorted by path, to
# stdout. Stable across machines and filesystem ordering.
manifest_of_tree() {
    local root="$1"
    ( cd "${root}" && find . -type f -printf '%P\n' | LC_ALL=C sort | xargs -r sha256sum )
}
