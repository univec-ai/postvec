#!/usr/bin/env bash
# Start a postvec-release run without getting the two refs wrong.
#
#   dispatch-release.sh rehearse [<ref>]      # build everything, push nothing
#   dispatch-release.sh disposable-publication
#   dispatch-release.sh publish
#   dispatch-release.sh --print-only publish  # print the command, run nothing
#   dispatch-release.sh --watch publish       # follow the run afterwards
#
# `gh workflow run --ref X -f ref=Y` has two refs on purpose: `--ref` chooses
# which copy of the *workflow file* GitHub executes, `-f ref=` chooses what it
# checks out and builds. For a publication they must be the same tag, or the run
# attests one commit's workflow against another commit's artifacts — and the
# workflow refuses, hours in. This script derives both from versions.env, so the
# pair cannot disagree.
#
# It is a convenience, not a gate: every rule it observes is enforced again by
# scripts/release-mode.sh inside the run, where it counts.

source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

MODE=""; REF=""; PRINT_ONLY=0; WATCH=0
IMAGE_OVERRIDE=""; STAGING_OVERRIDE=""
while (($#)); do
    case "$1" in
    --print-only) PRINT_ONLY=1; shift ;;
    --watch)      WATCH=1; shift ;;
    --image-repository)         IMAGE_OVERRIDE="$2"; shift 2 ;;
    --image-staging-repository) STAGING_OVERRIDE="$2"; shift 2 ;;
    -h|--help)    sed -n '2,19p' "$0"; exit 0 ;;
    -*)           die "unknown option: $1" ;;
    rehearse|publish|disposable-publication) MODE="$1"; shift ;;
    disposable)   MODE=disposable-publication; shift ;;
    *)            [[ -z "${MODE}" ]] && die "unknown mode: $1"; REF="$1"; shift ;;
    esac
done
[[ -n "${MODE}" ]] || die "no mode given (rehearse, publish, disposable-publication)"

load_versions
need git

SLUG="${GH_REPO:-${SOURCE_REPOSITORY#https://github.com/}}"
SLUG="${SLUG%.git}"

# What each mode releases, and where its images go. The disposable namespaces
# are derived from the pinned one so they are obviously related and obviously
# not it; `release-mode.sh` refuses them if they ever collide.
#
# DISPATCH_REF is what `--ref` gets and must be a *name* — GitHub resolves it to
# pick the workflow file, and answers `422 No ref found` for a commit SHA. REF is
# what `-f ref=` gets and is what the run checks out and builds; a rehearsal may
# pin an exact commit there. For the publishing modes the two are the same tag,
# which `release-mode.sh` re-checks inside the run.
ARGS=()
DISPATCH_REF=""
case "${MODE}" in
rehearse)
    [[ -n "${REF}" ]] || REF="$(git -C "${REPO_ROOT}" rev-parse --abbrev-ref HEAD)"
    # A branch or tag name is its own dispatch ref. Anything else — a SHA, or
    # `HEAD` — is built from the workflow file on the branch that contains it.
    if git -C "${REPO_ROOT}" show-ref --verify --quiet "refs/heads/${REF}" \
        || git -C "${REPO_ROOT}" show-ref --verify --quiet "refs/tags/${REF}"; then
        DISPATCH_REF="${REF}"
    else
        DISPATCH_REF="$(git -C "${REPO_ROOT}" rev-parse --abbrev-ref HEAD)"
        [[ "${DISPATCH_REF}" != HEAD ]] || die "detached HEAD: name the branch \
whose workflow file should run, e.g. 'dispatch-release.sh rehearse main'"
        log "dispatching the workflow file from '${DISPATCH_REF}', building '${REF}'"
    fi
    ;;
publish)
    REF="${RELEASE_TAG}"
    ;;
disposable-publication)
    REF="${REHEARSAL_TAG}"
    ARGS+=(-f "image_repository=${IMAGE_OVERRIDE:-${IMAGE_REPOSITORY}-rehearsal}")
    ARGS+=(-f "image_staging_repository=${STAGING_OVERRIDE:-${IMAGE_REPOSITORY}-rehearsal-staging}")
    ;;
esac
# Publishing modes attest the workflow file against the artifacts, so the two
# refs must be the same tag.
[[ -n "${DISPATCH_REF}" ]] || DISPATCH_REF="${REF}"

print_command() {
    printf '\n  gh workflow run postvec-release.yml \\\n'
    printf '    --repo %s \\\n' "${SLUG}"
    printf '    --ref %s \\\n' "${DISPATCH_REF}"
    printf '    -f ref=%s \\\n' "${REF}"
    printf '    -f mode=%s' "${MODE}"
    local i
    for ((i = 0; i < ${#ARGS[@]}; i += 2)); do
        printf ' \\\n    %s %s' "${ARGS[i]}" "${ARGS[i + 1]}"
    done
    printf '\n\n'
}

if ((PRINT_ONLY)); then
    print_command
    exit 0
fi

need gh

# For a publication the ref must be a tag that exists here and on origin. The
# workflow checks this too; failing now costs a second rather than a queue slot.
if [[ "${MODE}" != rehearse ]]; then
    git -C "${REPO_ROOT}" rev-parse --verify "refs/tags/${REF}" >/dev/null 2>&1 \
        || die "no such tag: ${REF}
Create it on the commit you mean to release:
  git tag -a ${REF} -m 'postvec ${RELEASE_ID}' && git push origin ${REF}"
    git -C "${REPO_ROOT}" ls-remote --exit-code --tags origin "${REF}" >/dev/null 2>&1 \
        || die "${REF} is not on origin — run: git push origin ${REF}"
fi

log "dispatching ${MODE} for ${REF} on ${SLUG}"
print_command

if [[ "${MODE}" == publish ]]; then
    # The one irreversible dispatch. GitHub still asks for the environment
    # approval, but by then the run has spent hours of Actions minutes.
    warn "this publishes postvec ${RELEASE_ID} to ${IMAGE_REPOSITORY} and to the ${SLUG} releases page."
    read -r -p "Type the release id (${RELEASE_ID}) to continue: " confirm
    [[ "${confirm}" == "${RELEASE_ID}" ]] || die "not confirmed; nothing dispatched"
fi

gh workflow run postvec-release.yml \
    --repo "${SLUG}" \
    --ref "${DISPATCH_REF}" \
    -f "ref=${REF}" \
    -f "mode=${MODE}" \
    "${ARGS[@]}"

log "dispatched. GitHub takes a moment to create the run."
if ((WATCH)); then
    sleep 10
    gh run list --repo "${SLUG}" --workflow postvec-release.yml --limit 1
    gh run watch --repo "${SLUG}" \
        "$(gh run list --repo "${SLUG}" --workflow postvec-release.yml \
             --limit 1 --json databaseId --jq '.[0].databaseId')"
else
    printf '\nWatch it with:\n  gh run list  --repo %s --workflow postvec-release.yml --limit 1\n' "${SLUG}"
    printf '  gh run watch --repo %s <run-id>\n\n' "${SLUG}"
fi

if [[ "${MODE}" != rehearse ]]; then
    log "the run will pause for the postvec-release environment approval before it builds packages."
fi
