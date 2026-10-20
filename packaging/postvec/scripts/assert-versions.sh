#!/usr/bin/env bash
# Refuse a release whose version disagrees across the crates, the pins, the
# control file and the git tag. A partial bump ships a CLI that will not
# talk to its own extension.

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

# cargo metadata, not a regex: a moved or duplicated key must not fool this.
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

# The control file must use @CARGO_VERSION@. A literal would win over the crate.
control="${REPO_ROOT}/postvec/postvec.control"
check "postvec.control default_version" "default_version = '@CARGO_VERSION@'" \
    "$(grep -E "^default_version" "${control}" | tr -s ' ')"

# Every crate linked into a shipped artifact must declare PostgreSQL:
# extension, CLI, engine, shared, providers and registry-schema.
[[ -f "${REPO_ROOT}/postvec/LICENSE" ]] || { echo "  FAIL  postvec/LICENSE is missing"; fail=1; }
for manifest in core postvec postvec-cli engine shared providers registry/schema; do
    licence="$(grep -E '^license = ' "${REPO_ROOT}/${manifest}/Cargo.toml" | head -1 | cut -d'"' -f2)"
    check "${manifest} license" "PostgreSQL" "${licence}"
done

# postvec-server is separately licensed. The pin, the crate, the licence
# text and the LICENSING.md index must agree, or a copy-paste relicenses it.
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

# LICENSING.md row: `| `postvec-server/` | <name> | ... |`. Map the name to its SPDX id.
index_licence="$(sed -nE 's#^\| `postvec-server/` \| ([^|]+) \|.*$#\1#p' "${REPO_ROOT}/LICENSING.md" | head -1 | sed -e 's/ *$//' -e 's/^Business Source License 1\.1$/BUSL-1.1/' -e 's/^Elastic License 2\.0$/Elastic-2.0/' -e 's/^Apache License 2\.0$/Apache-2.0/')"
check "LICENSING.md index (postvec-server)" "${SERVER_LICENSE}" "${index_licence:-<not listed in LICENSING.md>}"
# The node package carries POSTVEC_VERSION: it speaks this commit's wire contract.
check "postvec-server/Cargo.toml" "${POSTVEC_VERSION}" \
    "$(crate_version "${REPO_ROOT}/postvec-server/Cargo.toml" postvec-server)"

# A local run has no tag and skips this rather than inventing one.
# POSTVEC_RELEASE_TAG wins: workflow_dispatch sets GITHUB_REF_NAME to the
# workflow file's ref, which may not be the release.
tag="${POSTVEC_RELEASE_TAG:-${GITHUB_REF_NAME:-}}"
if [[ -n "${tag}" && "${tag}" == postvec-rehearsal-v* ]]; then
    # A different name, so a rehearsal cannot occupy the real release tag.
    check "rehearsal tag" "${REHEARSAL_TAG}" "${tag}"
elif [[ -n "${tag}" && "${tag}" == postvec-v* ]]; then
    # The packaging revision is in the tag (postvec-v0.1.0-1) so a rebuild cannot overwrite a published tag.
    check "release tag" "${RELEASE_TAG}" "${tag}"
elif [[ -n "${tag}" ]]; then
    printf '  skip  release tag                 %s (not a postvec release tag)\n' "${tag}"
else
    printf '  skip  release tag                 <not a tag build>\n'
fi

# Every older released product version needs postvec--FROM--TO.sql up to
# this one, or apt upgrade parks the worker. Packaging revisions share SQL.
# Newer tags are ignored. Released versions are the postvec-v* tags.
check_upgrade_graph() {
    local released=() missing=() line versions unreachable
    # Command substitution so a git failure fails the gate.
    versions="$("${PKG_DIR}/scripts/previous-release.sh" --versions)"
    while IFS= read -r line; do
        [[ -n "${line}" ]] && released+=("${line}")
    done <<<"${versions}"
    if (( ${#released[@]} == 0 )); then
        # The first release has no upgrade graph. The next one must ship the SQL path.
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

    # Freeze upgrade scripts the previous identity shipped. --identity includes a same-version packaging predecessor.
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
