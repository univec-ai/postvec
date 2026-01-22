#!/usr/bin/env bash
# Decide what a release run is allowed to do, and refuse the combinations that
# would do the wrong thing.
#
# Reads its inputs from the environment and writes `key=value` lines to stdout
# for `$GITHUB_OUTPUT`; everything a human should read goes to stderr.
#
#   POSTVEC_MODE                        rehearse | publish | disposable-publication
#   POSTVEC_RELEASE_REF                 the ref being released (an input)
#   POSTVEC_DISPATCH_REF                the ref the workflow file came from
#   POSTVEC_IMAGE_REPOSITORY            optional override
#   POSTVEC_IMAGE_STAGING_REPOSITORY    optional override
#
# This lives in a script rather than in the workflow because it is the single
# decision that separates "built and tested" from "published to where users
# look", and a decision that cannot be run outside GitHub Actions cannot be
# tested outside GitHub Actions. tests/unit-test.sh drives the whole
# mode × tag × override cross-product through it.
#
# The rules, and the accident each one prevents:
#
#   rehearse                 pushes nothing anywhere, so it needs no environment
#                            and an override would mean nothing. Any ref: a
#                            rehearsal exists to run before the tag is cut.
#
#   publish                  a real release, to the pinned repositories. Refuses
#                            overrides — there is no dispatch that redirects a
#                            production publication — and refuses a rehearsal
#                            tag, which would advance the production moving tags
#                            from a scratch build.
#
#   disposable-publication   a full publication rehearsal. Requires *both*
#                            overrides, so half a form cannot publish half a
#                            release into production, and requires a
#                            `postvec-rehearsal-v*` tag: it creates and deletes a
#                            real GitHub release, and under the production tag it
#                            would occupy the identity the real publication
#                            needs — which can only be published once.
#
# And for both publishing modes, the dispatch ref must equal the released ref.
# An input controls what is checked out; it does not control which version of
# the workflow GitHub runs. A dispatch from a branch naming a tag would execute
# — and attest — the branch's workflow against the tag's artifacts.
#
# **This script must be run from the dispatch ref, before the released ref is
# checked out.** It decides whether publication is enabled, whether the
# protected environment is requested, and which repositories become job outputs.
# Run from the ref it is validating, a modified branch would supply the code
# that authorises its own publication — the check would be asking the suspect.
# The release workflow checks out the dispatch ref, runs this, and only then
# checks out what is being built.

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
