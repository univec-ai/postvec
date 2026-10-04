-- postvec 0.3.0 -> 0.4.0. Applied by ALTER EXTENSION postvec UPDATE; see README.md.
-- Schema changes go below the lock. upgrade_test.sh compares the result with a fresh install.
SELECT pg_advisory_xact_lock(hashtext('postvec_schema'));
