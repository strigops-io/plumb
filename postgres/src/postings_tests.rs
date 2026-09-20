// SPDX-License-Identifier: AGPL-3.0-or-later
// Added by Plumb contributors on 2026-09-20: experimental postings AM regressions.
#[pgrx::pg_schema]
mod tests {
    use pgrx::prelude::*;

    fn ids(query: &str) -> Vec<i32> {
        Spi::get_one::<Vec<i32>>(&format!(
            "SELECT coalesce(array_agg(id ORDER BY id), '{{}}'::int[]) FROM p_docs WHERE body ~~> {}",
            pgrx::spi::quote_literal(query)
        )).unwrap().unwrap()
    }
    fn setup() {
        Spi::run("CREATE TABLE p_docs(id int, body text) WITH (fillfactor=60);
            INSERT INTO p_docs VALUES (1,'craft beer'),(2,'wine'),(3,'beer festival'),(4,NULL),(5,'...');
            CREATE INDEX p_docs_idx ON p_docs USING plumb(body) WITH(storage='postings_v1');
            SET LOCAL enable_seqscan=off;").unwrap();
    }
    fn segments() -> i64 {
        Spi::get_one::<i64>(
            "SELECT (plumb.index_stats('p_docs_idx'::regclass)->>'segments')::bigint",
        )
        .unwrap()
        .unwrap()
    }

    #[pg_test]
    fn postings_term_boolean_exact_bitmap_and_stats() {
        setup();
        assert_eq!(ids("BEER"), vec![1, 3]);
        assert_eq!(ids("craft AND beer"), vec![1]);
        assert_eq!(ids("wine OR beer"), vec![1, 2, 3]);
        assert_eq!(ids("beer^2"), vec![1, 3]);
        assert_eq!(segments(), 1);
        assert_eq!(
            Spi::get_one::<String>("SELECT plumb.index_stats('p_docs_idx')->>'storage'")
                .unwrap()
                .as_deref(),
            Some("postings_v1")
        );
        assert!(
            Spi::get_one::<i64>(
                "SELECT (plumb.index_stats('p_docs_idx')->>'relation_blocks')::bigint"
            )
            .unwrap()
            .unwrap()
                > 0
        );
        let plan = Spi::get_one::<pgrx::Json>(
            "EXPLAIN (ANALYZE,FORMAT JSON) SELECT * FROM p_docs WHERE body ~~> 'beer'",
        )
        .unwrap()
        .unwrap()
        .0;
        assert_eq!(plan[0]["Plan"]["Node Type"], "Bitmap Heap Scan");
        assert_eq!(plan[0]["Plan"]["Exact Heap Blocks"], 1);
        assert_eq!(plan[0]["Plan"]["Lossy Heap Blocks"], 0);
        assert_eq!(plan[0]["Plan"]["Plans"][0]["Actual Rows"], 2);
    }

    #[pg_test]
    fn postings_unsupported_queries_fall_back_to_heap() {
        setup();
        assert_eq!(ids("\"craft beer\""), vec![1]);
        assert_eq!(ids("beer AND NOT craft"), vec![3]);
        assert_eq!(ids("bee*"), vec![1, 3]);
        assert_eq!(ids("*"), vec![1, 2, 3]);
        let plan = Spi::get_one::<pgrx::Json>(
            "EXPLAIN (ANALYZE,FORMAT JSON) SELECT * FROM p_docs WHERE body ~~> '\"craft beer\"'",
        )
        .unwrap()
        .unwrap()
        .0;
        assert_eq!(plan[0]["Plan"]["Lossy Heap Blocks"], 1);
    }

    #[pg_test]
    fn postings_mutations_rollback_and_hot_recheck() {
        setup();
        let before = segments();
        // id is not indexed; this is HOT eligible. Index candidates retain root TID.
        Spi::run("UPDATE p_docs SET id=10 WHERE id=1").unwrap();
        assert_eq!(segments(), before);
        assert_eq!(ids("craft"), vec![10]);
        Spi::run(
            "INSERT INTO p_docs VALUES (6,'new beer'),(7,NULL),(8,'...');
            UPDATE p_docs SET body='wine' WHERE id=3;
            DELETE FROM p_docs WHERE id=10;
            DO $$ BEGIN
              BEGIN
                INSERT INTO p_docs VALUES (9,'rollback beer');
                RAISE EXCEPTION 'rollback test';
              EXCEPTION WHEN raise_exception THEN NULL;
              END;
            END $$;",
        )
        .unwrap();
        assert_eq!(ids("beer"), vec![6]);
        assert_eq!(ids("wine"), vec![2, 3]);
        assert_eq!(ids("rollback"), Vec::<i32>::new());
        assert!(segments() > before);
    }

    #[pg_test]
    fn postings_partial_expression_multiple_keys_and_rescan() {
        Spi::run("CREATE TABLE p_expr(id int, body text, active bool);
            INSERT INTO p_expr VALUES (1,'BEER CRAFT',true),(2,'BEER',true),(3,'BEER CRAFT',false),(4,NULL,true);
            CREATE INDEX p_expr_idx ON p_expr USING plumb(lower(body)) WITH(storage='postings_v1') WHERE active;
            SET LOCAL enable_seqscan=off;").unwrap();
        assert_eq!(Spi::get_one::<Vec<i32>>("SELECT array_agg(id ORDER BY id) FROM p_expr WHERE active AND lower(body) ~~> 'beer' AND lower(body) ~~> 'craft'").unwrap(),Some(vec![1]));
        Spi::run("UPDATE p_expr SET active=true WHERE id=3").unwrap();
        assert_eq!(Spi::get_one::<Vec<i32>>("SELECT array_agg(id ORDER BY id) FROM p_expr WHERE active AND lower(body) ~~> 'craft'").unwrap(),Some(vec![1,3]));
        assert_eq!(Spi::get_one::<Vec<i32>>("SELECT array_agg(n ORDER BY q) FROM (VALUES ('beer'),('craft'),('absent')) v(q) CROSS JOIN LATERAL (SELECT count(*)::int n FROM p_expr WHERE active AND lower(body) ~~> v.q OFFSET 0) x").unwrap(),Some(vec![0,3,2]));
    }

    #[pg_test]
    fn postings_persisted_identity_survives_heap_reloption_toggle() {
        setup();
        Spi::run(
            "ALTER INDEX p_docs_idx SET(storage='heap'); INSERT INTO p_docs VALUES(6,'beer');",
        )
        .unwrap();
        assert_eq!(ids("beer"), vec![1, 3, 6]);
        assert_eq!(segments(), 2);
        let plan = Spi::get_one::<pgrx::Json>(
            "EXPLAIN (ANALYZE,FORMAT JSON) SELECT * FROM p_docs WHERE body ~~> 'beer'",
        )
        .unwrap()
        .unwrap()
        .0;
        assert_eq!(plan[0]["Plan"]["Lossy Heap Blocks"], 0);
        Spi::run("REINDEX INDEX p_docs_idx").unwrap();
        assert_eq!(
            Spi::get_one::<String>("SELECT plumb.index_stats('p_docs_idx')->>'storage'")
                .unwrap()
                .as_deref(),
            Some("heap")
        );
        assert_eq!(ids("beer"), vec![1, 3, 6]);
    }

    #[pg_test]
    fn postings_reindex_truncate_and_empty_build() {
        setup();
        Spi::run("INSERT INTO p_docs VALUES(6,'beer'); REINDEX INDEX p_docs_idx;").unwrap();
        assert_eq!(segments(), 1);
        assert_eq!(ids("beer"), vec![1, 3, 6]);
        Spi::run("TRUNCATE p_docs").unwrap();
        assert_eq!(ids("beer"), Vec::<i32>::new());
        Spi::run("INSERT INTO p_docs VALUES(9,'beer')").unwrap();
        assert_eq!(ids("beer"), vec![9]);
    }

    #[pg_test(
        error = "postings_v1 metadata is missing; ALTER INDEX cannot convert a heap-baseline index; REINDEX is required"
    )]
    fn postings_heap_toggle_rejects_scan_without_reindex() {
        Spi::run(
            "CREATE TABLE p_legacy(body text); INSERT INTO p_legacy VALUES('beer');
            CREATE INDEX p_legacy_idx ON p_legacy USING plumb(body);
            ALTER INDEX p_legacy_idx SET(storage='postings_v1'); SET LOCAL enable_seqscan=off;",
        )
        .unwrap();
        let _ = Spi::get_one::<i64>("SELECT count(*) FROM p_legacy WHERE body ~~> 'beer'");
    }

    #[pg_test(
        error = "postings_v1 metadata is missing; ALTER INDEX cannot convert a heap-baseline index; REINDEX is required"
    )]
    fn postings_heap_toggle_rejects_insert_without_reindex() {
        Spi::run(
            "CREATE TABLE p_legacy_write(body text); INSERT INTO p_legacy_write VALUES('beer');
            CREATE INDEX p_legacy_write_idx ON p_legacy_write USING plumb(body);
            ALTER INDEX p_legacy_write_idx SET(storage='postings_v1');
            INSERT INTO p_legacy_write VALUES('wine');",
        )
        .unwrap();
    }

    #[pg_test(
        error = "postings_v1 supports only the default analyzer; restore default tokenizer options and REINDEX"
    )]
    fn postings_rejects_nondefault_analyzer_build() {
        Spi::run("CREATE TABLE p_analyzer(body text); CREATE INDEX p_analyzer_idx ON p_analyzer USING plumb(body) WITH(storage='postings_v1',tokenizer='whitespace');").unwrap();
    }

    #[pg_test(
        error = "postings_v1 supports only the default analyzer; restore default tokenizer options and REINDEX"
    )]
    fn postings_rejects_analyzer_toggle_scan_even_after_storage_toggle() {
        setup();
        Spi::run("ALTER INDEX p_docs_idx SET(storage='heap',case_folding='preserve')").unwrap();
        let _ = ids("beer");
    }

    #[pg_test(
        error = "postings_v1 supports only the default analyzer; restore default tokenizer options and REINDEX"
    )]
    fn postings_rejects_analyzer_toggle_insert() {
        setup();
        Spi::run("ALTER INDEX p_docs_idx SET(accent_folding='preserve'); INSERT INTO p_docs VALUES(9,'wine')").unwrap();
    }

    #[pg_test(
        error = "postings_v1 requires a permanent heap; temporary/unlogged relations are unsupported"
    )]
    fn postings_rejects_temporary_heap() {
        Spi::run("CREATE TEMP TABLE p_temp(body text); CREATE INDEX p_temp_idx ON p_temp USING plumb(body) WITH(storage='postings_v1');").unwrap();
    }

    #[pg_test(
        error = "postings_v1 requires a permanent heap; temporary/unlogged relations are unsupported"
    )]
    fn postings_rejects_unlogged_heap() {
        Spi::run("CREATE UNLOGGED TABLE p_unlogged(body text); CREATE INDEX p_unlogged_idx ON p_unlogged USING plumb(body) WITH(storage='postings_v1');").unwrap();
    }

    #[pg_test]
    fn postings_accepts_explicit_default_analyzer_and_scoring_options() {
        Spi::run("CREATE TABLE p_defaults(body text); INSERT INTO p_defaults VALUES('BEÉR');
            CREATE INDEX p_defaults_idx ON p_defaults USING plumb(body) WITH(storage='postings_v1',tokenizer='unicode',case_folding='fold',accent_folding='fold',long_tokens='split',max_token_bytes=256,graphemes='emoji',position_gaps='preserve',k1=2,b=0.5);
            SET LOCAL enable_seqscan=off;").unwrap();
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM p_defaults WHERE body ~~> 'beer'").unwrap(),
            Some(1)
        );
    }

    #[pg_test(error = "postings_v1 requires strategy 1 to be pg_catalog.~~>(text,text)")]
    fn postings_rejects_text_inequality_opclass_at_build() {
        // Nonempty heap proves the guard runs before collecting any document.
        Spi::run(
            "CREATE OPERATOR CLASS p_inequality_ops FOR TYPE text USING plumb AS
                OPERATOR 1 pg_catalog.<>(text,text), STORAGE text;
             CREATE TABLE p_inequality(body text);
             INSERT INTO p_inequality VALUES ('beer'),('wine');
             CREATE INDEX p_inequality_idx ON p_inequality USING plumb(body p_inequality_ops)
                WITH(storage='postings_v1');",
        )
        .unwrap();
    }

    #[pg_test(error = "postings_v1 requires a text input and text index key")]
    fn postings_rejects_integer_opclass_before_datum_cast() {
        Spi::run(
            "CREATE OPERATOR CLASS p_integer_ops FOR TYPE integer USING plumb AS
                OPERATOR 1 pg_catalog.=(integer,integer), STORAGE integer;
             CREATE TABLE p_integer(value integer);
             INSERT INTO p_integer VALUES (1),(2147483647);
             CREATE INDEX p_integer_idx ON p_integer USING plumb(value p_integer_ops)
                WITH(storage='postings_v1');",
        )
        .unwrap();
    }

    #[pg_test(error = "postings_v1 requires strategy 1 to be pg_catalog.~~>(text,text)")]
    fn postings_rejects_incompatible_opclass_even_on_empty_heap() {
        Spi::run(
            "CREATE OPERATOR CLASS p_empty_ops FOR TYPE text USING plumb AS
                OPERATOR 1 pg_catalog.<>(text,text), STORAGE text;
             CREATE TABLE p_empty(body text);
             CREATE INDEX p_empty_idx ON p_empty USING plumb(body p_empty_ops)
                WITH(storage='postings_v1');",
        )
        .unwrap();
    }

    #[pg_test]
    fn postings_accepts_equivalent_opclass_and_validates_semantics() {
        Spi::run(
            "CREATE OPERATOR CLASS p_equivalent_ops FOR TYPE text USING plumb AS
                OPERATOR 1 pg_catalog.~~>(text,text), STORAGE text;
             CREATE OPERATOR CLASS p_invalid_ops FOR TYPE text USING plumb AS
                OPERATOR 1 pg_catalog.<>(text,text), STORAGE text;
             CREATE TABLE p_equivalent(body text);
             INSERT INTO p_equivalent VALUES ('beer'),('wine');
             CREATE INDEX p_equivalent_idx ON p_equivalent USING plumb(body p_equivalent_ops)
                WITH(storage='postings_v1');
             INSERT INTO p_equivalent VALUES ('craft beer');
             SET LOCAL enable_seqscan=off;",
        )
        .unwrap();
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM p_equivalent WHERE body ~~> 'beer'").unwrap(),
            Some(2)
        );
        assert_eq!(
            Spi::get_one::<bool>(
                "SELECT pg_catalog.amvalidate(oid) FROM pg_opclass WHERE opcname='p_equivalent_ops'"
            )
            .unwrap(),
            Some(true)
        );
        assert_eq!(
            Spi::get_one::<bool>(
                "SELECT pg_catalog.amvalidate(oid) FROM pg_opclass WHERE opcname='p_invalid_ops'"
            )
            .unwrap(),
            Some(false)
        );
    }

    #[pg_test(error = "postings_v1 requires canonical text-search scan keys")]
    fn postings_rejects_cross_type_query_before_datum_cast() {
        // A valid text class can live in a family containing foreign cross-type
        // operators. Validate every scan key, not just the family's text member.
        Spi::run(
            "CREATE FUNCTION p_foreign_match(text, integer) RETURNS boolean
                LANGUAGE sql IMMUTABLE STRICT AS 'SELECT true';
             CREATE OPERATOR public.~~# (PROCEDURE=p_foreign_match, LEFTARG=text, RIGHTARG=integer);
             CREATE OPERATOR CLASS p_cross_ops FOR TYPE text USING plumb AS
                OPERATOR 1 pg_catalog.~~>(text,text), STORAGE text;
             ALTER OPERATOR FAMILY p_cross_ops USING plumb ADD
                OPERATOR 1 public.~~#(text,integer);
             CREATE TABLE p_cross(body text);
             INSERT INTO p_cross VALUES ('beer');
             CREATE INDEX p_cross_idx ON p_cross USING plumb(body p_cross_ops)
                WITH(storage='postings_v1');
             SET LOCAL enable_seqscan=off;",
        )
        .unwrap();
        let _ =
            Spi::get_one::<i64>("SELECT count(*) FROM p_cross WHERE body OPERATOR(public.~~#) 1");
    }

    #[pg_test(error = "postings_v1 requires strategy 1 to be pg_catalog.~~>(text,text)")]
    fn postings_rejects_operator_identity_change_on_insert() {
        setup();
        Spi::run(
            "CREATE SCHEMA p_moved;
             ALTER OPERATOR pg_catalog.~~>(text,text) SET SCHEMA p_moved;
             INSERT INTO p_docs VALUES (6,'beer');",
        )
        .unwrap();
    }

    #[pg_test(error = "postings_v1 requires strategy 1 to be pg_catalog.~~>(text,text)")]
    fn postings_rejects_operator_identity_change_on_scan() {
        setup();
        Spi::run(
            "CREATE SCHEMA p_moved;
             ALTER OPERATOR pg_catalog.~~>(text,text) SET SCHEMA p_moved;",
        )
        .unwrap();
        let _ = Spi::get_one::<i64>(
            "SELECT count(*) FROM p_docs WHERE body OPERATOR(p_moved.~~>) 'beer'",
        );
    }

    #[pg_test]
    fn postings_hardening_keeps_foreign_heap_baseline_working() {
        Spi::run(
            "CREATE OPERATOR CLASS p_heap_inequality_ops FOR TYPE text USING plumb AS
                OPERATOR 1 pg_catalog.<>(text,text), STORAGE text;
             CREATE TABLE p_heap_inequality(body text);
             INSERT INTO p_heap_inequality VALUES ('beer'),('wine');
             CREATE INDEX p_heap_inequality_idx ON p_heap_inequality
                USING plumb(body p_heap_inequality_ops);
             INSERT INTO p_heap_inequality VALUES ('craft beer');
             SET LOCAL enable_seqscan=off;",
        )
        .unwrap();
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM p_heap_inequality WHERE body <> 'beer'")
                .unwrap(),
            Some(2)
        );
    }

    #[pg_test(error = "plumb.index_stats requires an index using the plumb access method")]
    fn postings_stats_rejects_other_access_methods() {
        Spi::run("CREATE TABLE p_wrong(id int); CREATE INDEX p_wrong_idx ON p_wrong(id)").unwrap();
        let _ = Spi::get_one::<pgrx::JsonB>("SELECT plumb.index_stats('p_wrong_idx')");
    }
}
