#!/usr/bin/env bash
# Point moving image tags at recorded digests, and report what actually landed.
#
#   advance-moving-tags.sh [--recovery-hint TEXT] <plan-file>
#
# The plan is whitespace-separated, one line per tag:
#
#   <moving tag>  <immutable source>  <expected digest>  [anything else]
#
# The source must be `<repository>@sha256:…`. A tag would be a mutable pointer,
# and the whole reason a plan exists is that time passes between deciding what
# to write and writing it — an environment approval, a queue behind another
# publication. A digest cannot move; a tag can, and the one thing worse than a
# stale plan is a stale plan that still looks applied.
#
# ---------------------------------------------------------------------------
#
# **Why this is a script, and why it does not stop on the first failure.**
#
# Both the release workflow and the recovery workflow advance the same six
# names, and both ran this as an inline loop under `set -e`. That is exactly
# wrong for the job: a registry write that fails aborts the loop, so the
# remaining tags are never attempted, the verification pass never runs, and the
# recovery guidance never prints. The operator is left with a red step, some
# unknown number of tags advanced, and nothing telling them which.
#
# So: every registry operation here is individually non-fatal. All entries are
# attempted, then *all* entries are inspected, and the exit status is decided by
# what the registry actually holds at the end — not by which command happened to
# fail first. A write that fails but whose tag already points at the right digest
# is not a failure; a write that "succeeds" and leaves the wrong digest is.
#
# Idempotent by construction: pointing a tag at the digest it already has is a
# no-op, so every entry is applied every time rather than compared against a
# `current` value captured before the wait.

set -Eeuo pipefail

RECOVERY_HINT="Run the postvec-moving-tags workflow to finish the job; it is
idempotent, and it verifies every tag rather than assuming the writes took."
PLAN=""

while (($#)); do
    case "$1" in
    --recovery-hint) RECOVERY_HINT="$2"; shift 2 ;;
    -h|--help) sed -n '2,20p' "$0"; exit 0 ;;
    -*) printf 'unknown argument: %s\n' "$1" >&2; exit 2 ;;
    *) PLAN="$1"; shift ;;
    esac
done

[[ -n "${PLAN}" && -f "${PLAN}" ]] || { echo "usage: advance-moving-tags.sh <plan-file>" >&2; exit 2; }

# The registry client, overridable so the failure paths can be tested without a
# registry. Split into an array because it is a command *and* its subcommand
# prefix ("docker buildx imagetools"), not a single word.
read -r -a IMAGETOOLS <<<"${POSTVEC_IMAGETOOLS:-docker buildx imagetools}"

say()  { printf '%s\n' "$*" >&2; }
# `::error::` is a GitHub annotation and harmless anywhere else.
err()  { printf '::error::%s\n' "$*" >&2; }

digest_of() { "${IMAGETOOLS[@]}" inspect "$1" --format '{{.Manifest.Digest}}' 2>/dev/null; }

# ------------------------------------------------------------------- the plan

# `<repository>@sha256:<64 lowercase hex>`, anchored at both ends. A substring
# test for `@sha256:` is not this check: it accepts `repo@sha256:not-a-digest`
# and `@sha256:` alike, and "it contains the word digest" is not "it is pinned".
SOURCE_SHAPE='^([^@[:space:]]+)@(sha256:[0-9a-f]{64})$'
DIGEST_SHAPE='^sha256:[0-9a-f]{64}$'

declare -a MOVING=() SOURCE=() EXPECTED=()
while read -r moving source expected _rest; do
    [[ -n "${moving}" ]] || continue

    # Every one of these is checked before *any* write, because the contract
    # this helper offers is that a malformed plan changes nothing. Discovering a
    # bad entry at verification time would mean the registry had already been
    # written — which is precisely the state a moving tag must never be left in.
    if [[ ! "${source}" =~ ${SOURCE_SHAPE} ]]; then
        err "plan entry for ${moving} is not pinned: ${source}"
        err "A moving tag may only be pointed at <repository>@sha256:<64 lowercase hex>."
        exit 1
    fi
    source_repository="${BASH_REMATCH[1]}"
    source_digest="${BASH_REMATCH[2]}"

    if [[ ! "${expected}" =~ ${DIGEST_SHAPE} ]]; then
        err "plan entry for ${moving} has a malformed expected digest: ${expected}"
        exit 1
    fi

    # The two halves of a plan entry are "write this" and "then it should be
    # this". If they disagree, one of them is wrong and there is no way to tell
    # which — so writing the source and *then* reporting the mismatch would
    # leave a moving tag pointing somewhere nobody chose. Refuse instead.
    if [[ "${source_digest}" != "${expected}" ]]; then
        err "plan entry for ${moving} contradicts itself:"
        err "  source   ${source_repository}@${source_digest}"
        err "  expected ${expected}"
        err "Writing the source would leave the tag at a digest the plan does not expect."
        exit 1
    fi

    MOVING+=("${moving}"); SOURCE+=("${source}"); EXPECTED+=("${expected}")
done < "${PLAN}"

(( ${#MOVING[@]} )) || { err "the plan is empty"; exit 1; }
say "advancing ${#MOVING[@]} moving tag(s)"

# ------------------------------------------------------------------ the writes
#
# Every one attempted, whatever the previous one did.

write_failures=0
for i in "${!MOVING[@]}"; do
    say "  ${MOVING[i]} -> ${EXPECTED[i]}"
    if ! "${IMAGETOOLS[@]}" create --tag "${MOVING[i]}" "${SOURCE[i]}"; then
        # Noted, not fatal. Whether it *matters* is a question about the
        # registry's final state, which the verification pass below asks.
        err "could not write ${MOVING[i]}"
        write_failures=$((write_failures + 1))
    fi
done
(( write_failures )) && say "${write_failures} write(s) reported a failure; verifying all tags anyway"

# ------------------------------------------------------------- what landed
#
# All of them, including the ones whose write failed: a tag that already pointed
# at the right digest is correct regardless of what the write said, and a tag
# whose write claimed success is not correct until the registry agrees.

wrong=0
for i in "${!MOVING[@]}"; do
    now="$(digest_of "${MOVING[i]}")" || now=""
    if [[ -z "${now}" ]]; then
        err "${MOVING[i]} could not be inspected; its state is unknown"
        wrong=$((wrong + 1))
    elif [[ "${now}" != "${EXPECTED[i]}" ]]; then
        err "${MOVING[i]} is ${now}, expected ${EXPECTED[i]}"
        wrong=$((wrong + 1))
    else
        say "  ok ${MOVING[i]} -> ${EXPECTED[i]}"
    fi
done

if (( wrong )); then
    err "${wrong} of ${#MOVING[@]} moving tag(s) are not where they should be."
    while IFS= read -r line; do err "${line}"; done <<<"${RECOVERY_HINT}"
    exit 1
fi

say "all ${#MOVING[@]} moving tag(s) verified"
