# Extension upgrade scripts (plan R10 / task 2.7)

cargo-pgrx generates the *install* script for the current version at package
time (`sql/postvec--<version>.sql`), but **upgrade scripts are always
hand-written** — pgrx has no automatic diffing.

Convention, effective from the first released version:

1. Every released version `X` → `Y` ships `sql/postvec--X--Y.sql` in this
   directory. `cargo pgrx package`/`install` copies any `postvec--*.sql`
   found here into the extension's share directory, so
   `ALTER EXTENSION postvec UPDATE;` works.
2. Author them from the schema diff: run `cargo pgrx schema` on both
   versions and diff the output, then write the minimal `ALTER TABLE` /
   `CREATE OR REPLACE FUNCTION` statements. Remember the four dumpable
   control tables (`registry`, `jobs`, `jobs_dead`, `migrations`) carry user
   data across dump/restore — upgrades must migrate their contents, never
   drop/recreate them.
3. Per-entry trigger functions (`postvec.trg_ins_<id>` etc.) are *generated*
   objects owned by the extension's runtime, not by the install script. If an
   upgrade changes the trigger template, the upgrade script must regenerate
   them for every registry row (a `DO` block looping over
   `postvec.registry`).
4. **Every upgrade script must begin by taking the schema lock exclusively:**

   ```sql
   SELECT pg_advisory_xact_lock(hashtext('postvec_schema'));
   ```

   This is the other half of a protocol the background worker relies on. Each
   worker transaction takes the same lock in *shared* mode and then asserts
   that `pg_extension.extversion` equals its own compiled-in version
   (`guard_schema_version` in `src/worker/mod.rs`). Without the exclusive lock
   here, an upgrade can commit in the window between a worker's version check
   and the work it protects, and an old worker will then read and write rows in
   a schema that belongs to the new release.

   With it, the two serialise: the upgrade waits for in-flight worker
   transactions to finish, and every transaction that starts afterwards sees
   the new version and parks. Take it *first*, before any DDL.

5. CI exercises upgrades once a prior release exists: install version X from
   the previous tag, `ALTER EXTENSION postvec UPDATE`, then run the test
   suite against the upgraded schema. The upgrade test must include a
   **concurrent** case: a worker draining a real backlog while the upgrade
   runs, proving no job is processed against the wrong schema.

No released version exists yet (0.1.0 is unreleased), so this directory holds
no upgrade scripts — only the convention.

> Note for the first release: `postvec.build_info()` was added to 0.1.0 before
> it shipped, so it needs no upgrade script. Were it added *after* a release,
> it would need one — `postvec-cli` treats its absence as "this extension is
> too old to prove its build features" and refuses embedded-mode setup on that
> basis.
>
> P5 (recursive chunking, 2026-08-13) likewise amended the 0.1.0 base schema
> in place — seven registry columns + the whole-shape CHECK, the four-column
> queue dedup key and the reworked queue/dead indexes, `jobs`/`jobs_dead`
> `chunk_id`, the widened `op`, the heartbeat counters, the recursive
> `trg_truncate` body, and the shared `trg_chunk_*` trigger functions — with
> no upgrade script. If a release had existed first, that change would have
> been a full maintenance-window migration (writers quiesced, schema lock
> first, queue-index replacement, per-entry trigger regeneration with the new
> `ON CONFLICT` key).
