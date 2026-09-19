// Copyright (C) 2026 Plumb contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Plumb-only catalog and planner isolation regressions. Each pg_test rolls back.

#[pgrx::pg_schema]
mod tests {
    use pgrx::prelude::*;

    #[pg_test]
    fn canonical_catalog_identity_has_no_tin_aliases() {
        assert_eq!(
            Spi::get_one::<bool>(
                "SELECT e.extversion = '0.1.0' AND n.nspname = 'plumb'
             FROM pg_extension e JOIN pg_namespace n ON n.oid=e.extnamespace
             WHERE e.extname='plumb'"
            )
            .unwrap(),
            Some(true)
        );
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM pg_extension WHERE extname='tin'").unwrap(),
            Some(0)
        );
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM pg_am WHERE amname='tin'").unwrap(),
            Some(0)
        );
        assert_eq!(Spi::get_one::<bool>(
        "SELECT to_regnamespace('tin') IS NULL AND to_regoperator('pg_catalog.==>(text,text)') IS NULL"
    ).unwrap(), Some(true));
        assert_eq!(
            Spi::get_one::<bool>(
                "SELECT o.oprcode = 'plumb.plumb_text_cmpfunc(text,text)'::regprocedure
           AND p.probin = 'plumb'
           AND c.opcnamespace = 'plumb'::regnamespace AND c.opcdefault
           AND a.amhandler = 'plumb.amhandler(internal)'::regprocedure
         FROM pg_operator o JOIN pg_proc p ON p.oid=o.oprcode
         JOIN pg_opclass c ON c.opcname='plumb_text_ops'
         JOIN pg_am a ON a.oid=c.opcmethod AND a.amname='plumb'
         WHERE o.oid='pg_catalog.~~>(text,text)'::regoperator"
            )
            .unwrap(),
            Some(true)
        );
        assert_eq!(Spi::get_one::<i64>(
        "SELECT count(*) FROM pg_depend d JOIN pg_extension e ON e.oid=d.refobjid
         WHERE d.refclassid='pg_extension'::regclass AND d.deptype='e' AND e.extname='plumb'
         AND ((d.classid='pg_operator'::regclass AND d.objid='pg_catalog.~~>(text,text)'::regoperator)
           OR (d.classid='pg_proc'::regclass AND d.objid='plumb.plumb_text_cmpfunc(text,text)'::regprocedure)
           OR (d.classid='pg_am'::regclass AND d.objid=(SELECT oid FROM pg_am WHERE amname='plumb')))")
        .unwrap(), Some(3));
    }

    fn shadow_fixture() {
        Spi::run(
        "CREATE SCHEMA plumb_shadow;
         CREATE FUNCTION plumb_shadow.match(text,text) RETURNS boolean
           LANGUAGE plpgsql STABLE AS $$ BEGIN RETURN true; END $$;
         CREATE OPERATOR plumb_shadow.~~> (PROCEDURE=plumb_shadow.match, LEFTARG=text, RIGHTARG=text);
         CREATE TABLE identity_docs(id int, body text);
         INSERT INTO identity_docs VALUES (1, 'beer wine'), (2, 'wine');
         CREATE INDEX identity_docs_idx ON identity_docs USING plumb(body);
         SET LOCAL search_path=plumb_shadow,pg_catalog,public;"
    ).unwrap();
    }

    #[pg_test]
    fn exact_operator_oid_ignores_schema_and_signature_shadows() {
        shadow_fixture();
        Spi::run(
        "CREATE FUNCTION plumb_shadow.overload(text,varchar) RETURNS boolean
           LANGUAGE plpgsql STABLE AS $$ BEGIN RETURN true; END $$;
         CREATE OPERATOR pg_catalog.~~> (PROCEDURE=plumb_shadow.overload, LEFTARG=text, RIGHTARG=varchar);"
    ).unwrap();
        let actual = unsafe { crate::operator::search_operator_oid() };
        let expected =
            Spi::get_one::<pg_sys::Oid>("SELECT 'pg_catalog.~~>(text,text)'::regoperator::oid")
                .unwrap()
                .unwrap();
        let shadow =
            Spi::get_one::<pg_sys::Oid>("SELECT 'plumb_shadow.~~>(text,text)'::regoperator::oid")
                .unwrap()
                .unwrap();
        let overload =
            Spi::get_one::<pg_sys::Oid>("SELECT 'pg_catalog.~~>(text,varchar)'::regoperator::oid")
                .unwrap()
                .unwrap();
        assert_eq!(actual, expected);
        assert_ne!(actual, shadow);
        assert_ne!(actual, overload);
        let baseline = Spi::get_one::<f32>(
        "SELECT plumb.full_score(ctid) FROM public.identity_docs WHERE body OPERATOR(pg_catalog.~~>) 'beer'"
    ).unwrap();
        let mixed = Spi::get_one::<f32>(
            "SELECT plumb.full_score(ctid) FROM public.identity_docs
         WHERE body OPERATOR(pg_catalog.~~>) 'beer'
           AND body ~~> 'wine' AND body OPERATOR(pg_catalog.~~>) 'wine'::varchar",
        )
        .unwrap();
        assert_eq!(mixed, baseline);
        assert_eq!(
            Spi::get_one::<String>(
                "SELECT plumb.highlight(body) FROM public.identity_docs
         WHERE body OPERATOR(pg_catalog.~~>) 'beer'
           AND body ~~> 'wine' AND body OPERATOR(pg_catalog.~~>) 'wine'::varchar"
            )
            .unwrap(),
            Some("<b>beer</b> wine".into())
        );
    }

    #[pg_test(
        error = "plumb.full_score() requires a plumb index scan and cannot be used in this query context"
    )]
    fn scoring_rejects_shadow_operator() {
        shadow_fixture();
        Spi::run("SELECT plumb.full_score(ctid) FROM public.identity_docs WHERE body ~~> 'beer'")
            .unwrap();
    }

    #[pg_test(
        error = "plumb.highlight() requires an explicit query or a matching plumb index scan"
    )]
    fn highlighting_rejects_shadow_operator() {
        shadow_fixture();
        Spi::run("SELECT plumb.highlight(body) FROM public.identity_docs WHERE body ~~> 'beer'")
            .unwrap();
    }

    #[pg_test]
    fn operator_lookup_observes_catalog_recreation_without_oid_cache() {
        let original = unsafe { crate::operator::search_operator_oid() };
        Spi::run(
            "CREATE SCHEMA moved_operator;
         ALTER OPERATOR pg_catalog.~~>(text,text) SET SCHEMA moved_operator;",
        )
        .unwrap();
        assert_eq!(
            unsafe { crate::operator::search_operator_oid() },
            pg_sys::InvalidOid
        );
        Spi::run("CREATE OPERATOR pg_catalog.~~> (PROCEDURE=plumb.plumb_text_cmpfunc, LEFTARG=text, RIGHTARG=text)").unwrap();
        let current = unsafe { crate::operator::search_operator_oid() };
        assert_ne!(original, current);
        assert_eq!(
            Some(current),
            Spi::get_one::<pg_sys::Oid>("SELECT 'pg_catalog.~~>(text,text)'::regoperator::oid")
                .unwrap()
        );
    }
}
