-- 0001: what every copy keeps, whatever else it adds.
--
-- Run by `xpayctl migrate` as xpay_owner, which owns the database (roles.rs). The
-- runtime role xpay_app may use the schema and read the migration ledger (the
-- binary/schema compatibility check, schema.rs); every table grant is explicit and the
-- manifest (crates/xpay-db/sql/protections.sql) checks them exactly.
REVOKE ALL ON SCHEMA public FROM PUBLIC;
GRANT USAGE ON SCHEMA public TO xpay_app;
GRANT SELECT ON _sqlx_migrations TO xpay_app;

-- Refuses the row or statement it is attached to: append-only tables use it for UPDATE,
-- DELETE and TRUNCATE. The search path is pinned (pg_temp last) so no temporary object can
-- stand in for anything it names, and it runs with the caller's rights.
CREATE FUNCTION public.refuse_change() RETURNS trigger
LANGUAGE plpgsql
SECURITY INVOKER
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    RAISE EXCEPTION '% on %.% is refused: the table is append-only', TG_OP, TG_TABLE_SCHEMA, TG_TABLE_NAME
        USING ERRCODE = 'P0001';
END
$$;

-- Stamps an appended row with the database's clock, so the runtime role cannot backdate.
CREATE FUNCTION public.stamp_event() RETURNS trigger
LANGUAGE plpgsql
SECURITY INVOKER
SET search_path = pg_catalog, pg_temp
AS $$
BEGIN
    NEW.at := pg_catalog.now();
    RETURN NEW;
END
$$;
