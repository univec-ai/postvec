# Contributing to postvec

Thanks for considering a contribution. postvec is in public beta; small,
well-tested changes land fastest.

## Before you open a PR

Run the same gates CI runs:

```console
cd postvec && ./ci.sh        # fmt + clippy -D warnings + the pgrx suites
cargo test -p postvec-cli --all-targets
cargo test -p postvec-providers --features wire,test-util --all-targets
cargo test -p postvec-server --all-targets
cargo test -p registry-schema --features archive
# After 0.1.0 is tagged, also: ./postvec/upgrade_test.sh
```

The extension suites need a `cargo pgrx` PG 18 dev cluster (`cargo pgrx init`);
CI covers PG 16/17 and the packaging matrix, so don't block on running those
locally.

## Sign-off

Every commit must carry a `Signed-off-by:` line (`git commit -s`), certifying
the [Developer Certificate of Origin](https://developercertificate.org/): you
wrote the change or otherwise have the right to submit it under the project's
licenses.

## Rules that are easy to trip over

- **Generated SQL / upgrade scripts.** Once 0.1.0 is published, any change to
  `postvec/src/schema.rs` or a `#[pg_extern]` signature needs a matching
  `postvec--<old>--<new>.sql` upgrade script following
  `postvec/sql/README.md` — exact generated DDL, obsolete overloads dropped,
  per-entry triggers regenerated. Before the first release the convention was
  amend-in-place; that ends at the tag.
- **The engine fork (`engine/`, `shared/`).** A trimmed fork of UniVec's
  internal inference engine, changed by *deletion only*: a diff against
  upstream must show absences, not differences (see `engine/FORK.md`). If
  your change would rewrite inference logic there, stop and open an issue
  first. Anything touching a tokenizer or pooling path is a
  numbers-changing risk — run
  `packaging/postvec/tests/model-golden-test.sh` before trusting it.
- **Licensing.** The tree is PostgreSQL-licensed except `postvec-server/`,
  which is under the Business Source License 1.1 (see [LICENSING.md](LICENSING.md)).
  Keep new files' SPDX headers and `Cargo.toml` `license` fields consistent
  with their crate, and never copy code across that boundary in either
  direction without carrying its license. Pull requests that touch
  `postvec-server/` need a signed [CLA](CLA.md) on file (email
  legal@univec.ai, wait for confirmation). The DCO covers everything else.
- **The vendored proto** (`postvec/proto/ninference.proto`) must stay
  byte-identical to `proto/ninference.proto`; it is a live wire contract
  with services outside this repository. A `#[pg_test]` pins it.
- **Worker code never `error!`s on per-entry problems** — quarantine or
  dead-letter instead; an unhandled error is a crash-respawn loop in
  someone's cluster.

## Tests

A behavioral change ships with a test that fails without it. Cross-session
lock/race contracts go in `postvec/src/tests_concurrency.rs` and must be able
to fail when the lock is removed. Provider wire behavior is tested against
the in-process mock (`providers/src/testing.rs`) — never against a live,
billable endpoint.

## Security issues

Never as a public issue or PR — see [SECURITY.md](./SECURITY.md).
