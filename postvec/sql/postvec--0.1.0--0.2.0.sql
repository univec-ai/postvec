-- postvec 0.1.0 -> 0.2.0
--
-- Applied by `ALTER EXTENSION postvec UPDATE`. See README.md in this directory
-- for the rules every upgrade script follows.
--
-- Schema lock only for now. The release gate requires a path from every
-- released version to this one, and the worker keys on extversion. As 0.2.0
-- changes schema.rs or a #[pg_extern] signature, the matching DDL goes below
-- the lock. postvec/upgrade_test.sh compares a fresh 0.2.0 install with an
-- upgraded 0.1.0 and fails on any catalog difference.

-- First, before any DDL. The worker takes this lock in shared mode and then
-- asserts pg_extension.extversion matches its compiled-in version. Holding it
-- exclusively waits for in-flight worker transactions; every transaction that
-- starts afterwards sees the new version.
SELECT pg_advisory_xact_lock(hashtext('postvec_schema'));
