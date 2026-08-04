// SPDX-License-Identifier: BUSL-1.1

use sqlx::PgConnection;

pub async fn detect(connection: &mut PgConnection) -> Result<String, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT CASE
            WHEN version() ILIKE '%aurora%' OR EXISTS (
                SELECT FROM pg_catalog.pg_proc WHERE proname = 'aurora_version') THEN 'aurora'
            WHEN current_setting('rds.extensions', true) IS NOT NULL THEN 'rds'
            WHEN EXISTS (SELECT FROM pg_catalog.pg_settings WHERE name LIKE 'cloudsql.%') THEN 'cloudsql'
            WHEN current_setting('azure.extensions', true) IS NOT NULL THEN 'azure'
            WHEN version() ILIKE '%neon%' THEN 'neon'
            ELSE 'postgresql' END::text",
    )
    .fetch_one(connection)
    .await
}
