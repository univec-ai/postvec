#!/usr/bin/env bash
# Turn one staged release cell into its packages.
#
#   build-packages.sh --distro debian12 --pg 18 --arch amd64
#   build-packages.sh --distro el9 --pg 18 --arch amd64 --only extension
#   build-packages.sh --distro debian12 --pg 18 --arch amd64 --only cli,onnxruntime,server
#
# `--only` names any of: extension, cli, server, onnxruntime, model. Without
# it, everything the staged cell and the payloads allow: `server` needs a cell
# built with `build-extension-stage.sh --with-server`, and is skipped (with a
# warning) when the cell has no node binary — unless POSTVEC_RELEASE=1, where
# a shared-package cell without one is a defect.
#
# Reads packaging/postvec/build/<cell>/ (from build-extension-stage.sh) plus
# the engine-asset payloads (from build-onnxruntime-bundle.sh and
# build-model-bundle.sh), and writes .deb or .rpm files to
# packaging/postvec/dist/<cell>/.
#
# One nfpm description per package produces both formats, so a dependency or
# a file-placement rule is stated once. Where the two ecosystems genuinely
# differ (package naming, version relations, licence directory) the
# difference is explicit in the description's `overrides` block or in the
# variables below.

source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

DISTRO=""; PG_MAJOR=""; RELEASE_ARCH=""; ONLY=""
while (($#)); do
    case "$1" in
    --distro) DISTRO="$2"; shift 2 ;;
    --pg)     PG_MAJOR="$2"; shift 2 ;;
    --arch)   RELEASE_ARCH="$2"; shift 2 ;;
    --only)   ONLY="$2"; shift 2 ;;
    -h|--help) sed -n '2,18p' "$0"; exit 0 ;;
    *) die "unknown argument: $1" ;;
    esac
done

: "${DISTRO:?--distro is required}"
: "${PG_MAJOR:?--pg is required}"
RELEASE_ARCH="${RELEASE_ARCH:-$(host_release_arch)}"

load_versions
# The bundled model's package name, version, licence and dimension are derived
# from the verified registry archive, not from versions.env directly. Loading
# them here means every description below follows the pin.
load_model_facts
require_pg_major "${PG_MAJOR}"
distro_facts "${DISTRO}"
arch_facts "${RELEASE_ARCH}"

CELL="${DISTRO}-pg${PG_MAJOR}-${RELEASE_ARCH}"
CELL_DIR="${PKG_DIR}/build/${CELL}"
# Three output roots, one per package identity; see `common_dist_dir` and
# `noarch_dist_dir` in lib.sh for why.
COMMON_DIR="$(common_dist_dir "${DISTRO}" "${RELEASE_ARCH}")"
EXTENSION_DIR_OUT="$(extension_dist_dir "${DISTRO}" "${PG_MAJOR}" "${RELEASE_ARCH}")"
NOARCH_DIR_OUT="$(noarch_dist_dir "${DISTRO}")"
# Engine-asset payloads. ONNX Runtime is architecture-specific; the model
# bundle is one portable FP32 graph shared by every architecture.
ORT_PAYLOAD_ROOT="${PKG_DIR}/build/payload-${RELEASE_ARCH}"
MODEL_PAYLOAD_ROOT="${PKG_DIR}/build/payload-common"
# The node's dashboard: static files, built once (build-ui-bundle.sh).
UI_PAYLOAD_ROOT="${PKG_DIR}/build/payload-ui"

wanted() { [[ -z "${ONLY}" || " ${ONLY//,/ } " == *" $1 "* ]]; }

# Only the packages that carry compiled output need a compiled cell. The model
# bundle and the metapackage are architecture- and PostgreSQL-independent, and
# requiring a staged build for them would mean compiling the extension in order
# to package something that does not contain it.
NEEDS_STAGE=0
if wanted extension || wanted cli || wanted server; then NEEDS_STAGE=1; fi

if ((NEEDS_STAGE)); then
    [[ -d "${CELL_DIR}/stage" ]] || die "no staged build at ${CELL_DIR}
Run: scripts/build-extension-stage.sh --distro ${DISTRO} --pg ${PG_MAJOR} --arch ${RELEASE_ARCH}"
fi

# Only the roots this invocation actually writes to. An empty directory left
# behind by `--only model` is not harmless: the next consumer globs it, finds
# nothing, and has to decide whether that means "not built yet" or "built and
# produced nothing" — and those need different answers.
if wanted cli || wanted onnxruntime || wanted server; then mkdir -p "${COMMON_DIR}"; fi
if wanted extension;                   then mkdir -p "${EXTENSION_DIR_OUT}"; fi
if wanted model;                       then mkdir -p "${NOARCH_DIR_OUT}"; fi

# Remove leftover model and metapackage files from dist/common. The
# manifest walks all of dist/, so a leftover copy next to the noarch
# one would be published.
shopt -s nullglob
for stale in "${PKG_DIR}"/dist/common/"${DISTRO}"-*/postvec-model-* \
             "${PKG_DIR}"/dist/common/"${DISTRO}"-*/"${EXTRAS_METAPACKAGE}"[-_]* \
             "${PKG_DIR}"/dist/common/"${DISTRO}"-*/postvec-embedded[-_]*; do
    warn "removing ${stale#"${PKG_DIR}/"}"
    warn "  architecture-independent packages now live in dist/noarch/${DISTRO}/"
    rm -f "${stale}"
done
shopt -u nullglob

# ------------------------------------------------------- derived package facts

# Where this PostgreSQL puts extensions is a property of the build, not a
# guess: read it back from the staging tree cargo-pgrx laid out from the
# target's own pg_config. Debian's /usr/lib/postgresql/18/lib and PGDG-RPM's
# /usr/pgsql-18/lib both fall out of this with no special case.
PKGLIBDIR=""; EXTENSIONDIR=""
if ((NEEDS_STAGE)); then
    mapfile -t so_paths < <(find "${CELL_DIR}/stage" -name postvec.so -type f | LC_ALL=C sort)
    mapfile -t ctl_paths < <(find "${CELL_DIR}/stage" -name postvec.control -type f | LC_ALL=C sort)
    (( ${#so_paths[@]} && ${#ctl_paths[@]} )) \
        || die "staged tree is missing postvec.so or postvec.control"
    # More than one means the stage carries two layouts — a Debian tree and a PGDG
    # tree, say — and picking the first would package the wrong library at the
    # wrong path. Refuse rather than guess.
    (( ${#so_paths[@]} == 1 )) || die "the staged tree contains ${#so_paths[@]} copies of postvec.so:
$(printf '  %s\n' "${so_paths[@]}")
Rebuild the cell: scripts/build-extension-stage.sh --distro ${DISTRO} --pg ${PG_MAJOR} --arch ${RELEASE_ARCH}"
    so_path="${so_paths[0]}"
    ctl_path="${ctl_paths[0]}"
    PKGLIBDIR="$(dirname "${so_path#"${CELL_DIR}/stage"}")"
    EXTENSIONDIR="$(dirname "${ctl_path#"${CELL_DIR}/stage"}")"
fi

EXTENSION_PACKAGE="$(extension_package_name "${DIST_FAMILY}" "${PG_MAJOR}")"
# Each ecosystem's own name for a detached-symbols package, so the usual
# tooling finds it.
case "${DIST_FAMILY}" in
deb) DEBUG_SUFFIX=dbgsym ;;
rpm) DEBUG_SUFFIX=debuginfo ;;
esac
DEBUG_PACKAGE="${EXTENSION_PACKAGE}-${DEBUG_SUFFIX}"

case "${DIST_FAMILY}" in
deb)
    PACKAGER=deb
    # 1+deb12 sorts below 1+deb13 and above a plain 1, which is what a user
    # upgrading across distributions expects.
    PKG_RELEASE="${PACKAGE_RELEASE}+${DIST_ID}"
    NOARCH=all
    # Debian policy: the licence goes to /usr/share/doc/<pkg>/copyright.
    LICENSE_DST=/usr/share/doc
    LICENSE_NAME=copyright
    CHANGELOG_NAME=changelog.Debian.gz
    # A licence text is not repeated in a Debian copyright file when the
    # distribution ships a common copy: policy asks for a reference to
    # /usr/share/common-licenses/<id>, and lintian reports repeating it as an
    # error. The verbatim licence still travels with the model under /opt.
    #
    # The file is rendered by the bundle step from the *verified* archive's
    # licence and provenance (scripts/check-model-bundle.py), so this script
    # no longer reaches into model/ for a hand-written one.
    MODEL_LICENSE_SRC="${MODEL_PAYLOAD_ROOT}${MODEL_DOC_DIR}/copyright"
    ;;
rpm)
    PACKAGER=rpm
    PKG_RELEASE="${PACKAGE_RELEASE}.${DIST_ID}"
    NOARCH=noarch
    LICENSE_DST=/usr/share/licenses
    LICENSE_NAME=LICENSE
    CHANGELOG_NAME=changelog.gz
    # RPM has no common-licenses directory, so /usr/share/licenses carries the
    # full text — which is also what `rpm -qL` expects to find there.
    MODEL_LICENSE_SRC="${MODEL_PAYLOAD_ROOT}${MODEL_DOC_DIR}/LICENSE"
    ;;
esac

PKG_ARCH="${RELEASE_ARCH}"
SOURCE_DATE_ISO="$(date -u -d "@${SOURCE_DATE_EPOCH}" +%Y-%m-%dT%H:%M:%SZ)"

# Where the node's systemd unit goes. /usr/lib/systemd/system on both
# families: every supported distribution is merged-/usr, so on Debian and
# Ubuntu this is the same directory as /lib/systemd/system, and it is the
# path RPM packaging has always used.
SERVER_UNIT_DIR=/usr/lib/systemd/system

export CELL_DIR \
       ORT_PAYLOAD_DIR="${ORT_PAYLOAD_ROOT}" MODEL_PAYLOAD_DIR="${MODEL_PAYLOAD_ROOT}" \
       UI_PAYLOAD_DIR="${UI_PAYLOAD_ROOT}" \
       PKGLIBDIR EXTENSIONDIR EXTENSION_PACKAGE DEBUG_PACKAGE DEBUG_SUFFIX PG_MAJOR \
       PKG_RELEASE PKG_ARCH NOARCH LICENSE_DST LICENSE_NAME SOURCE_DATE_ISO \
       CHANGELOG_NAME MODEL_LICENSE_SRC SERVER_UNIT_DIR \
       MODEL_PKG_NAME MODEL_PKG_VERSION MODEL_DOC_DIR MODEL_NAME MODEL_BACKEND \
       MODEL_LICENSE MODEL_TARGET_DIM MODEL_SEQUENCE_LEN \
       MODEL_REGISTRY_REVISION MODEL_ARCHIVE_SHA256

log "packaging ${CELL} as .${PACKAGER}"
if ((NEEDS_STAGE)); then
    log "  extension package  ${EXTENSION_PACKAGE} ${POSTVEC_VERSION}-${PKG_RELEASE}"
    log "  pkglibdir          ${PKGLIBDIR}"
    log "  extension dir      ${EXTENSIONDIR}"
else
    log "  architecture-independent packages only; no compiled cell needed"
fi

# ------------------------------------------------------------------- packaging

# Rendered descriptions are kept next to the artifacts: when a package turns
# out to contain the wrong thing, the exact input that produced it is the first
# thing anyone wants to read.
RENDERED="${PKG_DIR}/dist/.rendered/${CELL}"
mkdir -p "${RENDERED}"

# Every package ships the same changelog, because every package comes from the
# same release of the same source. It is rendered once per cell rather than per
# package: the content does not depend on which binary package carries it, and
# generating it seven times would be seven chances for them to differ.
export CHANGELOG_FILE="${RENDERED}/${CHANGELOG_NAME}"
render_changelog "${PKG_DIR}/changelog.Debian" "${CHANGELOG_FILE}"

# The node's licence file, rendered from the crate's own LICENSE and the
# reviewed SERVER_LICENSE pin rather than kept as a second hand-written copy.
#
# Debian wants a DEP-5 copyright file, and for a licence the distribution
# ships under /usr/share/common-licenses it wants a *reference* — lintian
# reports repeating the Apache-2.0 text as an error. Any other identifier has
# no common copy, so the crate's LICENSE is inlined, indented as DEP-5
# requires. RPM's /usr/share/licenses takes the verbatim text either way.
# This is the same policy check-model-bundle.py applies to the bundled model.
render_server_copyright() {
    local licence="${REPO_ROOT}/postvec-server/LICENSE" out="${RENDERED}/server-copyright"
    [[ -f "${licence}" ]] || die "no postvec-server/LICENSE to package"
    case "${PACKAGER}" in
    rpm)
        cp "${licence}" "${out}" ;;
    deb)
        {
            echo "Format: https://www.debian.org/doc/packaging-manuals/copyright-format/1.0/"
            echo "Upstream-Name: postvec-server"
            echo "Source: ${SOURCE_REPOSITORY}"
            echo "Comment: postvec-server is the one component of postvec that is not under"
            echo " the PostgreSQL License. The extension, the postvec command, the engine"
            echo " assets and their packaging are; see LICENSING.md in the source repository."
            echo
            echo "Files: *"
            echo "Copyright: 2026 Univec Ltd"
            echo "License: ${SERVER_LICENSE}"
            if [[ "${SERVER_LICENSE}" == Apache-2.0 ]]; then
                echo " On Debian systems, the complete text of the Apache License, Version 2.0"
                echo " can be found in /usr/share/common-licenses/Apache-2.0."
            else
                # DEP-5: continuation lines are indented one space, and a
                # blank line inside the text is a single ` .`.
                sed -e 's/^$/./' -e 's/^/ /' "${licence}"
            fi
        } > "${out}" ;;
    esac
    printf '%s\n' "${out}"
}
SERVER_COPYRIGHT_SRC="$(render_server_copyright)"
export SERVER_COPYRIGHT_SRC

# The dependency generators are slow (they start a container), so each distinct
# set of binaries is resolved once and reused.
declare -A SHLIB_CACHE=()

# Render `${SHLIB_DEPENDS}` for a package: the target distribution's own
# dependency generator, formatted as YAML list items at the indentation the
# templates use. nFPM runs no generator of its own, so this is the only thing
# standing between a published package and a missing libc dependency.
shlib_depends_for() {
    local key="$1"; shift
    if [[ -z "${SHLIB_CACHE[${key}]+set}" ]]; then
        local generated
        generated="$("${PKG_DIR}/scripts/elf-depends.sh" \
            --distro "${DISTRO}" --arch "${RELEASE_ARCH}" "$@")" \
            || die "could not compute ${key} dependencies"
        SHLIB_CACHE["${key}"]="$(sed 's/^/      - /' <<<"${generated}")"
    fi
    printf '%s' "${SHLIB_CACHE[${key}]}"
}

# ${3} is the output root: `${COMMON_DIR}` for anything that is the same for
# every PostgreSQL major, `${EXTENSION_DIR_OUT}` for anything that is not.
build_one() {
    local description="$1" label="$2" target="$3"
    log "  → ${label}"
    render_nfpm_config "${PKG_DIR}/nfpm/${description}" "${RENDERED}/${description}"
    nfpm package --config "${RENDERED}/${description}" \
                 --packager "${PACKAGER}" \
                 --target "${target}/"
}

if wanted cli; then
    # Plain assignment first: `VAR="$(...)" cmd` would discard the generator's
    # failure and hand nfpm an empty ${SHLIB_DEPENDS}.
    cli_shlibs="$(shlib_depends_for cli "${CELL_DIR}/cli/postvec")"
    SHLIB_DEPENDS="${cli_shlibs}" \
        build_one postvec-cli.yaml "postvec-cli" "${COMMON_DIR}"
    # The CLI's symbols belong with the CLI: one package for every major, so
    # two majors' debug packages cannot conflict over the same file.
    if [[ -f "${CELL_DIR}/debug/postvec.debug" ]]; then
        build_one postvec-cli-debug.yaml "postvec-cli-${DEBUG_SUFFIX}" "${COMMON_DIR}"
    elif [[ "${POSTVEC_RELEASE:-0}" == 1 ]]; then
        die "no detached symbols at ${CELL_DIR}/debug/postvec.debug.
A publishable build must ship postvec-cli-${DEBUG_SUFFIX}."
    else
        log "  (no separable CLI symbols; skipping postvec-cli-${DEBUG_SUFFIX})"
    fi
fi
if wanted server; then
    # The inference node. PostgreSQL-independent like the CLI, so it lives in
    # the common root and comes out of the shared-package cell — which is the
    # one built with `--with-server`. An extension cell has no node binary and
    # is not asked for one; a shared-package cell in a release must have one.
    if [[ -f "${CELL_DIR}/server/postvec-server" ]]; then
        # The dashboard is part of the package, not an optional extra: a
        # node without it answers / with a stub, and nothing else would tell.
        [[ -f "${UI_PAYLOAD_ROOT}/opt/postvec/server/ui/index.html" ]] \
            || die "no dashboard payload at ${UI_PAYLOAD_ROOT}.
Run: scripts/build-ui-bundle.sh"
        # The TLS listener links OpenSSL, which no bare base image carries;
        # the generator installs it first so it can name the package (see
        # elf-depends.sh --runtime-package). The pattern covers Debian 12 and
        # Ubuntu 22.04 (libssl3) and Ubuntu 24.04 (libssl3t64); EL9's rpmdeps
        # needs nothing installed.
        server_shlibs="$(shlib_depends_for server \
            --runtime-package '^libssl3(t64)?$' "${CELL_DIR}/server/postvec-server")"
        SHLIB_DEPENDS="${server_shlibs}" \
            build_one postvec-server.yaml "postvec-server (${SERVER_LICENSE})" "${COMMON_DIR}"
        if [[ -f "${CELL_DIR}/debug/postvec-server.debug" ]]; then
            build_one postvec-server-debug.yaml "postvec-server-${DEBUG_SUFFIX}" "${COMMON_DIR}"
        elif [[ "${POSTVEC_RELEASE:-0}" == 1 ]]; then
            die "no detached symbols at ${CELL_DIR}/debug/postvec-server.debug.
A publishable build must ship postvec-server-${DEBUG_SUFFIX}."
        else
            log "  (no separable postvec-server symbols; skipping postvec-server-${DEBUG_SUFFIX})"
        fi
    elif [[ "${POSTVEC_RELEASE:-0}" == 1 ]]; then
        die "no postvec-server binary at ${CELL_DIR}/server/postvec-server.
A release's shared-package cell is built with --with-server:
  scripts/build-extension-stage.sh --distro ${DISTRO} --pg ${PG_MAJOR} --arch ${RELEASE_ARCH} --with-server"
    else
        warn "no postvec-server binary in ${CELL} — skipping postvec-server"
        warn "  build it with: scripts/build-extension-stage.sh --distro ${DISTRO} --pg ${PG_MAJOR} --arch ${RELEASE_ARCH} --with-server"
    fi
fi
if wanted extension; then
    ext_shlibs="$(shlib_depends_for extension "${so_path}")"
    SHLIB_DEPENDS="${ext_shlibs}" \
        build_one postvec-extension.yaml "${EXTENSION_PACKAGE}" "${EXTENSION_DIR_OUT}"

    # A published release ships symbols. Both builds emit line tables, so their
    # absence means the split silently did not happen — and a release that
    # advertises debug packages and has none is worse than one that never
    # promised them. Outside a release the absence is tolerated, because a
    # developer may be building without them on purpose.
    if [[ -f "${CELL_DIR}/debug/postvec.so.debug" ]]; then
        build_one postvec-debug.yaml "${DEBUG_PACKAGE}" "${EXTENSION_DIR_OUT}"
    elif [[ "${POSTVEC_RELEASE:-0}" == 1 ]]; then
        die "no detached symbols at ${CELL_DIR}/debug/postvec.so.debug.
A publishable build must ship ${DEBUG_PACKAGE}; see build-extension-stage.sh."
    else
        log "  (no separable extension symbols; skipping ${DEBUG_PACKAGE})"
    fi
fi

# The engine assets are architecture- and model-versioned, not PG-versioned, so
# they are built once per architecture and packaged only when their payload is
# present. Absence is normal for a remote-only release job.
if wanted onnxruntime; then
    if [[ -d "${ORT_PAYLOAD_ROOT}/opt/postvec/libs/onnxruntime" ]]; then
        # Every real shared object in the payload, resolved as one set — the
        # providers library needs libstdc++ even though the main one does not.
        mapfile -t ort_libs < <(
            find "${ORT_PAYLOAD_ROOT}/opt/postvec/libs/onnxruntime" \
                 -type f -name '*.so*' | LC_ALL=C sort
        )
        ort_shlibs="$(shlib_depends_for onnxruntime "${ort_libs[@]}")"
        SHLIB_DEPENDS="${ort_shlibs}" \
            build_one postvec-onnxruntime.yaml "postvec-onnxruntime ${ORT_VERSION}" "${COMMON_DIR}"
    else
        warn "no ONNX Runtime payload at ${ORT_PAYLOAD_ROOT} — skipping postvec-onnxruntime"
        warn "  build it with: scripts/build-onnxruntime-bundle.sh --arch ${RELEASE_ARCH}"
    fi
fi

if wanted model; then
    if [[ -d "${MODEL_PAYLOAD_ROOT}/opt/postvec/models/${MODEL_BACKEND}/${MODEL_NAME}" ]]; then
        # `all`/`noarch`: one build per distribution, into the noarch root. The
        # architecture in this invocation names nothing about these two packages
        # and must not end up in their path, or an arm64 consumer would have to
        # know which architecture's cell the amd64 job happened to use.
        build_one postvec-model.yaml "${MODEL_PKG_NAME}" "${NOARCH_DIR_OUT}"
        build_one postvec-extras.yaml "${EXTRAS_METAPACKAGE} (metapackage)" "${NOARCH_DIR_OUT}"
    else
        warn "no model payload at ${MODEL_PAYLOAD_ROOT} — skipping the model and metapackage"
        warn "  build it with: scripts/build-model-bundle.sh"
    fi
fi

# -------------------------------------------------- distribution in the name

# The package *name* must not carry the distribution (apt and dnf resolve
# upgrades by name), but the release *filename* must, because a Debian 12 build
# and an Ubuntu 24.04 build of the same version are different artifacts with
# different glibc floors. nfpm names files from the package metadata, so the
# distribution tag arrives via PKG_RELEASE and is already in every filename
# below — this loop only proves it, rather than renaming anything.
OUT_DIRS=()
for dir in "${COMMON_DIR}" "${EXTENSION_DIR_OUT}" "${NOARCH_DIR_OUT}"; do
    [[ -d "${dir}" ]] && OUT_DIRS+=("${dir}")
done

for dir in "${OUT_DIRS[@]}"; do
    for artifact in "${dir}"/*."${PACKAGER}"; do
        [[ -e "${artifact}" ]] || continue
        [[ "${artifact}" == *"${DIST_ID}"* ]] \
            || die "artifact ${artifact##*/} does not identify its distribution (${DIST_ID})"
    done
done

log "packages for ${CELL}:"
find "${OUT_DIRS[@]}" -maxdepth 1 -type f -printf '  %f  (%s bytes)\n' | sort
