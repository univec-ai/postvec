#!/usr/bin/env bash
# Runs once from /docker-entrypoint-initdb.d when the data directory is
# empty. Creating the extension is first-run convenience, not an upgrade.
#
# On an existing volume this does not run. Adopt with:
#     CREATE EXTENSION postvec CASCADE;
# and put that database in POSTVEC_DATABASES. This script never walks
# every database.
set -Eeuo pipefail

if [[ "${POSTVEC_CREATE_EXTENSION:-1}" != 1 ]]; then
    echo "postvec: POSTVEC_CREATE_EXTENSION=0 — leaving the database untouched"
    exit 0
fi

# The official entrypoint exports these and gives this script socket access to
# its temporary server. POSTVEC_DATABASES may name several databases, but only
# POSTGRES_DB has been created at this point; the others are the operator's to
# create, and their workers will pick the extension up as soon as it appears.
psql \
    --set ON_ERROR_STOP=1 \
    --no-psqlrc \
    --username "${POSTGRES_USER}" \
    --dbname "${POSTGRES_DB}" \
    <<'SQL'
CREATE EXTENSION IF NOT EXISTS postvec CASCADE;

DO $check$
DECLARE
    library  jsonb := postvec.build_info();
    catalog_version text;
BEGIN
    SELECT extversion INTO catalog_version
      FROM pg_extension WHERE extname = 'postvec';

    -- The image builds both halves from one commit, so a mismatch here means
    -- the image is corrupt (or was assembled from mixed artifacts). Failing
    -- initialisation is right: the worker's own version gate would park it
    -- anyway, and a half-initialised database is worse than none.
    IF library->>'version' IS DISTINCT FROM catalog_version THEN
        RAISE EXCEPTION
            'postvec library version % does not match the installed extension %',
            library->>'version', catalog_version;
    END IF;

    -- Normalised the way the extension normalises it: unset and empty both
    -- mean the default, which is `embedded`. The images always pass
    -- `-c postvec.mode=`, so this only matters for a hand-built image or a
    -- hand-run script — but reading an unset setting as "not embedded" would
    -- skip exactly the check that catches the build it applies to.
    IF COALESCE(NULLIF(current_setting('postvec.mode', true), ''), 'embedded') = 'embedded'
       AND NOT (library->'features'->>'embedded')::boolean THEN
        RAISE EXCEPTION
            'this image is configured for embedded mode but its postvec.so was '
            'built without embedded support (build_info: %)', library;
    END IF;

    RAISE NOTICE 'postvec % installed in database %',
        catalog_version, current_database();
END
$check$;
SQL

# Deliberately no refresh_models() here. In embedded mode the engine may still
# be loading, and in remote mode the inference host may not be reachable yet; making
# first initialisation depend on either would turn a recoverable, retried
# condition into a database that failed to initialise. The worker refreshes the
# cache on its own cadence and the healthcheck waits for the result.
