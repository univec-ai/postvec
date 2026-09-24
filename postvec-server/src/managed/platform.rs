// SPDX-License-Identifier: BUSL-1.1

use sqlx::PgConnection;

/// The hosting platform, from roles every provider creates and any role can see.
pub async fn detect(connection: &mut PgConnection) -> Result<String, sqlx::Error> {
    sqlx::query_scalar(
        "SELECT CASE WHEN EXISTS (SELECT FROM pg_catalog.pg_proc WHERE proname = 'aurora_version'
                AND pronamespace = 'pg_catalog'::regnamespace) THEN 'aurora'
            ELSE coalesce((SELECT p FROM (VALUES (1, 'rds_superuser', 'rds'), (2, 'alloydbsuperuser', 'alloydb'),
                (3, 'cloudsqlsuperuser', 'cloudsql'), (4, 'azure_pg_admin', 'azure'), (5, 'supabase_admin', 'supabase'),
                (6, 'neon_superuser', 'neon')) v(o, r, p)
                WHERE EXISTS (SELECT FROM pg_catalog.pg_roles WHERE rolname = r) ORDER BY o LIMIT 1), 'postgresql') END",
    )
    .fetch_one(connection)
    .await
}
