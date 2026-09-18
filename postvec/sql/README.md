# Extension upgrade scripts

`cargo pgrx` generates the *install* script for the current version at package
time (`sql/postvec--<version>.sql`). Upgrade scripts are always written by
hand. pgrx has no automatic schema diff.

From the first released version:

1. Every released version `X` to `Y` ships `sql/postvec--X--Y.sql` in this
   directory. `cargo pgrx package` / `install` copies any `postvec--*.sql`
   found here into the extension share directory, so
   `ALTER EXTENSION postvec UPDATE` can find it.
2. Author the script from a schema diff: run `cargo pgrx schema` on both
   versions, then write the minimal `ALTER TABLE` /
   `CREATE OR REPLACE FUNCTION` statements. The four dumpable control tables
   (`registry`, `jobs`, `jobs_dead`, `migrations`) hold user data across
   dump/restore. Migrate their contents. Recreate them only when the upgrade
   also copies the rows.
3. Per-entry trigger functions (`postvec.trg_ins_<id>` and the rest) are
   generated at runtime. If an upgrade changes the trigger template, the
   script must regenerate them for every registry row (a `DO` block over
   `postvec.registry`).
4. Every upgrade script starts by taking the schema lock exclusively:

   ```sql
   SELECT pg_advisory_xact_lock(hashtext('postvec_schema'));
   ```

   The background worker takes the same lock in shared mode, then asserts
   that `pg_extension.extversion` equals its compiled-in version
   (`guard_schema_version` in `src/worker/mod.rs`). The exclusive lock
   serialises the two: the upgrade waits for in-flight worker transactions,
   and every transaction that starts afterwards sees the new version and
   parks. Take the lock first, before any DDL. Without it an upgrade can
   commit between a worker's version check and the work it protects, and an
   old worker will read and write rows in a schema that belongs to the new
   release.

5. `postvec/upgrade_test.sh` proves every upgrade before it ships. It builds
   the previous release from its tag (or any ref you name), populates it,
   swaps in the new library, and checks that:
   - the background worker parks across the version skew: no claims, no
     writes, no heartbeat;
   - `ALTER EXTENSION postvec UPDATE` keeps every row, compared inside the
     upgrade transaction;
   - the worker then resumes and drains a pending chunk refresh against the
     upgraded schema;
   - the upgraded catalog is identical to a fresh install's: members,
     relations, columns in order, defaults, indexes, constraints, policies,
     triggers, views, functions, types, sequences, ACLs, comments, config
     tables. This is the check that catches a script missing a change.

   It runs in `postvec-ci` (job `upgrade`) and gates the release
   (`upgrade-gate`, which `extension-packages` waits on). Both skip until a
   release of another version exists. The release's install tests also
   upgrade real packages from the previous published release
   (`packaging/postvec/tests/package-upgrade-test.sh`).

   Run it locally before tagging:

   ```console
   ./upgrade_test.sh                # from the newest postvec-v* tag
   ./upgrade_test.sh <git-ref>      # from any commit
   ```

   Not covered: a worker mid-transaction at the instant the upgrade starts.
   The exclusive schema lock serialises the two by design; the test proves
   the before and after, not every interleaving.

Changes to `schema.rs` and `#[pg_extern]` signatures were amended in place
until 0.1.0 was tagged. From then on every such change ships with the
matching `postvec--X--Y.sql`, and the upgrade test fails if the script and the
fresh install disagree. A release with no schema change still needs a script
(the schema lock alone): the release gate requires a path from every
released version.

`postvec.build_info()` and recursive chunking (the extra registry columns,
queue key, `trg_chunk_*` functions and related indexes) landed in the 0.1.0
base schema. After a release those would have been a maintenance-window
migration: writers quiesced, schema lock first, queue-index replacement,
per-entry trigger regeneration.
