#!/usr/bin/env bash
# Run `cargo audit` as a release gate over both dependency trees.
#
#   scripts/audit.sh
#
# Blocking, not advisory: a release must not publish a binary with a known
# vulnerability just because nobody read the log. Exceptions are declared in
# audit-exceptions.toml with a reason and an expiry, and an expired exception
# fails exactly as the advisory would.
#
# Both trees, because they are genuinely separate lockfiles: the workspace
# (which builds the CLI) and postvec's own (which builds the extension,
# including the whole engine stack in the embedded build).

source "$(dirname "${BASH_SOURCE[0]}")/lib.sh"

load_versions
need cargo python3

# `cargo audit` is a separate binary, and `cargo <missing-subcommand>` exits
# non-zero exactly like an audit that found something. Distinguishing them
# matters: one is a release blocker, the other is an unprovisioned machine, and
# reporting the second as the first sends someone hunting for a vulnerability
# that was never reported.
cargo audit --version >/dev/null 2>&1 || die "cargo-audit is not installed.
Install it with: cargo install cargo-audit --locked
The release workflow provisions it; this is a local prerequisite, not a finding."

EXCEPTIONS="${PKG_DIR}/audit-exceptions.toml"

# Parse the exceptions first, so an expired one fails before a long scan.
#
# Command substitution, not `mapfile < <(…)`: a process substitution's exit
# status is not the shell's, so an expired exception would be reported and then
# silently ignored.
if ! accepted="$(python3 - "${EXCEPTIONS}" <<'PY'
import datetime, re, sys

# Comments first: the file documents its own format with a commented example,
# and an example must never be mistaken for a live exception.
text = "\n".join(
    line for line in open(sys.argv[1]).read().splitlines()
    if not line.lstrip().startswith("#")
)
today = datetime.date.today()
problems, ignores = [], []
for block in re.findall(r"\[\[accepted\]\](.*?)(?=\n\[\[|\Z)", text, re.S):
    fields = dict(re.findall(r'^\s*(\w+)\s*=\s*"([^"]*)"', block, re.M))
    missing = [k for k in ("id", "reason", "decided_by", "review_by") if not fields.get(k)]
    if missing:
        problems.append("an exception is missing: %s" % ", ".join(missing))
        continue
    try:
        review_by = datetime.date.fromisoformat(fields["review_by"])
    except ValueError:
        problems.append("%s: review_by is not a date" % fields["id"])
        continue
    if review_by < today:
        problems.append(
            "%s: the exception expired on %s and must be re-reviewed"
            % (fields["id"], fields["review_by"]))
        continue
    ignores.append(fields["id"])

if problems:
    for problem in problems:
        print("audit exception: %s" % problem, file=sys.stderr)
    sys.exit(1)
print("\n".join(ignores))
PY
)"; then
    die "the audit exceptions file is not usable"
fi
mapfile -t IGNORES <<<"${accepted}"

ARGS=(--deny warnings)
for advisory in "${IGNORES[@]}"; do
    [[ -n "${advisory}" ]] || continue
    warn "accepting ${advisory} (see audit-exceptions.toml)"
    ARGS+=(--ignore "${advisory}")
done

fail=0
unreadable=0
for manifest in "${REPO_ROOT}/Cargo.lock" "${REPO_ROOT}/postvec/Cargo.lock"; do
    log "cargo audit: ${manifest#"${REPO_ROOT}/"}"
    # Tee, so the report still reaches the log while the outcome is inspected:
    # "the database would not load" and "the database says you are vulnerable"
    # both exit non-zero, and reporting the first as the second sends someone
    # hunting for a vulnerability that was never reported. This is the same
    # distinction the `cargo audit --version` probe above makes for a missing
    # binary — an unprovisioned tool is not a finding.
    output="$(cargo audit "${ARGS[@]}" --file "${manifest}" 2>&1)" || fail=1
    printf '%s\n' "${output}"
    if grep -qE 'error loading advisory database|error fetching advisory database' <<<"${output}"; then
        unreadable=1
    fi
done

if (( unreadable )); then
    die "the advisory database could not be loaded, so nothing was audited.
This is not a finding about postvec's dependencies.
A parse error usually means the pinned cargo-audit predates a schema the
database now uses — for example a CVSS v4 vector, which every release before
cargo-audit 0.22 rejects. Raise the pin in .github/workflows/postvec-release.yml
(and locally), then run this again."
fi

if (( fail )); then
    die "cargo audit found advisories.
Fix them, or add a reviewed, dated exception to packaging/postvec/audit-exceptions.toml."
fi
log "no known advisories in either dependency tree"
