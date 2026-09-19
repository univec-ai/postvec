-- postvec 0.1.0 -> 0.2.0
--
-- Applied by `ALTER EXTENSION postvec UPDATE`. See README.md in this directory
-- for the rules upgrade scripts follow
--
-- No schema or SQL-function changes between 0.1.0 and 0.2.0 so this script
-- only takes the schema lock. It still has to exist - rel gate requires
-- a path from every released version
-- postvec/upgrade_test.sh compares a fresh 0.2.0 install with an upgraded
-- 0.1.0 and fails on any catalog difference.

-- Before any DDL. The worker takes this lock in shared mode and then
-- asserts pg_extension.extversion matches its compiled-in version. Holding it
-- exclusively waits for in-flight worker transactions; every transaction that
-- starts afterwards sees the new version.
SELECT pg_advisory_xact_lock(hashtext('postvec_schema'));
