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
for manifest in core postvec postvec-cli engine shared providers registry/schema; do
    licence="$(grep -E '^license = ' "${REPO_ROOT}/${manifest}/Cargo.toml" | head -1 | cut -d'"' -f2)"
    check "${manifest} license" "PostgreSQL" "${licence}"
done

# postvec-server is the exception, and asserting it is the point: it is a
# separate program under a separate grant, sitting in a tree where every
# neighbour is PostgreSQL-licensed. A copy-paste from one of them would
# relicense it silently, and the LICENSING.md index would then be wrong
# about a file nobody re-read. All four statements of the fact have to
# agree: the reviewed pin (SERVER_LICENSE, which is what the postvec-server
# package declares), the crate manifest, the licence text, and the root
# index.
[[ -f "${REPO_ROOT}/postvec-server/LICENSE" ]] \
    || { echo "  FAIL  postvec-server/LICENSE is missing"; fail=1; }
check "postvec-server license" "${SERVER_LICENSE}" \
    "$(grep -E '^license = ' "${REPO_ROOT}/postvec-server/Cargo.toml" | head -1 | cut -d'"' -f2)"
while IFS= read -r -d '' source; do
    if ! grep -q '^// SPDX-License-Identifier: BUSL-1.1$' "${source}"; then
        printf '  FAIL  missing BUSL-1.1 header: %s\n' "${source}"
        fail=1
    fi
done < <(find "${REPO_ROOT}/postvec-server/src" -type f -name '*.rs' -print0)

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

# Upgrade scripts. Every older product version this project has released
# needs a path of postvec--FROM--TO.sql files to the version being built.
# Without one, an operator on that release parks the worker forever after
# `apt upgrade`. Newer tags (a 0.2.0 already in the repo while cutting a
# 0.1.1 hotfix) are ignored.
#
# The set of released versions is the set of postvec-v* tags. Packaging
# revisions collapse: 0.1.0-1 and 0.1.0-2 ship the same SQL.
check_upgrade_graph() {
    local released=() missing=() line versions unreachable
    # Command substitution so a git failure fails the gate.
    versions="$("${PKG_DIR}/scripts/previous-release.sh" --versions)"
    while IFS= read -r line; do
        [[ -n "${line}" ]] && released+=("${line}")
    done <<<"${versions}"
    if (( ${#released[@]} == 0 )); then
        # First release has no upgrade graph. The second one needs
        # postvec--<old>--<new>.sql and a native upgrade test; that is not
        # something to discover from users.
        printf '  skip  upgrade graph               no previous release tags (first release)\n'
        printf '        from the second release on, every older released version needs a path of\n'
        printf '        postvec--<old>--<new>.sql scripts, proven by postvec/upgrade_test.sh.\n'
        return
    fi

    unreachable="$(upgrade_graph_unreachable "${POSTVEC_VERSION}" \
        "${REPO_ROOT}/postvec/sql" "${released[@]}")"
    while IFS= read -r line; do
        [[ -n "${line}" ]] && missing+=("${line}")
    done <<<"${unreachable}"
    if (( ${#missing[@]} )); then
        printf '  FAIL  upgrade graph               no path to %s from: %s\n' \
            "${POSTVEC_VERSION}" "${missing[*]}"
        printf '        add the missing postvec--<old>--<new>.sql scripts in postvec/sql/\n'
        fail=1
    else
        printf '  ok    upgrade graph               all %d released version(s) reach %s\n' \
            "${#released[@]}" "${POSTVEC_VERSION}"
    fi

    # Freeze every upgrade script the previous release identity already
    # shipped. --identity includes a same-version packaging predecessor
    # (0.2.0-1 when building 0.2.0-2), which the product-version query skips.
    local previous changes rc=0
    previous="$("${PKG_DIR}/scripts/previous-release.sh" --identity)"
    [[ -n "${previous}" ]] || return 0
    changes="$(shipped_upgrade_scripts_changed "${REPO_ROOT}" "${previous}" postvec/sql)" || rc=$?
    if (( rc )); then
        printf '  FAIL  shipped upgrade scripts     git could not compare against %s\n' "${previous}"
        fail=1
    elif [[ -n "${changes}" ]]; then
        printf '  FAIL  shipped upgrade scripts     released in %s, altered since:\n' "${previous}"
        while IFS= read -r line; do printf '          %s\n' "${line}"; done <<<"${changes}"
        printf '        restore them (git checkout %s -- postvec/sql/); a fix to a\n' "${previous}"
        printf '        released upgrade belongs in a new postvec--<old>--<new>.sql\n'
        fail=1
    else
        printf '  ok    shipped upgrade scripts     unchanged since %s\n' "${previous}"
    fi
}
check_upgrade_graph

if (( fail )); then
    die "release version consistency failed"
fi
log "release ${POSTVEC_VERSION}-${PACKAGE_RELEASE} is internally consistent"
