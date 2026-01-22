#!/usr/bin/env bash
# Everything that can be checked *before* dispatching a release run, in one
# read-only command. It writes nothing, pushes nothing and dispatches nothing.
#
#   release-preflight.sh                        # for the real publication
#   release-preflight.sh --mode rehearse        # before the full-matrix rehearsal
#   release-preflight.sh --mode disposable-publication
#
# The release workflow performs all of these checks itself — that is where they
# belong, because a check that only runs on a laptop is not a gate. This script
# exists so the answer arrives in five seconds instead of after two hours of
# compilation, and so the dispatch command at the end is one you can paste
# rather than assemble.
#
# Exit status: 0 when nothing blocks the dispatch, 1 otherwise. A check that
# could not be *performed* (no gh, no docker, no network) is reported as a skip,
# never as a pass: "could not look" is not "nothing there".

source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

MODE=publish
while (($#)); do
    case "$1" in
    --mode) MODE="$2"; shift 2 ;;
    -h|--help) sed -n '2,18p' "$0"; exit 0 ;;
    *) die "unknown argument: $1" ;;
    esac
done
case "${MODE}" in
rehearse|publish|disposable-publication) ;;
*) die "unknown mode '${MODE}' (expected: rehearse, publish, disposable-publication)" ;;
esac

load_versions

fail=0
ok()   { printf '  ok    %-30s %s\n' "$1" "${2-}"; }
bad()  { printf '  FAIL  %-30s %s\n' "$1" "${2-}"; fail=1; }
skip() { printf '  skip  %-30s %s\n' "$1" "${2-}"; }

# The tag this mode releases from, and the namespaces its images go to.
TAG=""
case "${MODE}" in
publish)                TAG="${RELEASE_TAG}" ;;
disposable-publication) TAG="${REHEARSAL_TAG}" ;;
esac

# owner/name, from the reviewed pin rather than from whatever remote this
# checkout happens to have. GH_REPO overrides, for a fork or a rename.
SLUG="${GH_REPO:-${SOURCE_REPOSITORY#https://github.com/}}"
SLUG="${SLUG%.git}"

log "postvec release preflight — mode ${MODE}"
printf '\nidentity\n'
ok "release id" "${RELEASE_ID}"
ok "release tag" "${RELEASE_TAG}"
ok "rehearsal tag" "${REHEARSAL_TAG}"
ok "images" "${IMAGE_REPOSITORY} (staging ${IMAGE_STAGING_REPOSITORY})"
ok "maintainer" "${MAINTAINER}"
ok "repository" "${SLUG}"

printf '\nsource\n'
if POSTVEC_RELEASE_TAG="${TAG}" "${PKG_DIR}/scripts/assert-versions.sh" >/dev/null 2>&1; then
    ok "version consistency" "crates, pins and tag agree"
else
    bad "version consistency" "run scripts/assert-versions.sh to see why"
fi

if [[ -n "$(git -C "${REPO_ROOT}" status --porcelain)" ]]; then
    if [[ "${MODE}" == rehearse ]]; then
        skip "working tree" "uncommitted changes (a rehearsal builds the pushed ref, not these)"
    else
        bad "working tree" "uncommitted changes — a release is cut from one committed tree"
    fi
else
    ok "working tree" "clean"
fi

missing=()
for workflow in postvec-release postvec-moving-tags postvec-packaging-ci; do
    [[ -f "${REPO_ROOT}/.github/workflows/${workflow}.yml" ]] || missing+=("${workflow}.yml")
done
if ((${#missing[@]})); then
    bad "workflow files" "missing: ${missing[*]}"
else
    ok "workflow files" "release, moving-tags, packaging-ci"
fi

printf '\ngithub\n'
GH_READY=0
if ! command -v gh >/dev/null 2>&1; then
    bad "gh" "not installed — see https://cli.github.com/"
elif ! gh auth status >/dev/null 2>&1; then
    bad "gh auth" "not authenticated — run: gh auth login"
else
    GH_VERSION="$(gh --version 2>/dev/null | awk 'NR==1 {print $3}')"
    # 2.49 is where `gh attestation` appears; a release nobody can verify is a
    # release nobody should trust.
    if [[ "$(printf '2.49.0\n%s\n' "${GH_VERSION}" | sort -V | head -1)" == "2.49.0" ]]; then
        ok "gh" "${GH_VERSION}"
    else
        bad "gh" "${GH_VERSION} is older than 2.49 (no \`gh attestation\`)"
    fi
    GH_READY=1
fi

if ((GH_READY)); then
    if gh workflow list --repo "${SLUG}" 2>/dev/null | grep -q postvec-release; then
        ok "workflow visible" "postvec-release on ${SLUG}"
    else
        bad "workflow visible" "gh cannot see postvec-release on ${SLUG}"
    fi

    # Reading environments needs repository access; a refusal is a skip, not a
    # pass, because an absent environment is exactly what this looks for.
    if rules="$(gh api "repos/${SLUG}/environments/postvec-release" \
                  --jq '[.protection_rules[].type] | join(",")' 2>/dev/null)"; then
        if [[ "${rules}" == *required_reviewers* ]]; then
            ok "postvec-release env" "required reviewers configured"
        else
            # Not fatal, and often not fixable: on Free/Pro/Team, required
            # reviewers and wait timers are public-repository-only. A private
            # repo on those plans gets the environment (deployment history,
            # environment secrets) with no protection rules at all. The
            # remaining gates — dispatch-only, the mode/tag gate, the
            # already-published check, immutable image tags — do not depend on
            # it, but the human pause does. Add reviewers when the repo goes
            # public, where the rule is free.
            skip "postvec-release env" "exists, no protection rules (${rules:-none}) — no approval pause"
            printf '        required reviewers are public-repo-only below Enterprise; see the release docs\n'
        fi
    else
        skip "postvec-release env" "could not read it — create it under Settings → Environments"
    fi
fi

if [[ -n "${TAG}" ]]; then
    printf '\ntag %s\n' "${TAG}"
    if git -C "${REPO_ROOT}" rev-parse --verify "refs/tags/${TAG}" >/dev/null 2>&1; then
        tagged="$(git -C "${REPO_ROOT}" rev-list -n 1 "refs/tags/${TAG}")"
        head_sha="$(git -C "${REPO_ROOT}" rev-parse HEAD)"
        if [[ "${tagged}" == "${head_sha}" ]]; then
            ok "points at HEAD" "${head_sha:0:12}"
        else
            bad "points at HEAD" "tag is ${tagged:0:12}, HEAD is ${head_sha:0:12}"
        fi
        if git -C "${REPO_ROOT}" ls-remote --exit-code --tags origin "${TAG}" >/dev/null 2>&1; then
            ok "pushed" "present on origin"
        else
            bad "pushed" "run: git push origin ${TAG}"
        fi
    else
        bad "exists" "run: git tag -a ${TAG} -m 'postvec ${RELEASE_ID}'"
    fi

    if ((GH_READY)); then
        if state="$(gh release view "${TAG}" --repo "${SLUG}" --json isDraft --jq .isDraft 2>&1)"; then
            if [[ "${state}" == true ]]; then
                ok "not published" "a draft exists; the next run replaces it"
            else
                bad "not published" "${TAG} is already published — bump PACKAGE_RELEASE"
            fi
        elif grep -qiE 'release not found|not found' <<<"${state}"; then
            ok "not published" "no release for this tag yet"
        else
            skip "not published" "could not ask GitHub: ${state}"
        fi
    fi
fi

# Versioned image tags are immutable. An existing one is only survivable when a
# rerun reproduces its digest exactly, so finding one here is a warning about
# which situation you are in, not a verdict.
if [[ "${MODE}" == publish ]]; then
    printf '\nimages\n'
    if command -v docker >/dev/null 2>&1; then
        existing=(); unknown=0
        for major in 16 17 18; do
            for suffix in "" "-complete"; do
                image="${IMAGE_REPOSITORY}:${RELEASE_ID}-pg${major}${suffix}"
                if output="$(docker buildx imagetools inspect "${image}" \
                              --format '{{.Manifest.Digest}}' 2>&1)"; then
                    existing+=("${RELEASE_ID}-pg${major}${suffix}")
                elif ! grep -qiE 'manifest unknown|not found|MANIFEST_UNKNOWN' <<<"${output}"; then
                    # `denied` is an authorization failure. Absence has to be
                    # observed, not inferred from a refusal to look.
                    unknown=1
                fi
            done
        done
        if ((unknown)); then
            skip "versioned tags" "could not read ${IMAGE_REPOSITORY} — docker login ghcr.io first"
        elif ((${#existing[@]})); then
            skip "versioned tags" "${#existing[@]} already exist (${existing[*]}) — this would be a resumption"
        else
            ok "versioned tags" "none of the six exist yet"
        fi
    else
        skip "versioned tags" "docker is not installed"
    fi
fi

printf '\n'
if ((fail)); then
    die "preflight failed — fix the items marked FAIL before dispatching"
fi

log "preflight passed — dispatch with:"
"${PKG_DIR}/scripts/dispatch-release.sh" --print-only "${MODE}"
