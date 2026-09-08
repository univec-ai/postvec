-- SPDX-License-Identifier: PostgreSQL
-- Routes serve spaces. `postvec.routes` is the per-route view of the model
-- cache; `postvec._route(model, space)` is the one resolution order every
-- caller (extension, plpgsql mirror, worker) uses: the exact route named,
-- else the routes of that space by priority, else the entry's remembered
-- space (a vanished route), ties on route name.
CREATE OR REPLACE VIEW postvec.routes AS
SELECT COALESCE(target_model, name) AS space,
       name AS route,
       raw->'extra'->>'provider' AS provider,
       COALESCE('provider ' || (raw->'extra'->>'provider'), 'local') AS execution,
       target_dim AS dim,
       priority,
       COALESCE(raw->'extra'->'priority_explicit' = 'true'::jsonb, false) AS explicit,
       row_number() OVER (PARTITION BY COALESCE(target_model, name) ORDER BY priority, name) = 1
           AS preferred,
       last_seen
  FROM (SELECT *,
               CASE WHEN jsonb_typeof(raw->'extra'->'priority') = 'number'
                         AND raw->'extra'->>'priority' ~ '^[0-9]{1,5}$'
                         THEN CASE WHEN (raw->'extra'->>'priority')::int BETWEEN 1 AND 65535
                              THEN (raw->'extra'->>'priority')::int
                              WHEN raw->'extra'->>'provider' IS NULL THEN 100 ELSE 200 END
                    WHEN raw->'extra'->>'provider' IS NULL THEN 100 ELSE 200 END AS priority
          FROM postvec.models WHERE model_type = 'embed') m;
GRANT SELECT ON postvec.routes TO PUBLIC;

CREATE OR REPLACE FUNCTION postvec._route(model text, space text DEFAULT NULL)
RETURNS SETOF postvec.routes LANGUAGE sql STABLE SET search_path = pg_catalog, pg_temp AS $$
  SELECT * FROM postvec.routes
   WHERE route = $1 OR space IN ($1, $2)
   ORDER BY (route = $1) DESC, (space = $1) DESC, priority, route LIMIT 1
$$;
