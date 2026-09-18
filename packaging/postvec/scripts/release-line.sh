#!/usr/bin/env bash
# Is this release the newest product line, or a maintenance release of an
# older one?
#
#   release-line.sh     # prints `newest`, or `maintenance <newer-tag>`
#
# `maintenance` when a GitHub Release of a strictly newer product version is
# already published: a 0.1.1 hotfix cut after 0.2.0 shipped. That release
# still publishes under its own versioned tags. The publish job then leaves
# GitHub's "Latest release" badge and the moving image tags (pg18-local,
# latest, ...) on the newer line.
#
# Drafts are excluded: a half-finished publish has no packages. Pre-releases
# are included: 0.1.0-1 is marked pre-release on purpose. This release's own
# version counts as current, so a re-run after publishing stays `newest`.
# Exit 1 when gh fails.
#
# Test overrides: POSTVEC_CURRENT_VERSION, POSTVEC_GH_REPO.

source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

load_versions
need gh

CURRENT="${POSTVEC_CURRENT_VERSION:-${POSTVEC_VERSION}}"
slug="${POSTVEC_GH_REPO:-${GITHUB_REPOSITORY:-${GH_REPO:-${SOURCE_REPOSITORY#https://github.com/}}}}"
slug="${slug%.git}"

tags="$(gh release list --repo "${slug}" --exclude-drafts --limit 100 \
    --json tagName --jq '.[].tagName')" \
    || die "gh release list --repo ${slug} failed"

newest_tag="" newest_version="${CURRENT}"
while read -r tag; do
    [[ -n "${tag}" ]] || continue
    version="$(sed -nE 's/^postvec-v([0-9]+\.[0-9]+\.[0-9]+)(-[0-9]+)?$/\1/p' <<<"${tag}")"
    [[ -n "${version}" ]] || continue
    if version_less "${newest_version}" "${version}"; then
        newest_version="${version}"
        newest_tag="${tag}"
    fi
done <<<"${tags}"

if [[ -n "${newest_tag}" ]]; then
    printf 'maintenance %s\n' "${newest_tag}"
else
    printf 'newest\n'
fi
