#!/usr/bin/env bash
# Make the mechanical edits for the next release.
#
#   bump-release.sh 0.2.1          # code release: version, lockfiles, upgrade script, changelog
#   bump-release.sh --packaging    # packaging-only: PACKAGE_RELEASE + 1, changelog
#   bump-release.sh --docs         # re-sync compose examples and site.ts (the bumps do it too)
#
# A code release sets POSTVEC_VERSION and the three crate versions, resets
# PACKAGE_RELEASE to 1, updates both lockfiles, creates
# postvec/sql/postvec--<old>--<new>.sql (schema lock only; add DDL below it),
# freezes the changelog's top entry and prepends a DRAFT one. It finishes with
# assert-versions.sh. Nothing is committed.
#
# Options: --no-lock skips the lockfile update and the final gate (tests).

source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

MODE="" NEW="" LOCK=1
while (($#)); do
    case "$1" in
    --packaging)
        [[ -z "${MODE}" ]] || die "one mode only (got ${MODE} and --packaging)"
        MODE=packaging; shift ;;
    --docs)
        [[ -z "${MODE}" ]] || die "one mode only (got ${MODE} and --docs)"
        MODE=docs; shift ;;
    --no-lock)   LOCK=0; shift ;;
    -h|--help)   sed -n '2,14p' "$0"; exit 0 ;;
    -*)          die "unknown option: $1" ;;
    *)
        [[ -z "${MODE}" || "${MODE}" == code ]] || die "unexpected argument: $1"
        [[ -z "${NEW}" ]] || die "unexpected argument: $1"
        MODE=code; NEW="$1"; shift ;;
    esac
done
[[ -n "${MODE}" ]] || die "name the new version (bump-release.sh 0.2.1), or --packaging, or --docs"

load_versions
OLD="${POSTVEC_VERSION}" OLD_REL="${PACKAGE_RELEASE}"
ENV_FILE="${PKG_DIR}/versions.env"
CHANGELOG="${PKG_DIR}/changelog.Debian"
TEMPLATED_HEADER='postvec (${POSTVEC_VERSION}-${PACKAGE_RELEASE}) unstable; urgency=medium'

if [[ "${MODE}" != docs && -n "$(git -C "${REPO_ROOT}" status --porcelain)" ]]; then
    die "the working tree has uncommitted changes; commit the fix first, then bump"
fi

set_env() {  # <key> <old> <new>
    grep -qx "$1=$2" "${ENV_FILE}" || die "versions.env: expected $1=$2"
    sed -i "s/^$1=$2\$/$1=$3/" "${ENV_FILE}"
}

# Freeze the templated top entry at the identity being superseded, then put a
# new templated entry above it.
bump_changelog() {  # <released identity> <new identity, for the DRAFT text>
    [[ "$(head -n1 "${CHANGELOG}")" == "${TEMPLATED_HEADER}" ]] \
        || die "changelog.Debian: the top entry is not the templated one; fix it by hand"
    local frozen body
    frozen="postvec ($1) unstable; urgency=medium"
    body="$(tail -n +2 "${CHANGELOG}")"
    {
        printf '%s\n\n' "${TEMPLATED_HEADER}"
        printf '  * DRAFT: describe what changed in %s.\n\n' "$2"
        printf ' -- ${MAINTAINER}  %s\n\n' "$(date -R)"
        printf '%s\n%s\n' "${frozen}" "${body}"
    } > "${CHANGELOG}.new"
    mv "${CHANGELOG}.new" "${CHANGELOG}"
}

# Point the compose examples and the website at a release. The site is only
# deployed after the publish (handbook step 10), so main may name it early.
point_docs() {  # <version> <packaging revision>
    local id="$1-$2" file site="${REPO_ROOT}/web/.vitepress/theme/site.ts"
    for file in "${PKG_DIR}"/docker/compose/*.yml; do
        sed -i -E "s#(ghcr\.io/univec-ai/postvec(-server)?:)[0-9]+\.[0-9]+\.[0-9]+-[0-9]+#\1${id}#g" "${file}"
    done
    sed -i -E \
        -e "s#^(\s+releaseTag: )\"[^\"]*\"#\1\"postvec-v${id}\"#" \
        -e "s#^(\s+version: )\"[^\"]*\"#\1\"$1\"#" \
        -e "s#^(\s+release: )\"[^\"]*\"#\1\"${id}\"#" \
        -e "s#^(\s+packageRelease: )\"[^\"]*\"#\1\"$2\"#" "${site}"
    log "compose examples and site.ts now point at ${id}"
}

case "${MODE}" in
code)
    [[ "${NEW}" =~ ^[0-9]+\.[0-9]+\.[0-9]+$ ]] || die "not MAJOR.MINOR.PATCH: ${NEW}"
    version_less "${OLD}" "${NEW}" || die "${NEW} is not newer than ${OLD}"

    set_env POSTVEC_VERSION "${OLD}" "${NEW}"
    [[ "${OLD_REL}" == 1 ]] || set_env PACKAGE_RELEASE "${OLD_REL}" 1
    for manifest in postvec postvec-cli postvec-server; do
        file="${REPO_ROOT}/${manifest}/Cargo.toml"
        grep -qx "version = \"${OLD}\"" "${file}" || die "${manifest}/Cargo.toml: expected version = \"${OLD}\""
        sed -i "0,/^version = \"${OLD//./\\.}\"\$/s//version = \"${NEW}\"/" "${file}"
    done

    script="${REPO_ROOT}/postvec/sql/postvec--${OLD}--${NEW}.sql"
    if [[ -e "${script}" ]]; then
        warn "keeping the existing $(basename "${script}")"
    else
        cat > "${script}" <<EOF
-- postvec ${OLD} -> ${NEW}. Applied by ALTER EXTENSION postvec UPDATE; see README.md.
-- Schema changes go below the lock. upgrade_test.sh compares the result with a fresh install.
SELECT pg_advisory_xact_lock(hashtext('postvec_schema'));
EOF
    fi
    if ! git -C "${REPO_ROOT}" rev-parse --verify --quiet "refs/tags/postvec-v${OLD}-${OLD_REL}" >/dev/null; then
        warn "${OLD} has no release tag here: if it was never published, rename the existing
postvec--*--${OLD}.sql to end at ${NEW} instead of keeping an extra hop (or git fetch --tags)"
    fi

    bump_changelog "${OLD}-${OLD_REL}" "${NEW}-1"
    if ((LOCK)); then
        (cd "${REPO_ROOT}" && cargo update --workspace --quiet)
        (cd "${REPO_ROOT}/postvec" && cargo update -p postvec --quiet)
    fi
    point_docs "${NEW}" 1
    log "bumped ${OLD}-${OLD_REL} -> ${NEW}-1"
    ;;
packaging)
    new_rel=$((OLD_REL + 1))
    set_env PACKAGE_RELEASE "${OLD_REL}" "${new_rel}"
    bump_changelog "${OLD}-${OLD_REL}" "${OLD}-${new_rel}"
    point_docs "${OLD}" "${new_rel}"
    log "bumped ${OLD}-${OLD_REL} -> ${OLD}-${new_rel} (packaging only)"
    ;;
docs)
    point_docs "${OLD}" "${OLD_REL}"
    git -C "${REPO_ROOT}" --no-pager diff --stat -- packaging/postvec/docker/compose web/.vitepress/theme/site.ts
    exit 0
    ;;
esac

git -C "${REPO_ROOT}" --no-pager diff --stat
git -C "${REPO_ROOT}" status --porcelain --untracked-files=all -- postvec/sql
((LOCK)) && "${PKG_DIR}/scripts/assert-versions.sh"
cat <<EOF

Next: replace the DRAFT line in packaging/postvec/changelog.Debian, add any DDL
to the upgrade script, run postvec/upgrade_test.sh, then commit.
EOF
