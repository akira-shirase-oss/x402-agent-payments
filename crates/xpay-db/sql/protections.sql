-- The database's protections, stated once (Payment Sandbox D-058, distilled).
--
-- One row per problem; no rows means the database is protected exactly as the migrations
-- left it. Checked absolutely, not by comparison with another copy: the server runs it at
-- startup and refuses to serve on any row (xpay_db::guard::check_runtime), and the
-- restore drills and the upgrade rehearsal run it on every restored copy.
--
-- A migration that adds or changes a table, a trigger, a function or a grant updates this
-- file in the same commit. The digests are md5(pg_get_triggerdef(oid)) and
-- md5(pg_get_functiondef(oid)) on a freshly migrated database (`xpayctl digests`
-- prints them), on the PostgreSQL major the copy runs (18): another major may format a
-- definition differently, so re-derive them when it changes.
WITH expected_triggers (tbl, name, digest) AS (
    -- The events, timing, level and function of each trigger.
    VALUES ('account_events', 'account_events_append_only', 'bd4b64bf88a43d91c8f0229e240a55e1'),
           ('account_events', 'account_events_no_truncate', '91d454c7d8890d590a6e8305d980409e'),
           ('account_events', 'account_events_stamp', '0558d12b98ccd7beaf941f03835399de')
), expected_functions (name, digest) AS (
    -- The whole definition, body and settings included.
    VALUES ('refuse_change', 'aa7b64a277372e0a3e2126f016e3a79d'),
           ('stamp_event', 'a9e1e842d2841cfd8220413bd5548495')
), expected_grants (tbl, privileges) AS (
    -- Every table in public, with exactly what xpay_app holds on it.
    VALUES ('_sqlx_migrations', 'SELECT'),
           ('account_events', 'INSERT,SELECT'),
           ('api_tokens', 'DELETE,INSERT,SELECT,UPDATE'),
           ('email_tokens', 'DELETE,INSERT,SELECT,UPDATE'),
           ('invites', 'DELETE,INSERT,SELECT,UPDATE'),
           ('members', 'DELETE,INSERT,SELECT,UPDATE'),
           ('sessions', 'DELETE,INSERT,SELECT,UPDATE'),
           ('sign_in_trust', 'DELETE,INSERT,SELECT,UPDATE'),
           ('users', 'DELETE,INSERT,SELECT,UPDATE'),
           ('workspaces', 'DELETE,INSERT,SELECT,UPDATE')
), tables AS (
    SELECT c.oid, c.relname, pg_catalog.pg_get_userbyid(c.relowner) AS owner
      FROM pg_catalog.pg_class c
      JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
     WHERE n.nspname = 'public' AND c.relkind IN ('r', 'p')
), held AS (
    SELECT t.relname,
           coalesce(string_agg(p.privilege, ',' ORDER BY p.privilege), '') AS privileges
      FROM tables t
      LEFT JOIN LATERAL (
          SELECT x AS privilege
            FROM unnest(ARRAY['SELECT', 'INSERT', 'UPDATE', 'DELETE', 'TRUNCATE', 'REFERENCES', 'TRIGGER']) AS x
           WHERE pg_catalog.has_table_privilege('xpay_app', t.oid, x)
      ) p ON true
     GROUP BY t.relname
)
SELECT problem FROM (
    SELECT format('trigger %s.%s is missing or not enabled ALWAYS', e.tbl, e.name) AS problem
      FROM expected_triggers e
     WHERE NOT EXISTS (
         SELECT 1 FROM pg_catalog.pg_trigger t
          WHERE t.tgrelid = pg_catalog.to_regclass('public.' || e.tbl)
            AND t.tgname = e.name AND t.tgenabled = 'A' AND NOT t.tgisinternal)
    UNION ALL
    SELECT format('trigger %s.%s is not defined as the migrations left it', e.tbl, e.name)
      FROM expected_triggers e
      JOIN pg_catalog.pg_trigger t
        ON t.tgrelid = pg_catalog.to_regclass('public.' || e.tbl) AND t.tgname = e.name
     WHERE pg_catalog.md5(pg_catalog.pg_get_triggerdef(t.oid)) <> e.digest
    UNION ALL
    SELECT format('trigger %s.%s is not in the manifest', c.relname, t.tgname)
      FROM pg_catalog.pg_trigger t
      JOIN pg_catalog.pg_class c ON c.oid = t.tgrelid
      JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
     WHERE n.nspname = 'public' AND NOT t.tgisinternal
       AND NOT EXISTS (SELECT 1 FROM expected_triggers e WHERE e.tbl = c.relname AND e.name = t.tgname)
    UNION ALL
    SELECT format('function %s is missing', e.name)
      FROM expected_functions e
     WHERE NOT EXISTS (
         SELECT 1 FROM pg_catalog.pg_proc p
          JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
         WHERE n.nspname = 'public' AND p.proname = e.name)
    UNION ALL
    SELECT format('function %s is not defined as the migrations left it', p.proname)
      FROM pg_catalog.pg_proc p
      JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
      LEFT JOIN expected_functions e ON e.name = p.proname
     WHERE n.nspname = 'public'
       AND pg_catalog.md5(pg_catalog.pg_get_functiondef(p.oid)) IS DISTINCT FROM e.digest
    UNION ALL
    SELECT format('function %s is SECURITY DEFINER or not owned by xpay_owner', p.proname)
      FROM pg_catalog.pg_proc p
      JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
     WHERE n.nspname = 'public'
       AND (p.prosecdef OR pg_catalog.pg_get_userbyid(p.proowner) <> 'xpay_owner')
    UNION ALL
    SELECT format('function %s does not pin search_path = pg_catalog, pg_temp', p.proname)
      FROM pg_catalog.pg_proc p
      JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
     WHERE n.nspname = 'public'
       AND NOT coalesce(p.proconfig @> ARRAY['search_path=pg_catalog, pg_temp'], false)
    UNION ALL
    SELECT format('xpay_app holds [%s] on %s; expected [%s]', h.privileges, h.relname,
                  coalesce(g.privileges, ''))
      FROM held h LEFT JOIN expected_grants g ON g.tbl = h.relname
     WHERE h.privileges <> coalesce(g.privileges, '')
    UNION ALL
    SELECT format('table %s is missing', g.tbl)
      FROM expected_grants g
     WHERE NOT EXISTS (SELECT 1 FROM tables t WHERE t.relname = g.tbl)
    UNION ALL
    SELECT format('table %s is owned by %s, not xpay_owner', t.relname, t.owner)
      FROM tables t WHERE t.owner <> 'xpay_owner'
    UNION ALL
    SELECT 'xpay_app may create temporary tables'
     WHERE pg_catalog.has_database_privilege('xpay_app', pg_catalog.current_database(), 'TEMP')
    UNION ALL
    SELECT 'xpay_app may create objects in the database or in schema public'
     WHERE pg_catalog.has_database_privilege('xpay_app', pg_catalog.current_database(), 'CREATE')
        OR pg_catalog.has_schema_privilege('xpay_app', 'public', 'CREATE')
) problems
ORDER BY problem;
