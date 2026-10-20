#!/usr/bin/env bash
# Pull the bundled model and lay it out for packaging.
#
#   build-model-bundle.sh [--cli PATH] [--from ENGINE_ROOT] [--refresh]
#
# Writes build/payload-common/opt/postvec/models/<backend>/<name>/ plus
# provenance under usr/share/doc and model-facts.env.
#
# Acquisition is `postvec model pull`. The receipt digest must equal
# BUNDLED_MODEL_ARCHIVE_SHA256. Ship archive-owned files only (no CLI
# receipt: it records installed_at). Refuse descriptor defects; the only
# mutation is `postvec model activate`.

source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

CLI=""
FROM_DIR=""
REFRESH=0
while (($#)); do
    case "$1" in
    --cli)     CLI="$2"; shift 2 ;;
    --from)    FROM_DIR="$2"; shift 2 ;;
    --refresh) REFRESH=1; shift ;;
    -h|--help) sed -n '2,12p' "$0"; exit 0 ;;
    *) die "unknown argument: $1" ;;
    esac
done

load_versions
need sha256sum python3 find

PAYLOAD="${PKG_DIR}/build/payload-common"
# Keyed by the pin, not the model name: an old name or old dependency
# closure cannot contaminate a new selection, and an earlier pull of the
# same digest still hits the cache after the live head moves.
CACHE_ROOT="${PKG_DIR}/build/model-cache/sha256-${BUNDLED_MODEL_ARCHIVE_SHA256}"


resolve_cli() {
    if [[ -n "${CLI}" ]]; then
        [[ -x "${CLI}" ]] || die "--cli ${CLI} is not an executable file"
        return
    fi
    if [[ -n "${POSTVEC_CLI:-}" ]]; then
        [[ -x "${POSTVEC_CLI}" ]] || die "POSTVEC_CLI=${POSTVEC_CLI} is not an executable file"
        CLI="${POSTVEC_CLI}"
        return
    fi
    # A packaging job has already built one; prefer the newest, which is the
    # cell this run is working on.
    local candidate
    for candidate in $(find "${PKG_DIR}/build" -maxdepth 3 -path '*/cli/postvec' -type f \
                            -printf '%T@ %p\n' 2>/dev/null | sort -rn | cut -d' ' -f2-); do
        if [[ -x "${candidate}" ]]; then CLI="${candidate}"; return; fi
    done
    for candidate in "${REPO_ROOT}/target/release/postvec" "${REPO_ROOT}/target/debug/postvec"; do
        if [[ -x "${candidate}" ]]; then CLI="${candidate}"; return; fi
    done
    die "no postvec CLI to pull the bundled model with.
Build one:      cargo build -p postvec-cli      (-> target/debug/postvec)
or name one:    --cli /path/to/postvec, or POSTVEC_CLI=/path/to/postvec"
}

resolve_cli
CLI="$(cd "$(dirname "${CLI}")" && pwd)/$(basename "${CLI}")"
# A builder-produced binary can have a newer libc floor than the release host.
# Finding that out here, by name, beats finding it out as a loader error in the
# middle of a pull.
if ! "${CLI}" --version >/dev/null 2>&1; then
    die "${CLI} cannot be executed on this host.
It is probably built for a different distribution's glibc. Name one that runs
here: --cli /path/to/postvec, or POSTVEC_CLI=/path/to/postvec, or build one
with: cargo build -p postvec-cli"
fi
log "postvec CLI: ${CLI} ($("${CLI}" --version))"


# A completed pull is promoted into the digest-keyed cache only after it has
# been verified against the pin, so the directory name can never lie about its
# contents, and a moved head can never destroy an older valid cache.
PULL_ROOT=""
PROMOTE=0
# return 0: under set -e a trap that fails on its first command never
# reaches the rest, so a no-op cleanup would fail a successful build.
cleanup() { [[ -n "${PULL_ROOT}" ]] && rm -rf "${PULL_ROOT}"; return 0; }
trap cleanup EXIT

# Finish an interrupted promotion. `.replaced` is a complete cache (put it
# back, or the next run needs the network). `.incoming` may be partial, so
# delete it. A restored cache is still verified before it is trusted.
if [[ ! -d "${CACHE_ROOT}" && -d "${CACHE_ROOT}.replaced" ]]; then
    warn "an earlier run was interrupted mid-promotion; restoring the previous cache"
    mv "${CACHE_ROOT}.replaced" "${CACHE_ROOT}"
fi
rm -rf "${CACHE_ROOT}.incoming" "${CACHE_ROOT}.replaced"

if [[ -n "${FROM_DIR}" ]]; then
    [[ -d "${FROM_DIR}/models" ]] || die "--from: ${FROM_DIR} is not an engine root (no models/)"
    SOURCE_ROOT="$(cd "${FROM_DIR}" && pwd)"
    log "taking the bundle from ${SOURCE_ROOT} (every check still applies)"
else
    if (( ! REFRESH )) && [[ -d "${CACHE_ROOT}/models" ]]; then
        SOURCE_ROOT="${CACHE_ROOT}"
        log "using the cached pull at ${CACHE_ROOT}"
    else
        PULL_ROOT="$(mktemp_trusted_dir model-pull)"
        log "pulling ${BUNDLED_MODEL_NAME} from the registry's public channel"
        anonymous_model_pull "${CLI}" "${PULL_ROOT}" "${BUNDLED_MODEL_NAME}"
        SOURCE_ROOT="${PULL_ROOT}"
        PROMOTE=1
    fi
fi


mapfile -t INSTALLED < <(
    find "${SOURCE_ROOT}/models" -mindepth 2 -maxdepth 2 -type d -printf '%P\n' \
        | LC_ALL=C sort
)
(( ${#INSTALLED[@]} )) || die "no model was installed under ${SOURCE_ROOT}/models"
if (( ${#INSTALLED[@]} != 1 )); then
    printf '  %s\n' "${INSTALLED[@]}" >&2
    die "the pull produced ${#INSTALLED[@]} model directories (above).
Ship one package per model identity. A dependency closure is not folded into
one binary package."
fi
BACKEND="${INSTALLED[0]%%/*}"
INSTALLED_NAME="${INSTALLED[0]#*/}"
[[ "${INSTALLED_NAME}" == "${BUNDLED_MODEL_NAME}" ]] \
    || die "the engine root holds ${INSTALLED_NAME}, not the pinned ${BUNDLED_MODEL_NAME}"
SOURCE_MODEL_DIR="${SOURCE_ROOT}/models/${BACKEND}/${INSTALLED_NAME}"


# Pull leaves the model deactivated. Activate through the CLI so the receipt
# hash covers that one flip. Skip if already active: build/ is mode 775, and
# re-activating trips the CLI's engine-root trust check.
if python3 - "${SOURCE_MODEL_DIR}/ninference.hub.json" <<'PY'
import json, sys
sys.exit(0 if json.load(open(sys.argv[1])).get("enabled") is True else 1)
PY
then
    log "${BUNDLED_MODEL_NAME} is already active in the staging root"
else
    log "activating ${BUNDLED_MODEL_NAME} in the staging root"
    "${CLI}" model activate --path "${SOURCE_ROOT}" --yes "${BUNDLED_MODEL_NAME}" >/dev/null \
        || die "postvec model activate failed for ${BUNDLED_MODEL_NAME} at ${SOURCE_ROOT}"
fi


# The shared, offline Receipt::verify_files implementation: cache corruption, a
# modified --from root, a missing file and an unrecorded file all fail here,
# before packaging looks at anything.
log "verifying every installed file against the receipt"
"${CLI}" model show --path "${SOURCE_ROOT}" --verify "${BUNDLED_MODEL_NAME}" >/dev/null \
    || die "postvec model show --verify failed for ${BUNDLED_MODEL_NAME} at ${SOURCE_ROOT}"

META="$(mktemp -d "${PKG_DIR}/build/model-meta.XXXXXX")"
trap 'cleanup; rm -rf "${META}"' EXIT
"${PKG_DIR}/scripts/check-model-bundle.py" "${SOURCE_MODEL_DIR}" "${META}" \
    || die "the pulled model does not satisfy this release's pins and policy"

# Promote only after the bytes match the pin. Stage on the destination
# filesystem, then rename. A cross-filesystem copy must not overwrite a good cache.
if (( PROMOTE )); then
    mkdir -p "$(dirname "${CACHE_ROOT}")"
    STAGE="${CACHE_ROOT}.incoming"
    trap 'cleanup; rm -rf "${META}" "${STAGE}"' EXIT
    rm -rf "${STAGE}"
    if mv "${PULL_ROOT}" "${STAGE}" 2>/dev/null; then
        PULL_ROOT=""
    else
        cp -a "${PULL_ROOT}" "${STAGE}"
    fi

    # Everything below is same-filesystem renames, so the window in which the
    # cache is absent is one syscall wide and nothing can fail part-way.
    rm -rf "${CACHE_ROOT}.replaced"
    if [[ -d "${CACHE_ROOT}" ]]; then
        mv "${CACHE_ROOT}" "${CACHE_ROOT}.replaced"
    fi
    if ! mv "${STAGE}" "${CACHE_ROOT}"; then
        [[ -d "${CACHE_ROOT}.replaced" ]] && mv "${CACHE_ROOT}.replaced" "${CACHE_ROOT}"
        die "could not promote the verified pull into ${CACHE_ROOT}"
    fi
    rm -rf "${CACHE_ROOT}.replaced"
    SOURCE_ROOT="${CACHE_ROOT}"
    SOURCE_MODEL_DIR="${SOURCE_ROOT}/models/${BACKEND}/${INSTALLED_NAME}"
    log "cached at ${CACHE_ROOT}"
fi

# The facts are validator-constrained; reading them here is what makes the
# package name, version and doc directory follow the pin rather than a literal.
parse_env_file "${META}/model-facts.env" model-facts.env

MODEL_DIR="${PAYLOAD}/opt/postvec/models/${MODEL_BACKEND}/${MODEL_NAME}"
DOC="${PAYLOAD}/usr/share/doc/${MODEL_PKG_NAME}"


# Every previous bundle goes, including one built under a different model name
# or package name: a leftover tree would be packaged by the `type: tree` entry
# and would ship two models in a package that names one.
rm -rf "${PAYLOAD}/opt/postvec/models" \
       "${PAYLOAD}/usr/share/doc" \
       "${PAYLOAD}/model-facts.env"
mkdir -p "${MODEL_DIR}" "${DOC}"

mapfile -d '' -t ARCHIVE_FILES < "${META}/receipt-files.nul"
for rel in "${ARCHIVE_FILES[@]}"; do
    mkdir -p "${MODEL_DIR}/$(dirname "${rel}")"
    cp "${SOURCE_MODEL_DIR}/${rel}" "${MODEL_DIR}/${rel}"
done

# What landed must be exactly what the receipt listed. The receipt is not package content.
mapfile -t COPIED < <(cd "${MODEL_DIR}" && find . -type f -printf '%P\n' | LC_ALL=C sort)
if [[ "$(printf '%s\n' "${COPIED[@]}")" != "$(printf '%s\n' "${ARCHIVE_FILES[@]}")" ]]; then
    diff <(printf '%s\n' "${ARCHIVE_FILES[@]}") <(printf '%s\n' "${COPIED[@]}") >&2 || true
    die "the copied payload does not match the receipt's file list (diff above)"
fi
for rel in "${COPIED[@]}"; do
    case "/${rel}/" in
    */.postvec-install.json/*|*/.staging/*|*/.trash/*|*/.swap/*|*/.postvec.lock/*)
        die "a transaction or receipt path reached the payload: ${rel}" ;;
    esac
done


# The archive always carries a licence. Absence is a publication defect.
# Do not substitute one. check-model-bundle.py already refused that case.
cp "${MODEL_DIR}/LICENSE" "${DOC}/LICENSE"
cp "${META}/SOURCE.json"  "${DOC}/SOURCE.json"
cp "${META}/copyright"    "${DOC}/copyright"
manifest_of_tree "${MODEL_DIR}" > "${DOC}/model-files.sha256"

normalize_tree "${PAYLOAD}"

# Written last, and only now: its presence is what tells every later stage that
# the payload beside it is complete.
install -m 0644 "${META}/model-facts.env" "${PAYLOAD}/model-facts.env"
touch --date="@${SOURCE_DATE_EPOCH}" "${PAYLOAD}/model-facts.env"

log "model bundle ready: ${MODEL_NAME} @ registry revision ${MODEL_REGISTRY_REVISION}"
log "  package ${MODEL_PKG_NAME} ${MODEL_PKG_VERSION} (${MODEL_LICENSE}, ${MODEL_TARGET_DIM} dims)"
du -sh "${MODEL_DIR}" | sed 's/^/  /'
find "${MODEL_DIR}" -type f -printf '  %P\n' | sort
