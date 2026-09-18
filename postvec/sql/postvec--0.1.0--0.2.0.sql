-- postvec 0.1.0 -> 0.2.0
--
-- Applied by `ALTER EXTENSION postvec UPDATE`. See README.md in this directory
-- for the rules every upgrade script follows.
--
-- No schema change yet: this script exists because the release gate
-- (packaging/postvec/scripts/assert-versions.sh) requires a path from every
-- released version to the one being built, and because the extension version
-- is what the background worker's version gate keys on. As 0.2.0 changes
-- schema.rs or a #[pg_extern] signature, the matching DDL goes below the lock.
-- postvec/upgrade_test.sh fails when a fresh 0.2.0 install and an upgraded
-- 0.1.0 disagree on any catalog object, so an omission here is caught.

-- First, before any DDL: the worker takes this lock in shared mode and then
-- asserts pg_extension.extversion matches its compiled-in version. Holding it
-- exclusively makes the upgrade wait for in-flight worker transactions, and
-- every transaction that starts afterwards sees the new version.
SELECT pg_advisory_xact_lock(hashtext('postvec_schema'));
