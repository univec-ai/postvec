#!/usr/bin/env bash
# Check a published release the way a user would: from outside, over the
# network, against nothing this machine built.
#
#   verify-published-release.sh                       # the pinned identity
#   verify-published-release.sh --tag postvec-v0.1.0-1
#   verify-published-release.sh --pattern '*el9.x86_64.rpm'
#   verify-published-release.sh --all                 # every asset (~2 GB)
#   verify-published-release.sh --no-images
#
# By default it downloads the metadata assets plus the Debian 12 / amd64
# packages, because verifying a few real packages proves the chain and
# downloading eighty proves it eighty times. `--all` is the complete check, and
# is what the release itself runs before publishing the draft.
#
# What it establishes, in order:
#
#   1. the checksums in SHA256SUMS match the bytes GitHub served;
#   2. the assets carry a provenance attestation naming *this repository's
#      release workflow* — not merely "something in this repository";
#   3. every image the manifest records resolves, is attested, and really is a
#      multi-architecture index with both linux children.
#
# Anything short of that is "the file downloaded", which is not verification.

source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

TAG=""; DIR=""; SLUG=""; ALL=0; IMAGES=1
PATTERNS=()
while (($#)); do
    case "$1" in
    --tag)     TAG="$2"; shift 2 ;;
    --repo)    SLUG="$2"; shift 2 ;;
    --dir)     DIR="$2"; shift 2 ;;
    --pattern) PATTERNS+=("$2"); shift 2 ;;
    --all)     ALL=1; shift ;;
    --no-images) IMAGES=0; shift ;;
    -h|--help) sed -n '2,25p' "$0"; exit 0 ;;
    *) die "unknown argument: $1" ;;
    esac
done

load_versions
need gh jq sha256sum

[[ -n "${TAG}" ]] || TAG="${RELEASE_TAG}"
if [[ -z "${SLUG}" ]]; then
    SLUG="${GH_REPO:-${SOURCE_REPOSITORY#https://github.com/}}"
    SLUG="${SLUG%.git}"
fi
[[ -n "${DIR}" ]] || DIR="$(mktemp -d -t postvec-verify-XXXXXX)"
mkdir -p "${DIR}"

SIGNER="${SLUG}/.github/workflows/postvec-release.yml"
fail=0
check() {
    local what="$1"; shift
    if "$@" >/dev/null 2>&1; then
        printf '  ok    %s\n' "${what}"
    else
        printf '  FAIL  %s\n' "${what}"
        fail=1
    fi
}

log "verifying ${TAG} from ${SLUG} in ${DIR}"

# ------------------------------------------------------------------ download
if ((ALL)); then
    gh release download "${TAG}" --repo "${SLUG}" --dir "${DIR}" --clobber
else
    ((${#PATTERNS[@]})) || PATTERNS=('*deb12_amd64.deb')
    args=(--pattern SHA256SUMS --pattern postvec-release.json
          --pattern postvec-prerequisites.sh)
    for pattern in "${PATTERNS[@]}"; do args+=(--pattern "${pattern}"); done
    gh release download "${TAG}" --repo "${SLUG}" --dir "${DIR}" --clobber "${args[@]}"
fi
cd "${DIR}" || die "cannot enter ${DIR}"

# ----------------------------------------------------------------- checksums
printf '\nchecksums\n'
if ((ALL)); then
    # Strict: every listed file must be present and correct. The release
    # workflow additionally proves the asset *set* is exactly the release
    # directory — `sha256sum --check` verifies what it was told about and says
    # nothing about an extra file.
    check "SHA256SUMS (complete)" sha256sum --check SHA256SUMS
else
    check "SHA256SUMS (downloaded subset)" sha256sum --ignore-missing --check SHA256SUMS
fi

# --------------------------------------------------------------- attestations
printf '\nattestations (signer: %s)\n' "${SIGNER}"
for asset in postvec-prerequisites.sh SHA256SUMS postvec-release.json; do
    [[ -f "${asset}" ]] || { printf '  FAIL  %s is not in the release\n' "${asset}"; fail=1; continue; }
    check "${asset}" gh attestation verify "${asset}" --repo "${SLUG}" --signer-workflow "${SIGNER}"
done

shopt -s nullglob
packages=(./*.deb ./*.rpm)
shopt -u nullglob
if ((${#packages[@]} == 0)); then
    printf '  skip  no packages downloaded (adjust --pattern)\n'
fi
for package in "${packages[@]}"; do
    check "${package#./}" gh attestation verify "${package}" \
        --repo "${SLUG}" --signer-workflow "${SIGNER}"
done

# The bootstrap is the one asset the install instructions tell a user to
# execute, and the manifest has to agree about which bytes those are.
if [[ -f postvec-release.json && -f postvec-prerequisites.sh ]]; then
    recorded="$(jq -r '.release_assets[] | select(.role == "prerequisites") | .sha256' \
                  postvec-release.json)"
    actual="$(sha256sum postvec-prerequisites.sh | cut -d' ' -f1)"
    if [[ "${recorded}" == "${actual}" ]]; then
        printf '  ok    manifest records the bootstrap hash\n'
    else
        printf '  FAIL  manifest bootstrap hash %s is not the published file (%s)\n' \
            "${recorded:-<missing>}" "${actual}"
        fail=1
    fi
    printf '  ok    manifest release_id %s, git_tag %s\n' \
        "$(jq -r .release_id postvec-release.json)" \
        "$(jq -r .git_tag postvec-release.json)"
fi

# --------------------------------------------------------------------- images
if ((IMAGES)) && [[ -f postvec-release.json ]]; then
    printf '\nimages\n'
    if ! command -v docker >/dev/null 2>&1; then
        printf '  skip  docker is not installed; image checks not run\n'
    else
        while IFS=$'\t' read -r name tag digest; do
            check "${name}:${tag} attested" gh attestation verify "oci://${name}@${digest}" \
                --repo "${SLUG}" --signer-workflow "${SIGNER}"
            # A tag that resolves is not the claim; a tag that resolves to the
            # recorded digest *and* carries both architectures is.
            resolved="$(docker buildx imagetools inspect "${name}:${tag}" \
                          --format '{{.Manifest.Digest}}' 2>/dev/null || true)"
            if [[ "${resolved}" == "${digest}" ]]; then
                printf '  ok    %s:%s resolves to the recorded digest\n' "${name}" "${tag}"
            else
                printf '  FAIL  %s:%s resolves to %s, manifest records %s\n' \
                    "${name}" "${tag}" "${resolved:-<nothing>}" "${digest}"
                fail=1
            fi
            arches="$(docker buildx imagetools inspect "${name}@${digest}" --raw 2>/dev/null \
                       | jq -r '[.manifests[] | select(.platform.os == "linux")
                                 | .platform.architecture] | sort | join(",")' || true)"
            if [[ "${arches}" == "amd64,arm64" ]]; then
                printf '  ok    %s:%s is amd64 + arm64\n' "${name}" "${tag}"
            else
                printf '  FAIL  %s:%s children are [%s]\n' "${name}" "${tag}" "${arches}"
                fail=1
            fi
        done < <(jq -r '.images[] | [.name, .tag, .digest] | @tsv' postvec-release.json)
    fi
fi

printf '\n'
if ((fail)); then
    die "verification failed in ${DIR} — do not tell anyone to install this yet"
fi
log "release ${TAG} verifies as a user would see it (${DIR})"
