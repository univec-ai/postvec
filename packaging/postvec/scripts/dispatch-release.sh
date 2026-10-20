#!/usr/bin/env bash
# Start a postvec-release run without getting the two refs wrong.
#
#   dispatch-release.sh rehearse [<ref>]      # build everything, push nothing
#   dispatch-release.sh disposable-publication
#   dispatch-release.sh publish
#   dispatch-release.sh --print-only publish  # print the command, run nothing
#   dispatch-release.sh --watch publish       # follow the run afterwards
#
# `gh workflow run --ref X -f ref=Y` has two refs. `--ref` selects the
# workflow file. `-f ref=` selects what is checked out and built. A
# publication must use the same tag for both, or the run attests one
# commit's workflow against another's artifacts. Both come from versions.env.
#
# Convenience, not a gate. release-mode.sh enforces the same rules in the run.

source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

MODE=""; REF=""; PRINT_ONLY=0; WATCH=0
IMAGE_OVERRIDE=""; STAGING_OVERRIDE=""
while (($#)); do
    case "$1" in
    --print-only) PRINT_ONLY=1; shift ;;
    --watch)      WATCH=1; shift ;;
    --image-repository)         IMAGE_OVERRIDE="$2"; shift 2 ;;
    --image-staging-repository) STAGING_OVERRIDE="$2"; shift 2 ;;
    -h|--help)    sed -n '2,15p' "$0"; exit 0 ;;
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

# Disposable image names are derived from the pin and must not collide with it.
# DISPATCH_REF (--ref) must be a branch or tag name. GitHub rejects a SHA.
# REF (-f ref=) is what the run builds. Publishing modes use the same tag for both.
ARGS=()
DISPATCH_REF=""
case "${MODE}" in
rehearse)
    [[ -n "${REF}" ]] || REF="$(git -C "${REPO_ROOT}" rev-parse --abbrev-ref HEAD)"
    # A branch or tag is its own dispatch ref. A SHA or HEAD is built from
    # the workflow file on the current branch.
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
# Publishing modes use one tag for both refs. The run attests the workflow against the artifacts.
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

# A publication ref must be a tag here and on origin. Failing now saves a queue slot.
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
    # Irreversible. GitHub still asks for approval, after hours of Actions time.
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
