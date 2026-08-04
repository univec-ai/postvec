#!/usr/bin/env bash
# postvec CI gate. Run from anywhere; operates on this crate.
#
# Runs the same checks CI runs:
#   1. rustfmt        — formatting
#   2. clippy         — lints as errors (thin build, then the embedded build)
#   3. cargo pgrx test — unit + #[pg_test] (includes the proto-drift check and
#                        the full trigger/queue/enable/search/status suites).
#                        No inference engine needed: tests use fixtures + a mock client.
#   3b. the same, --features pg_test_concurrency, filtered to the cross-session
#                        suite: real second sessions proving lock/waiter
#                        behaviour, in their own cluster (see src/tests_concurrency.rs).
#   4. the same test suite with --features embedded — compiles the in-process
#      engine and adds the embedded marshalling/error-taxonomy/loopback-server
#      tests. No model assets needed. Skip with POSTVEC_SKIP_EMBEDDED=1 when
#      iterating (the engine tree is a heavy compile).
#
# Prerequisites: cargo-pgrx 0.18.1, a
# pgrx-managed PG 18 (`cargo pgrx init --pg18`), and pgvector built into it.
set -euo pipefail
cd "$(dirname "$0")"

PG="${POSTVEC_PG:-pg18}"

echo "== rustfmt =="
cargo fmt --check

echo "== clippy ($PG) =="
# --all-targets + pg_test so test code is linted too (about half the crate).
cargo clippy --all-targets --no-default-features --features "$PG,pg_test" -- -D warnings

echo "== cargo pgrx test ($PG) =="
cargo pgrx test "$PG"

# The cross-session concurrency suite runs as its OWN invocation: it must
# COMMIT fixtures for other sessions to see, while the suite above runs its
# tests in parallel against one shared database and asserts on global counts.
# pgrx builds a fresh cluster per invocation, so a separate run is complete
# isolation. Proves lock/waiter behaviour that a single-transaction test
# cannot (retry_dead serialization, set_format's refresh boundary, DML
# racing an index build, and the auto-index lifecycle serialization).
echo "== cargo pgrx test ($PG, cross-session concurrency) =="
cargo pgrx test "$PG" --features pg_test_concurrency tests_concurrency

if [[ "${POSTVEC_SKIP_EMBEDDED:-0}" != "1" ]]; then
  echo "== clippy ($PG, embedded) =="
  cargo clippy --all-targets --no-default-features --features "$PG,pg_test,embedded" -- -D warnings

  echo "== cargo pgrx test ($PG, embedded) =="
  cargo pgrx test "$PG" --features embedded
fi

echo "== dump/restore smoke (P5) =="
# Relies on the extension the test steps above installed into the pgrx dir.
POSTVEC_TEST_MANAGED=1 ./dump_restore_smoke.sh

echo "== postvec CI gate passed =="
