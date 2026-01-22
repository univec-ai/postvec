#!/usr/bin/env bash
# Acquire the bundled embedding model from the postvec model registry and lay
# it out as a packageable engine-model directory.
#
#   build-model-bundle.sh [--cli PATH] [--from ENGINE_ROOT] [--refresh]
#
# --cli PATH      the postvec binary to pull with. Default discovery order:
#                 $POSTVEC_CLI, the newest build/*/cli/postvec, target/release,
#                 target/debug.
# --from ROOT     take the model from an existing engine root instead of
#                 pulling. Every receipt and file-integrity assertion still
#                 applies; it is a cache, not an escape hatch.
# --refresh       ignore the digest-keyed cache and pull again.
#
# Writes build/payload-common/opt/postvec/ninference/models/<backend>/<name>/
# in the layout the engine loads, plus the licence, provenance and per-file
# digests under build/payload-common/usr/share/doc/<package>/, and the derived
# facts every later stage reads from build/payload-common/model-facts.env.
#
# The bundle is architecture-independent: one portable FP32 ONNX graph, the
# same bytes on amd64 and arm64.
#
# Four rules this script exists to enforce:
#
#   1. Acquisition is `postvec model pull` — the same command a user runs. The
#      CLI owns index parsing, channel containment, resumable download, whole-
#      archive digest verification, strict extraction and the receipt. There is
#      no second, packaging-private installer here, and no `curl`.
#   2. Nothing unpinned reaches the payload. The receipt's archive digest must
#      equal BUNDLED_MODEL_ARCHIVE_SHA256, or the build refuses before any byte
#      is copied.
#   3. Only archive-owned files are packaged, byte for byte. The CLI-generated
#      receipt is deliberately *not* shipped: it records `installed_at`, so
#      shipping it would make an otherwise reproducible package differ on every
#      clean rebuild. Packaging's own provenance lives in /usr/share/doc.
#   4. A descriptor requirement is a publication defect. This script asserts
#      and refuses; it never edits ninference.hub.json inside the archive.
#      The one sanctioned mutation is `postvec model activate` below — the
#      CLI's own persistent enable flip, which also updates the receipt.

source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

CLI=""
FROM_DIR=""
REFRESH=0
while (($#)); do
    case "$1" in
    --cli)     CLI="$2"; shift 2 ;;
    --from)    FROM_DIR="$2"; shift 2 ;;
    --refresh) REFRESH=1; shift ;;
    -h|--help) sed -n '2,14p' "$0"; exit 0 ;;
    *) die "unknown argument: $1" ;;
    esac
done

load_versions
need sha256sum python3 find

PAYLOAD="${PKG_DIR}/build/payload-common"
# Keyed by the pin, not by the model name: an old name or an old dependency
# closure can then never contaminate a new selection, and an exact earlier pull
# survives the live head advancing (see packaging-registry-models.md §4.2).
CACHE_ROOT="${PKG_DIR}/build/model-cache/sha256-${BUNDLED_MODEL_ARCHIVE_SHA256}"

# ------------------------------------------------------------------- the CLI

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

# ---------------------------------------------------------------- the source

# A completed pull is promoted into the digest-keyed cache only after it has
# been verified against the pin, so the directory name can never lie about its
# contents, and a moved head can never destroy an older valid cache.
PULL_ROOT=""
PROMOTE=0
# Explicitly `return 0`: under `set -e` a trap whose first command reports
# failure never reaches its second, so a no-op cleanup would leave the metadata
# directory behind *and* make a successful build exit non-zero.
cleanup() { [[ -n "${PULL_ROOT}" ]] && rm -rf "${PULL_ROOT}"; return 0; }
trap cleanup EXIT

# Finish an interrupted promotion.
#
# Promotion is two same-filesystem renames — old cache aside, new cache into
# place — and a kill or a power loss between them leaves the canonical cache
# absent with a complete copy sitting in `.replaced`. Nothing is corrupt, but
# the next run would pull again, which is the one thing that needs the network.
# So the previous cache is put back.
#
# `.replaced` exists only if staging had already completed, so it is a
# *complete* cache by construction. `.incoming` is not: an interrupted
# cross-filesystem copy leaves a partial one, and there is no way to tell the
# two apart, so it is removed as debris and a fresh pull is the right answer.
#
# Recovery is not a trust decision either way: a restored cache goes through
# `postvec model show --verify` and check-model-bundle.py below exactly like a
# live pull, so a wrong or damaged one still fails closed.
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

# ------------------------------------------------- exactly one model, one backend

mapfile -t INSTALLED < <(
    find "${SOURCE_ROOT}/models" -mindepth 2 -maxdepth 2 -type d -printf '%P\n' \
        | LC_ALL=C sort
)
(( ${#INSTALLED[@]} )) || die "no model was installed under ${SOURCE_ROOT}/models"
if (( ${#INSTALLED[@]} != 1 )); then
    printf '  %s\n' "${INSTALLED[@]}" >&2
    die "the pull produced ${#INSTALLED[@]} model directories (above).
A dependency closure is a deliberate packaging decision — one package per model
identity — not something to absorb into one binary package. See
docs/postgres/packaging-registry-models.md §8."
fi
BACKEND="${INSTALLED[0]%%/*}"
INSTALLED_NAME="${INSTALLED[0]#*/}"
[[ "${INSTALLED_NAME}" == "${BUNDLED_MODEL_NAME}" ]] \
    || die "the engine root holds ${INSTALLED_NAME}, not the pinned ${BUNDLED_MODEL_NAME}"
SOURCE_MODEL_DIR="${SOURCE_ROOT}/models/${INSTALLED[0]}"

# ------------------------------------------------------------------ activation

# `postvec model pull` installs deactivated by design: the descriptor's
# `enabled` field is the operator's persistent power switch, and `postvec
# model activate` is the only sanctioned way to flip it. The bundled model is
# the one the complete image must serve at startup, so packaging makes the
# same explicit choice an operator would — through the CLI, which updates the
# receipt's recorded hash with it, so the verification below still proves
# "archive bytes plus exactly this one recorded flip".
#
# Skip when the descriptor is already active: a fresh pull is activated here
# in its 0700 temp root and then promoted, so the cache always holds the
# activated state — and re-activating the cache would trip the CLI's
# engine-root trust policy on an ordinary group-writable build/ directory
# (mode 775), which is a check about managed serving roots, not about this
# staging area.
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

# ------------------------------------------------------------- integrity, then facts

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

# Promote only now: the bytes have been proved to be the pinned archive.
#
# Staged on the *destination* filesystem first, then renamed into place. The
# cross-filesystem case (a TMPDIR elsewhere) is a copy, and a copy can be
# interrupted or run out of disk — so it must not be the thing that overwrites
# a good cache. Only the final rename touches ${CACHE_ROOT}, and a rename
# within one filesystem is atomic: an interrupted promotion leaves the previous
# cache intact and a `.incoming` directory the next run removes.
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
    SOURCE_MODEL_DIR="${SOURCE_ROOT}/models/${INSTALLED[0]}"
    log "cached at ${CACHE_ROOT}"
fi

# The facts are validator-constrained; reading them here is what makes the
# package name, version and doc directory follow the pin rather than a literal.
parse_env_file "${META}/model-facts.env" model-facts.env

MODEL_DIR="${PAYLOAD}/opt/postvec/ninference/models/${MODEL_BACKEND}/${MODEL_NAME}"
DOC="${PAYLOAD}/usr/share/doc/${MODEL_PKG_NAME}"

# ------------------------------------------------------------------ the payload

# Every previous bundle goes, including one built under a different model name
# or package name: a leftover tree would be packaged by the `type: tree` entry
# and would ship two models in a package that names one.
rm -rf "${PAYLOAD}/opt/postvec/ninference/models" \
       "${PAYLOAD}/usr/share/doc" \
       "${PAYLOAD}/model-facts.env"
mkdir -p "${MODEL_DIR}" "${DOC}"

mapfile -d '' -t ARCHIVE_FILES < "${META}/receipt-files.nul"
for rel in "${ARCHIVE_FILES[@]}"; do
    mkdir -p "${MODEL_DIR}/$(dirname "${rel}")"
    cp "${SOURCE_MODEL_DIR}/${rel}" "${MODEL_DIR}/${rel}"
done

# What landed must be exactly what the receipt listed — no more (the receipt is
# not package content) and no less.
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

# ---------------------------------------------------------- provenance and digests

# The archive always carries a licence — the publisher adds one when the model
# directory lacks it — so its absence is a publication defect rather than
# something packaging may substitute for. check-model-bundle.py has already
# refused that case; this is the copy.
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
