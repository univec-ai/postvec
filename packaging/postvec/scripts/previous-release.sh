#!/usr/bin/env bash
# The previous postvec release, as a git tag or a published GitHub Release.
#
#   previous-release.sh              # newest tag with a lower product version
#   previous-release.sh --published  # newest GitHub Release with a lower product version
#   previous-release.sh --versions   # those product versions, oldest first
#   previous-release.sh --identity   # newest tag with a lower release identity
#
# A lower product version is 0.1.0 when this tree is 0.2.0 or 0.1.1. That is
# the hop `upgrade_test.sh` and the upgrade graph walk. A lower *identity* also
# includes a same-version packaging predecessor: 0.2.0-1 when this tree is
# 0.2.0-2. `assert-versions.sh` uses --identity to freeze upgrade scripts that
# the previous tag already shipped.
#
# Prints the tag (or versions) on stdout. Empty stdout and exit 0 when there is
# no match: the first release, or a packaging-only rebuild of the current
# product when the query is for an older product. Exit 1 if git or gh fails.
#
# A 0.1.1 hotfix in a repository that already has 0.2.0 tagged upgrades from
# 0.1.0. Rehearsal tags (postvec-rehearsal-v*) are ignored.
#
# Test overrides: POSTVEC_CURRENT_VERSION, POSTVEC_CURRENT_RELEASE,
# POSTVEC_TAG_REPO, POSTVEC_GH_REPO.

source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

load_versions

MODE=tag
while (($#)); do
    case "$1" in
    --published) MODE=published; shift ;;
    --versions)  MODE=versions; shift ;;
    --identity)  MODE=identity; shift ;;
    -h|--help)   sed -n '2,24p' "$0"; exit 0 ;;
    *)           die "unknown argument: $1" ;;
    esac
done

CURRENT="${POSTVEC_CURRENT_VERSION:-${POSTVEC_VERSION}}"
CURRENT_RELEASE="${POSTVEC_CURRENT_RELEASE:-${PACKAGE_RELEASE}}"
TAG_REPO="${POSTVEC_TAG_REPO:-${REPO_ROOT}}"

# Product version inside a postvec-vMAJOR.MINOR.PATCH[-REVISION] tag. Empty
# for rehearsal tags, moving tags and other names.
product_version() {
    sed -nE 's/^postvec-v([0-9]+\.[0-9]+\.[0-9]+)(-[0-9]+)?$/\1/p' <<<"$1"
}

# postvec-v0.1.0-1 -> version=0.1.0 revision=1. Empty when the tag has no
# packaging revision: those names are not release identities.
parse_identity() {
    local tag="$1"
    if [[ "${tag}" =~ ^postvec-v([0-9]+\.[0-9]+\.[0-9]+)-([0-9]+)$ ]]; then
        printf '%s %s\n' "${BASH_REMATCH[1]}" "${BASH_REMATCH[2]}"
    fi
}

# Tags of an older product version, newest first (the input is already
# version-sorted descending).
older_product_tags() {
    local tag version
    while read -r tag; do
        [[ -n "${tag}" ]] || continue
        version="$(product_version "${tag}")"
        [[ -n "${version}" ]] || continue
        version_less "${version}" "${CURRENT}" && printf '%s\n' "${tag}"
    done
}

# Tags with a lower release identity: older product, or the same product with
# a lower PACKAGE_RELEASE.
older_identity_tags() {
    local tag version revision
    while read -r tag; do
        [[ -n "${tag}" ]] || continue
        read -r version revision <<<"$(parse_identity "${tag}")"
        [[ -n "${version}" && -n "${revision}" ]] || continue
        if version_less "${version}" "${CURRENT}"; then
            printf '%s\n' "${tag}"
        elif [[ "${version}" == "${CURRENT}" ]] && (( 10#${revision} < 10#${CURRENT_RELEASE} )); then
            printf '%s\n' "${tag}"
        fi
    done
}

list_git_tags() {
    git -C "${TAG_REPO}" tag --list 'postvec-v*' --sort=-v:refname \
        || die "git tag --list failed in ${TAG_REPO}"
}

case "${MODE}" in
tag)
    tags="$(list_git_tags)"
    older_product_tags <<<"${tags}" | head -n1 || true
    ;;
versions)
    tags="$(list_git_tags)"
    older_product_tags <<<"${tags}" \
        | while read -r tag; do product_version "${tag}"; done \
        | sort -Vu
    ;;
identity)
    tags="$(list_git_tags)"
    older_identity_tags <<<"${tags}" | head -n1 || true
    ;;
published)
    need gh
    # POSTVEC_GH_REPO first: GitHub Actions always sets GITHUB_REPOSITORY.
    slug="${POSTVEC_GH_REPO:-${GITHUB_REPOSITORY:-${GH_REPO:-${SOURCE_REPOSITORY#https://github.com/}}}}"
    slug="${slug%.git}"
    # Drafts are excluded: a half-finished publish has no packages to download.
    # Pre-releases are included: 0.1.0-1 is marked pre-release on purpose.
    tags="$(gh release list --repo "${slug}" --exclude-drafts --limit 100 \
        --json tagName --jq '.[].tagName')" \
        || die "gh release list --repo ${slug} failed"
    older_product_tags <<<"${tags}" | head -n1 || true
    ;;
esac
