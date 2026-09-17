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

5. Once a prior release exists, CI installs version X from the previous tag,
   runs `ALTER EXTENSION postvec UPDATE`, then runs the test suite against
   the upgraded schema. Include a concurrent case: a worker draining a real
   backlog while the upgrade runs, so no job is processed against the wrong
   schema.

0.1.0 is unreleased, so this directory holds the convention and no upgrade
scripts yet. Changes to `schema.rs` and `#[pg_extern]` signatures are still
amended in place. After the first tag the same change needs a full
`postvec--X--Y.sql`.

`postvec.build_info()` and recursive chunking (the extra registry columns,
queue key, `trg_chunk_*` functions and related indexes) landed in the 0.1.0
base schema. After a release those would have been a maintenance-window
migration: writers quiesced, schema lock first, queue-index replacement,
per-entry trigger regeneration.
