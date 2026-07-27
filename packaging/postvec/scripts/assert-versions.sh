#!/usr/bin/env bash
# The release gate that runs before anything is compiled.
#
# One version number identifies a postvec release: it appears in the extension
# crate, the CLI crate, the reviewed pins, the extension control file and the
# git tag. Any disagreement between those is a mis-release — usually a partially
# bumped version — and it is far cheaper to catch here than in a published
# artifact whose CLI refuses to talk to its own extension.

source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

load_versions

fail=0
check() {
    local what="$1" expected="$2" actual="$3"
    if [[ "${expected}" == "${actual}" ]]; then
        printf '  ok    %-28s %s\n' "${what}" "${actual}"
    else
        printf '  FAIL  %-28s %s (expected %s)\n' "${what}" "${actual:-<missing>}" "${expected}"
        fail=1
    fi
}

# Read `version` from a crate manifest via cargo, not a regex: `cargo metadata`
# is the parser Cargo itself uses, so a moved or duplicated key cannot fool it.
crate_version() {
    local manifest="$1" name="$2"
    need cargo jq
    cargo metadata --format-version 1 --no-deps --manifest-path "${manifest}" \
        | jq -r --arg name "${name}" '.packages[] | select(.name == $name) | .version'
}

log "postvec release version consistency"

check "postvec/Cargo.toml" "${POSTVEC_VERSION}" \
    "$(crate_version "${REPO_ROOT}/postvec/Cargo.toml" postvec)"
check "postvec-cli/Cargo.toml" "${POSTVEC_VERSION}" \
    "$(crate_version "${REPO_ROOT}/postvec-cli/Cargo.toml" postvec-cli)"

# The control file must defer to the crate version rather than carrying its
# own: cargo-pgrx substitutes @CARGO_VERSION@ at package time, and a literal
# version here would silently win over the crate's.
control="${REPO_ROOT}/postvec/postvec.control"
check "postvec.control default_version" "default_version = '@CARGO_VERSION@'" \
    "$(grep -E "^default_version" "${control}" | tr -s ' ')"

# The licence the packages ship, and the metadata that names it. Every crate
# that is compiled into a shipped artifact must declare it: the extension and
# CLI (headline), the forked engine and shared (linked into postvec.so in
# embedded mode), providers (the gateway, linked into postvec.so and
# postvec-server), and registry-schema (linked into the CLI).
[[ -f "${REPO_ROOT}/postvec/LICENSE" ]] || { echo "  FAIL  postvec/LICENSE is missing"; fail=1; }
for manifest in postvec postvec-cli engine shared providers registry/schema; do
    licence="$(grep -E '^license = ' "${REPO_ROOT}/${manifest}/Cargo.toml" | head -1 | cut -d'"' -f2)"
    check "${manifest} license" "PostgreSQL" "${licence}"
done

# postvec-server is the exception, and asserting it is the point: it is a
# separate program under a separate grant, sitting in a tree where every
# neighbour is PostgreSQL-licensed. A copy-paste from one of them would
# relicense it silently, and the LICENSING.md index would then be wrong about
# a file nobody re-read. All four statements of the fact have to agree: the
# reviewed pin (SERVER_LICENSE, which is what the postvec-server *package*
# declares), the crate manifest, the licence text, and the root index.
[[ -f "${REPO_ROOT}/postvec-server/LICENSE" ]] \
    || { echo "  FAIL  postvec-server/LICENSE is missing"; fail=1; }
check "postvec-server license" "${SERVER_LICENSE}" \
    "$(grep -E '^license = ' "${REPO_ROOT}/postvec-server/Cargo.toml" | head -1 | cut -d'"' -f2)"
# The index row in LICENSING.md is `| `postvec-server/` | <name> | ... |`; the
# name is mapped to its SPDX id and compared literally, dots and all.
index_licence="$(sed -nE 's#^\| `postvec-server/` \| ([^|]+) \|.*$#\1#p' "${REPO_ROOT}/LICENSING.md" | head -1 | sed -e 's/ *$//' -e 's/^Business Source License 1\.1$/BUSL-1.1/' -e 's/^Elastic License 2\.0$/Elastic-2.0/' -e 's/^Apache License 2\.0$/Apache-2.0/')"
check "LICENSING.md index (postvec-server)" "${SERVER_LICENSE}" "${index_licence:-<not listed in LICENSING.md>}"
# And the version: the node speaks exactly the wire contract this commit's
# extension was compiled against, and its package carries POSTVEC_VERSION.
check "postvec-server/Cargo.toml" "${POSTVEC_VERSION}" \
    "$(crate_version "${REPO_ROOT}/postvec-server/Cargo.toml" postvec-server)"

# Tag discipline, when running in a tag build. A local run has no tag and skips
# this check rather than inventing one.
#
# POSTVEC_RELEASE_TAG wins over GITHUB_REF_NAME, not the other way round.
# `workflow_dispatch` sets GITHUB_REF_NAME to the branch or tag the *workflow
# file* was selected from, which is not necessarily what is being released — so
# the caller's explicit statement of the release tag is the authority, and the
# ambient one is only a fallback for a plain tag build.
tag="${POSTVEC_RELEASE_TAG:-${GITHUB_REF_NAME:-}}"
if [[ -n "${tag}" && "${tag}" == postvec-rehearsal-v* ]]; then
    # The disposable-publication namespace. Same identity, different name, so a
    # rehearsal can never occupy the release the real publication needs.
    check "rehearsal tag" "${REHEARSAL_TAG}" "${tag}"
elif [[ -n "${tag}" && "${tag}" == postvec-v* ]]; then
    # The packaging revision is part of the tag: postvec-v0.1.0-1. A rebuild
    # that only changes packaging is a distinct release with distinct
    # artifacts, and giving it a distinct name is what keeps published tags
    # immutable.
    check "release tag" "${RELEASE_TAG}" "${tag}"
elif [[ -n "${tag}" ]]; then
    printf '  skip  release tag                 %s (not a postvec release tag)\n' "${tag}"
else
    printf '  skip  release tag                 <not a tag build>\n'
fi

# Upgrade scripts. It is not enough that *some* script ends at this version:
# every version this project has released must be able to reach it, or an
# operator upgrading from an older release lands on a cluster whose worker
# parks forever on the version gate.
#
# The set of released versions is the set of postvec-v* tags, which is the only
# record of what was actually published.
check_upgrade_graph() {
    local released=() reachable=() version
    # Tags are postvec-v<semver>-<packaging revision>. The *product* version is
    # what an upgrade script names, so the revision is stripped and duplicates
    # collapse: 0.1.0-1 and 0.1.0-2 ship the same SQL.
    mapfile -t released < <(
        git -C "${REPO_ROOT}" tag --list 'postvec-v*' 2>/dev/null \
            | sed -nE 's/^postvec-v([0-9]+\.[0-9]+\.[0-9]+)(-[0-9]+)?$/\1/p' \
            | grep -vx "${POSTVEC_VERSION}" | sort -Vu
    )
    if (( ${#released[@]} == 0 )); then
        # First release has no upgrade graph. The second one needs
        # postvec--<old>--<new>.sql and a native upgrade test; that is not
        # something to discover from users.
        printf '  skip  upgrade graph               no previous release tags (first release)\n'
        printf '        the second release needs postvec--<old>--<new>.sql *and* a native\n'
        printf '        upgrade test: install old, populate, upgrade, restart, ALTER EXTENSION.\n'
        return
    fi

    # Which versions can reach this one, following the scripts transitively:
    # postvec--A--B.sql plus postvec--B--C.sql makes A reachable.
    local -A edges=()
    shopt -s nullglob
    local script from to
    for script in "${REPO_ROOT}"/postvec/sql/postvec--*--*.sql; do
        script="$(basename "${script}" .sql)"
        script="${script#postvec--}"
        from="${script%%--*}"
        to="${script##*--}"
        edges["${from}"]+="${to} "
    done
    shopt -u nullglob

    reachable=("${POSTVEC_VERSION}")
    local changed=1
    while (( changed )); do
        changed=0
        for from in "${!edges[@]}"; do
            [[ " ${reachable[*]} " == *" ${from} "* ]] && continue
            for to in ${edges[${from}]}; do
                if [[ " ${reachable[*]} " == *" ${to} "* ]]; then
                    reachable+=("${from}")
                    changed=1
                    break
                fi
            done
        done
    done

    local missing=()
    for version in "${released[@]}"; do
        [[ " ${reachable[*]} " == *" ${version} "* ]] || missing+=("${version}")
    done
    if (( ${#missing[@]} )); then
        printf '  FAIL  upgrade graph               no path to %s from: %s\n' \
            "${POSTVEC_VERSION}" "${missing[*]}"
        printf '        add the missing postvec--<old>--<new>.sql scripts in postvec/sql/\n'
        fail=1
    else
        printf '  ok    upgrade graph               all %d released version(s) reach %s\n' \
            "${#released[@]}" "${POSTVEC_VERSION}"
    fi
}
check_upgrade_graph

if (( fail )); then
    die "release version consistency failed"
fi
log "release ${POSTVEC_VERSION}-${PACKAGE_RELEASE} is internally consistent"
