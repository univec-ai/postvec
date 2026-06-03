#!/usr/bin/env bash
# Decide what a release run may do. Writes key=value to stdout for
# $GITHUB_OUTPUT; human text goes to stderr.
#
#   POSTVEC_MODE  rehearse | publish | disposable-publication
#   POSTVEC_RELEASE_REF   ref being released
#   POSTVEC_DISPATCH_REF  ref the workflow file came from
#   POSTVEC_IMAGE_REPOSITORY / POSTVEC_IMAGE_STAGING_REPOSITORY  optional
#
# rehearse: push nothing, any ref.
# publish: pinned repos, no overrides, refuse a rehearsal tag.
# disposable-publication: both overrides and a postvec-rehearsal-v* tag.
#
# Publishing modes require dispatch ref == released ref. Run this from the
# dispatch ref before checking out the released ref, or a branch would
# authorise its own publication.

set -Eeuo pipefail

MODE="${POSTVEC_MODE:-}"
RELEASE_REF="${POSTVEC_RELEASE_REF:-}"
DISPATCH_REF="${POSTVEC_DISPATCH_REF:-}"
REPOSITORY_OVERRIDE="${POSTVEC_IMAGE_REPOSITORY:-}"
STAGING_OVERRIDE="${POSTVEC_IMAGE_STAGING_REPOSITORY:-}"

# `::error::` is a GitHub annotation and harmless anywhere else.
fail() { while (($#)); do printf '::error::%s\n' "$1" >&2; shift; done; exit 1; }
note() { printf '%s\n' "$*" >&2; }

[[ -n "${RELEASE_REF}" ]] || fail "POSTVEC_RELEASE_REF is empty; there is nothing to build"

publish=false
environment=""

case "${MODE}" in
rehearse)
    if [[ -n "${REPOSITORY_OVERRIDE}${STAGING_OVERRIDE}" ]]; then
        fail "a rehearsal pushes nothing, so a repository override means nothing. Remove it."
    fi
    note "rehearsing ${RELEASE_REF} — nothing will be pushed"
    ;;

publish)
    publish=true; environment=postvec-release
    if [[ -n "${REPOSITORY_OVERRIDE}${STAGING_OVERRIDE}" ]]; then
        fail "a real release publishes to the repositories pinned in versions.env." \
             "Use mode=disposable-publication if you meant to publish somewhere else."
    fi
    case "${RELEASE_REF}" in
    postvec-rehearsal-v*)
        fail "${RELEASE_REF} is a rehearsal tag. Publishing it would advance the" \
             "production moving tags from a scratch build." ;;
    postvec-v*) : ;;
    *)
        fail "${RELEASE_REF} is not a postvec-v* tag; releases are cut from tags only" ;;
    esac
    note "publishing ${RELEASE_REF} to the pinned repositories"
    ;;

disposable-publication)
    publish=true; environment=postvec-release
    if [[ -z "${REPOSITORY_OVERRIDE}" || -z "${STAGING_OVERRIDE}" ]]; then
        fail "disposable-publication requires BOTH image_repository and" \
             "image_staging_repository. Half an override publishes half a release" \
             "into production."
    fi
    case "${RELEASE_REF}" in
    postvec-rehearsal-v*) : ;;
    *)
        fail "${RELEASE_REF} is not a postvec-rehearsal-v* tag." \
             "A disposable publication creates and deletes a real GitHub release;" \
             "done under the production tag it would occupy the identity the real" \
             "publication needs, and that release can only be published once." ;;
    esac
    printf '::warning::disposable publication rehearsal — %s\n' "${REPOSITORY_OVERRIDE}" >&2
    note "publishing ${RELEASE_REF} to a throwaway namespace"
    ;;

*)
    fail "unknown mode '${MODE}' (expected: rehearse, publish, disposable-publication)"
    ;;
esac

# The workflow file's own version, for anything that publishes.
#
# Absent is not "skip the check". A missing POSTVEC_DISPATCH_REF means this gate
# has been wired up wrongly — and a security check that passes when it cannot
# tell is not a check. GitHub always supplies `github.ref_name`; if it ever
# stops, or a caller forgets to pass it, publication stops with it.
if [[ "${publish}" == true && -z "${DISPATCH_REF}" ]]; then
    fail "POSTVEC_DISPATCH_REF is empty, so the ref this workflow is running from" \
         "cannot be compared with the ref being released. Publication is refused:" \
         "an unverifiable binding is not a binding."
fi

if [[ "${publish}" == true && "${DISPATCH_REF}" != "${RELEASE_REF}" ]]; then
    # The remedy, complete enough to paste — including the inputs the mode
    # requires, because an invocation that is missing them fails the *next*
    # check and sends someone round the loop again.
    extra=""
    if [[ "${MODE}" == disposable-publication ]]; then
        extra=" \\
    -f image_repository=${REPOSITORY_OVERRIDE} \\
    -f image_staging_repository=${STAGING_OVERRIDE}"
    fi
    fail "this run was dispatched from '${DISPATCH_REF}', so it is executing that ref's" \
         "workflow — but it was asked to release '${RELEASE_REF}'." \
         "Dispatch from the tag itself:" \
         "  gh workflow run postvec-release.yml --ref ${RELEASE_REF} \\
    -f ref=${RELEASE_REF} \\
    -f mode=${MODE}${extra}"
fi

printf 'publish=%s\n' "${publish}"
printf 'environment=%s\n' "${environment}"
