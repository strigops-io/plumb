// Copyright (C) 2026 Plumb contributors
// SPDX-License-Identifier: AGPL-3.0-or-later
//! Exact top-k regression tests. The harness rolls every pg_test back.
#[pgrx::pg_schema]
mod tests {
    use pgrx::prelude::*;

    fn fixture(storage: &str) {
        Spi::run(
            "CREATE TABLE topk_docs(id int, body text);
            INSERT INTO topk_docs VALUES
            (1,'beer beer wine'),(2,'beer'),(3,'beer'),(4,'wine cheese'),
            (5,NULL),(6,''),(7,'beer beer beer beer beer beer beer beer beer beer beer beer'),
            (8,'nothing relevant'),(9,'wine');",
        )
        .unwrap();
        Spi::run(&format!(
            "CREATE INDEX topk_docs_idx ON topk_docs USING plumb(body) WITH(storage='{storage}')"
        ))
        .unwrap();
        Spi::run("SET LOCAL enable_seqscan=off").unwrap();
    }

    fn assert_reference(query: &str, k: i32) {
        // Test strings are fixed literals, not a public SQL interpolation API.
        let actual = Spi::get_one::<Vec<String>>(&format!(
            "SELECT array_agg(ctid::text || ':' || pg_catalog.encode(pg_catalog.float4send(score),'hex') ORDER BY ord)
             FROM plumb.top_k('topk_docs_idx', '{query}', {k}) WITH ORDINALITY AS t(ctid,score,ord)"
        )).unwrap();
        let expected = Spi::get_one::<Vec<String>>(&format!(
            "SELECT array_agg(ctid::text || ':' || pg_catalog.encode(pg_catalog.float4send(score),'hex') ORDER BY score DESC,ctid)
             FROM (SELECT ctid,plumb.full_score(ctid) AS score FROM topk_docs
                   WHERE body OPERATOR(pg_catalog.~~>) '{query}' ORDER BY score DESC,ctid LIMIT {k}) s"
        )).unwrap();
        assert_eq!(actual, expected, "query={query}, k={k}");
    }

    #[pg_test]
    fn topk_matches_full_score_with_ties_boosts_null_empty_and_tf_buckets() {
        fixture("postings_v1");
        for query in [
            "beer",
            "beer OR wine",
            "beer AND wine",
            "beer^2 OR wine^3",
            "wine OR beer OR wine",
            "absent",
        ] {
            for k in [0, 1, 3, 10, 1000] {
                assert_reference(query, k);
            }
        }
    }

    #[pg_test]
    fn topk_heap_storage_and_nondefault_bm25_match_reference() {
        fixture("heap");
        Spi::run("ALTER INDEX topk_docs_idx SET(k1=2.0,b=0.2,score_stop_words='beer,wine')")
            .unwrap();
        assert_reference("beer OR wine^2", 10);
        // Full BM25 ignores score_stop_words and dense-term filtering.
        assert_reference("beer", 1);
    }

    #[pg_test]
    fn topk_fresh_after_update_delete_and_rollback() {
        fixture("postings_v1");
        assert_reference("beer OR wine", 10);
        Spi::run(
            "UPDATE topk_docs SET body='beer wine wine' WHERE id=2;
            DELETE FROM topk_docs WHERE id=3; INSERT INTO topk_docs VALUES(10,'wine wine');",
        )
        .unwrap();
        assert_reference("beer OR wine", 10);
        // SPI cannot execute SAVEPOINT directly; PL/pgSQL exception blocks use
        // real internal subtransactions and roll back both heap and index writes.
        Spi::run(
            "DO $$ BEGIN BEGIN
            UPDATE topk_docs SET body='beer beer beer' WHERE id=4;
            PERFORM * FROM plumb.top_k('topk_docs_idx','beer OR wine');
            RAISE EXCEPTION 'rollback marker';
            EXCEPTION WHEN raise_exception THEN NULL; END; END $$",
        )
        .unwrap();
        assert_reference("beer OR wine", 10);
    }

    #[pg_test]
    fn topk_quotes_catalog_identifiers() {
        Spi::run(
            "CREATE SCHEMA \"topk schema\";
            CREATE TABLE \"topk schema\".\"odd table\"(\"odd body\" text);
            INSERT INTO \"topk schema\".\"odd table\" VALUES('beer');
            CREATE INDEX \"odd index\" ON \"topk schema\".\"odd table\" USING plumb(\"odd body\");",
        )
        .unwrap();
        assert_eq!(
            Spi::get_one::<i64>(
                "SELECT count(*) FROM plumb.top_k('\"topk schema\".\"odd index\"', 'beer')"
            )
            .unwrap(),
            Some(1)
        );
    }

    #[pg_test(error = "plumb.top_k k must be between 0 and 1000")]
    fn topk_rejects_negative_k() {
        fixture("heap");
        Spi::run("SELECT * FROM plumb.top_k('topk_docs_idx','beer',-1)").unwrap();
    }

    #[pg_test(error = "plumb.top_k k must be between 0 and 1000")]
    fn topk_rejects_large_k() {
        fixture("heap");
        Spi::run("SELECT * FROM plumb.top_k('topk_docs_idx','beer',1001)").unwrap();
    }

    #[pg_test(error = "plumb.top_k query exceeds 4096-byte budget")]
    fn topk_rejects_large_query() {
        fixture("heap");
        Spi::run("SELECT * FROM plumb.top_k('topk_docs_idx',repeat('a',4097))").unwrap();
    }

    fn distinct_term_query(count: usize) -> String {
        (0..count)
            .map(|i| {
                format!(
                    "word{}{}",
                    (b'a' + (i / 26) as u8) as char,
                    (b'a' + (i % 26) as u8) as char
                )
            })
            .collect::<Vec<_>>()
            .join(" OR ")
    }

    fn score_term_occurrences(query: &str) -> usize {
        let parsed =
            tinql::runtime::parse_tinql_to_query(query, tokenizer::presets::default_pipeline())
                .unwrap();
        let mut inputs = Vec::new();
        crate::score::collect_score_terms(&parsed, 1.0, false, &mut inputs);
        inputs.len()
    }

    #[pg_test(error = "plumb.top_k query exceeds 64-term budget")]
    fn topk_rejects_many_terms() {
        fixture("heap");
        let query = distinct_term_query(65);
        assert_eq!(score_term_occurrences(&query), 65);
        Spi::run(&format!(
            "SELECT * FROM plumb.top_k('topk_docs_idx','{query}')"
        ))
        .unwrap();
    }

    #[pg_test(error = "plumb.top_k query exceeds 64-term budget")]
    fn topk_rejects_many_boosted_occurrences_even_one_distinct_term() {
        fixture("heap");
        let query = vec!["beer^2"; 65].join(" OR ");
        assert_eq!(score_term_occurrences(&query), 65);
        Spi::run(&format!(
            "SELECT * FROM plumb.top_k('topk_docs_idx','{query}')"
        ))
        .unwrap();
    }

    #[pg_test]
    fn topk_term_budget_is_lowered_occurrences_not_raw_spelling_or_unique_terms() {
        fixture("heap");
        // The shared parser deduplicates flat, unboosted term siblings. The
        // original failing fixture therefore had TWO score inputs, not 65.
        let deduplicated = format!("{}wine", "beer OR ".repeat(64));
        assert_eq!(score_term_occurrences(&deduplicated), 2);
        assert_reference(&deduplicated, 10);
        let distinct = distinct_term_query(64);
        assert_eq!(score_term_occurrences(&distinct), 64);
        assert_reference(&distinct, 10);
        let boosted = vec!["beer^2"; 64].join(" OR ");
        assert_eq!(score_term_occurrences(&boosted), 64);
        assert_reference(&boosted, 10);
    }

    #[pg_test(
        error = "plumb.top_k supports only positive term, AND, OR and finite nonnegative boost queries"
    )]
    fn topk_rejects_unsupported_shape() {
        fixture("heap");
        Spi::run("SELECT * FROM plumb.top_k('topk_docs_idx','beer AND NOT wine')").unwrap();
    }

    #[pg_test(error = "plumb.top_k does not yet support expression or partial indexes")]
    fn topk_rejects_expression_index() {
        Spi::run("CREATE TABLE topk_expr(body text); CREATE INDEX topk_expr_idx ON topk_expr USING plumb(lower(body))").unwrap();
        Spi::run("SELECT * FROM plumb.top_k('topk_expr_idx','beer')").unwrap();
    }

    #[pg_test(error = "plumb.top_k does not yet support expression or partial indexes")]
    fn topk_rejects_partial_index() {
        Spi::run("CREATE TABLE topk_partial(id int,body text); CREATE INDEX topk_partial_idx ON topk_partial USING plumb(body) WHERE id>1").unwrap();
        Spi::run("SELECT * FROM plumb.top_k('topk_partial_idx','beer')").unwrap();
    }

    #[pg_test(error = "plumb.top_k requires a valid plumb index")]
    fn topk_rejects_other_access_method() {
        Spi::run(
            "CREATE TABLE topk_btree(body text); CREATE INDEX topk_btree_idx ON topk_btree(body)",
        )
        .unwrap();
        Spi::run("SELECT * FROM plumb.top_k('topk_btree_idx','beer')").unwrap();
    }

    #[pg_test(error = "plumb.top_k document exceeds 262144-byte budget")]
    fn topk_rejects_oversized_document_before_copying_text_to_rust() {
        fixture("heap");
        Spi::run("INSERT INTO topk_docs VALUES(10,repeat('a',262145))").unwrap();
        Spi::run("SELECT * FROM plumb.top_k('topk_docs_idx','beer')").unwrap();
    }

    #[pg_test(error = "plumb.top_k document exceeds 32768-token budget")]
    fn topk_rejects_oversized_token_stream() {
        fixture("heap");
        Spi::run("INSERT INTO topk_docs VALUES(10,repeat('a ',32769))").unwrap();
        Spi::run("SELECT * FROM plumb.top_k('topk_docs_idx','beer')").unwrap();
    }

    #[pg_test(
        error = "plumb.top_k exceeds candidate feature budget (100000 rows / 33554432 bytes)"
    )]
    fn topk_rejects_candidate_budget() {
        fixture("heap");
        Spi::run("INSERT INTO topk_docs SELECT i,'beer' FROM generate_series(1,100001) i").unwrap();
        Spi::run("SELECT * FROM plumb.top_k('topk_docs_idx','beer')").unwrap();
    }

    #[pg_test(error = "plumb.top_k exceeds 1000000-row scan budget")]
    fn topk_scan_budget_counts_null_documents() {
        fixture("heap");
        Spi::run("INSERT INTO topk_docs SELECT i,NULL FROM generate_series(1,1000001) i").unwrap();
        Spi::run("SELECT * FROM plumb.top_k('topk_docs_idx','beer')").unwrap();
    }

    #[pg_test(error = "permission denied for table topk_docs")]
    fn topk_checks_select_even_when_k_zero() {
        fixture("heap");
        Spi::run("CREATE ROLE topk_no_select; GRANT USAGE ON SCHEMA public,plumb TO topk_no_select; SET LOCAL ROLE topk_no_select").unwrap();
        Spi::run("SELECT * FROM plumb.top_k('topk_docs_idx','beer',0)").unwrap();
    }

    #[pg_test(error = "permission denied for table topk_docs")]
    fn topk_checks_indexed_column_permissions() {
        fixture("heap");
        Spi::run("CREATE ROLE topk_wrong_column; GRANT USAGE ON SCHEMA public,plumb TO topk_wrong_column;
            GRANT SELECT(id) ON topk_docs TO topk_wrong_column; SET LOCAL ROLE topk_wrong_column").unwrap();
        Spi::run("SELECT * FROM plumb.top_k('topk_docs_idx','beer')").unwrap();
    }

    #[pg_test]
    fn topk_rls_statistics_and_candidates_use_same_invoker_corpus() {
        fixture("heap");
        Spi::run(
            "CREATE ROLE topk_rls;
            GRANT USAGE ON SCHEMA public,plumb TO topk_rls;
            GRANT SELECT ON topk_docs TO topk_rls;
            ALTER TABLE topk_docs ENABLE ROW LEVEL SECURITY;
            CREATE POLICY topk_visible ON topk_docs TO topk_rls USING (id<=3);
            SET LOCAL ROLE topk_rls",
        )
        .unwrap();
        assert_reference("beer OR wine", 10);
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM plumb.top_k('topk_docs_idx','beer')")
                .unwrap(),
            Some(3)
        );
        Spi::run("RESET ROLE").unwrap();
        assert_reference("beer OR wine", 10);
    }

    fn direct_score(document: &str, heap: &str, index: &str, mode: i32) -> String {
        // Fixed test identifiers/literals only.
        format!(
            "SELECT plumb.score_bound('{document}','beer OR wine',
            '{heap}'::regclass::oid::int,'{index}'::regclass::oid::int,
            {mode},NULL,NULL,NULL,NULL,NULL)"
        )
    }

    fn score_invoker(grant: &str) {
        Spi::run(&format!(
            "CREATE ROLE score_invoker;
            GRANT USAGE ON SCHEMA public,plumb TO score_invoker;
            {grant}; SET LOCAL ROLE score_invoker"
        ))
        .unwrap();
    }

    #[pg_test(error = "permission denied for table topk_docs")]
    fn score_bound_direct_requires_select() {
        fixture("heap");
        score_invoker("");
        Spi::run(&direct_score("beer", "topk_docs", "topk_docs_idx", 1)).unwrap();
    }

    #[pg_test(error = "permission denied for table topk_docs")]
    fn score_bound_direct_rejects_wrong_column_privilege() {
        fixture("heap");
        score_invoker("GRANT SELECT(id) ON topk_docs TO score_invoker");
        Spi::run(&direct_score("beer", "topk_docs", "topk_docs_idx", 1)).unwrap();
    }

    #[pg_test(error = "permission denied for table topk_docs")]
    fn score_bound_direct_checks_partial_predicate_column_privilege() {
        fixture("heap");
        Spi::run(
            "CREATE INDEX score_partial ON topk_docs USING plumb(lower(body))
            WITH(storage='heap') WHERE id>0",
        )
        .unwrap();
        // Reading the expression is allowed, but reading its corpus predicate
        // is not. The normal invoker SELECT must check both referenced columns.
        score_invoker("GRANT SELECT(body) ON topk_docs TO score_invoker");
        Spi::run(&direct_score("beer", "topk_docs", "score_partial", 1)).unwrap();
    }

    #[pg_test(error = "plumb score index no longer belongs to the scored relation")]
    fn score_bound_direct_rejects_forged_heap_index_pair() {
        fixture("heap");
        Spi::run("CREATE TABLE score_other(body text)").unwrap();
        score_invoker("GRANT SELECT ON score_other TO score_invoker");
        Spi::run(&direct_score("beer", "score_other", "topk_docs_idx", 1)).unwrap();
    }

    #[pg_test(error = "plumb score requires a valid plumb index")]
    fn score_bound_direct_rejects_other_am_before_reading_options() {
        fixture("heap");
        Spi::run("CREATE INDEX score_btree ON topk_docs(body) WITH(fillfactor=50)").unwrap();
        score_invoker("GRANT SELECT ON topk_docs TO score_invoker");
        Spi::run(&direct_score("beer", "topk_docs", "score_btree", 1)).unwrap();
    }

    #[pg_test(error = "plumb score requires a valid plumb index")]
    fn score_bound_direct_rejects_table_as_index() {
        fixture("heap");
        score_invoker("GRANT SELECT ON topk_docs TO score_invoker");
        Spi::run(&direct_score("beer", "topk_docs", "topk_docs", 1)).unwrap();
    }

    #[pg_test(error = "plumb score mode must be between 0 and 3")]
    fn score_bound_direct_rejects_invalid_mode() {
        fixture("heap");
        score_invoker("GRANT SELECT ON topk_docs TO score_invoker");
        Spi::run(&direct_score("beer", "topk_docs", "topk_docs_idx", 4)).unwrap();
    }

    #[pg_test(error = "dense_ratio must be finite and non-negative")]
    fn score_bound_direct_rejects_invalid_dense_ratio() {
        fixture("heap");
        score_invoker("GRANT SELECT ON topk_docs TO score_invoker");
        Spi::run(
            "SELECT plumb.score_bound('beer','beer','topk_docs'::regclass::oid::int,
            'topk_docs_idx'::regclass::oid::int,0,'NaN'::real,NULL,NULL,NULL,NULL)",
        )
        .unwrap();
    }

    #[pg_test(error = "plumb score parameters: invalid BM25 parameters")]
    fn score_bound_direct_rejects_invalid_bm25_bounds() {
        fixture("heap");
        score_invoker("GRANT SELECT ON topk_docs TO score_invoker");
        Spi::run(
            "SELECT plumb.score_bound('beer','beer','topk_docs'::regclass::oid::int,
            'topk_docs_idx'::regclass::oid::int,1,NULL,10001,NULL,NULL,NULL)",
        )
        .unwrap();
    }

    #[pg_test]
    fn score_bound_direct_column_select_is_invoker_and_public() {
        fixture("heap");
        let expected =
            Spi::get_one::<f32>(&direct_score("beer", "topk_docs", "topk_docs_idx", 1)).unwrap();
        score_invoker("GRANT SELECT(body) ON topk_docs TO score_invoker");
        assert_eq!(Spi::get_one::<bool>("SELECT bool_and(NOT prosecdef AND proparallel='u')
            FROM pg_catalog.pg_proc WHERE oid=
            'plumb.score_bound(text,text,integer,integer,integer,real,real,real,text[],text[])'::regprocedure").unwrap(), Some(true));
        // Same permissions as a native body SELECT; helper emits no CTIDs.
        Spi::run("SELECT body FROM topk_docs LIMIT 0").unwrap();
        let actual =
            Spi::get_one::<f32>(&direct_score("beer", "topk_docs", "topk_docs_idx", 1)).unwrap();
        assert_eq!(actual.map(f32::to_bits), expected.map(f32::to_bits));
    }

    #[pg_test]
    fn score_bound_direct_rls_cannot_probe_hidden_document_or_reuse_policy_context() {
        fixture("heap");
        // Prime the privileged cache, then switch to a policy-limited caller.
        assert!(
            Spi::get_one::<f32>(&direct_score(
                "wine cheese",
                "topk_docs",
                "topk_docs_idx",
                1
            ))
            .unwrap()
            .unwrap()
                > 0.0
        );
        Spi::run(
            "ALTER TABLE topk_docs ENABLE ROW LEVEL SECURITY;
            CREATE POLICY score_visible ON topk_docs USING
                (id <= pg_catalog.current_setting('plumb_test.visible_id')::integer);
            SET LOCAL plumb_test.visible_id='3'",
        )
        .unwrap();
        score_invoker("GRANT SELECT ON topk_docs TO score_invoker");
        assert_reference("beer OR wine", 10);
        assert_eq!(
            Spi::get_one::<f32>(&direct_score(
                "wine cheese",
                "topk_docs",
                "topk_docs_idx",
                1
            ))
            .unwrap(),
            Some(0.0)
        );
        // Change policy input inside ONE statement, after a materialized first
        // score. XID, snapshot, command, and user are unchanged: an RLS corpus
        // cached on only those fields would leak the first visible result.
        assert_eq!(
            Spi::get_one::<f32>(
                "WITH first AS MATERIALIZED (
                SELECT plumb.score_bound('beer','beer OR wine',
                    'topk_docs'::regclass::oid::int,'topk_docs_idx'::regclass::oid::int,
                    1,NULL,NULL,NULL,NULL,NULL) AS score),
             changed AS MATERIALIZED (
                SELECT pg_catalog.set_config('plumb_test.visible_id','0',true) AS setting
                FROM first WHERE score>0)
             SELECT plumb.score_bound('beer','beer OR wine',
                 'topk_docs'::regclass::oid::int,'topk_docs_idx'::regclass::oid::int,
                 1,NULL,NULL,NULL,NULL,NULL) FROM changed WHERE setting='0'"
            )
            .unwrap(),
            Some(0.0)
        );
        assert_eq!(
            Spi::get_one::<f32>(&direct_score("beer", "topk_docs", "topk_docs_idx", 1)).unwrap(),
            Some(0.0)
        );
        assert_eq!(
            Spi::get_one::<f32>(&direct_score("beer", "topk_docs", "topk_docs_idx", 3)).unwrap(),
            Some(0.0)
        );
    }

    #[pg_test(
        error = "query would be affected by row-level security policy for table \"topk_docs\""
    )]
    fn score_bound_direct_row_security_off_does_not_bypass_rls() {
        fixture("heap");
        Spi::run(
            "ALTER TABLE topk_docs ENABLE ROW LEVEL SECURITY;
            CREATE POLICY score_visible ON topk_docs USING (id<=3)",
        )
        .unwrap();
        score_invoker("GRANT SELECT ON topk_docs TO score_invoker");
        Spi::run("SET LOCAL row_security=off").unwrap();
        Spi::run(&direct_score("beer", "topk_docs", "topk_docs_idx", 1)).unwrap();
    }

    fn hostile_operator() {
        Spi::run(
            "CREATE SCHEMA topk_hostile;
            CREATE FUNCTION topk_hostile.never_le(integer,integer) RETURNS boolean
            LANGUAGE sql IMMUTABLE AS 'SELECT false';
            CREATE OPERATOR topk_hostile.<=
                (LEFTARG=integer, RIGHTARG=integer, FUNCTION=topk_hostile.never_le);
            SET LOCAL search_path=topk_hostile,pg_catalog,public",
        )
        .unwrap();
        // Prove this is a same-signature operator that actually wins lookup.
        assert_eq!(
            Spi::get_one::<bool>("SELECT 1 <= 262144").unwrap(),
            Some(false)
        );
    }

    #[pg_test]
    fn topk_internal_byte_guard_ignores_shadow_operator() {
        fixture("heap");
        let expected = Spi::get_one::<Vec<String>>(
            "SELECT array_agg(ctid::text || ':' ||
            pg_catalog.encode(pg_catalog.float4send(score),'hex') ORDER BY ord)
            FROM plumb.top_k('public.topk_docs_idx','beer',10) WITH ORDINALITY t(ctid,score,ord)",
        )
        .unwrap();
        hostile_operator();
        let actual = Spi::get_one::<Vec<String>>(
            "SELECT array_agg(ctid::text || ':' ||
            pg_catalog.encode(pg_catalog.float4send(score),'hex') ORDER BY ord)
            FROM plumb.top_k('public.topk_docs_idx','beer',10) WITH ORDINALITY t(ctid,score,ord)",
        )
        .unwrap();
        assert!(expected.is_some());
        assert_eq!(actual, expected);
    }

    #[pg_test(error = "plumb.top_k document exceeds 262144-byte budget")]
    fn topk_shadow_true_operator_cannot_bypass_byte_budget() {
        fixture("heap");
        hostile_operator();
        Spi::run(
            "CREATE OR REPLACE FUNCTION topk_hostile.never_le(integer,integer)
            RETURNS boolean LANGUAGE sql IMMUTABLE AS 'SELECT true';
            INSERT INTO public.topk_docs VALUES(20,repeat('a',262145))",
        )
        .unwrap();
        Spi::run("SELECT * FROM plumb.top_k('public.topk_docs_idx','beer')").unwrap();
    }

    fn swap_corpus_schema_names() {
        Spi::run(
            "ALTER SCHEMA topk_original RENAME TO topk_moved;
            ALTER SCHEMA topk_replacement RENAME TO topk_original",
        )
        .unwrap();
    }

    fn schema_swap_fixture(nonempty_replacement: bool) {
        Spi::run(
            "CREATE SCHEMA topk_original; CREATE SCHEMA topk_replacement;
            CREATE TABLE topk_original.docs(body text);
            CREATE TABLE topk_replacement.docs(body text);
            INSERT INTO topk_original.docs VALUES('beer');
            CREATE INDEX docs_idx ON topk_original.docs USING plumb(body)",
        )
        .unwrap();
        if nonempty_replacement {
            Spi::run("INSERT INTO topk_replacement.docs VALUES('beer beer beer')").unwrap();
        }
        crate::score::BEFORE_CORPUS_PREPARE.with(|hook| hook.set(Some(swap_corpus_schema_names)));
    }

    // Deterministic name-capture/prepare gap. Even an empty replacement must
    // ERROR, not silently return an empty result (tableoid WHERE cannot pass).
    #[pg_test(error = "plumb corpus relation identity changed while planning; retry")]
    fn topk_schema_swap_rejects_empty_replacement_before_execution() {
        schema_swap_fixture(false);
        Spi::run("SELECT * FROM plumb.top_k('topk_original.docs_idx','beer')").unwrap();
    }

    #[pg_test(error = "plumb corpus relation identity changed while planning; retry")]
    fn topk_schema_swap_rejects_nonempty_replacement_before_execution() {
        schema_swap_fixture(true);
        Spi::run("SELECT * FROM plumb.top_k('topk_original.docs_idx','beer')").unwrap();
    }

    #[pg_test(error = "plumb corpus relation identity changed while planning; retry")]
    fn full_score_schema_swap_rejects_empty_replacement_before_execution() {
        schema_swap_fixture(false);
        Spi::run(
            "SET LOCAL enable_seqscan=off;
            SELECT plumb.full_score(ctid) FROM topk_original.docs
            WHERE body OPERATOR(pg_catalog.~~>) 'beer'",
        )
        .unwrap();
    }

    #[pg_test]
    fn topk_schema_swap_after_plan_keeps_original_oid_and_float_bits() {
        schema_swap_fixture(true);
        crate::score::BEFORE_CORPUS_PREPARE.with(|hook| hook.set(None));
        let sql = "SELECT array_agg(ctid::text || ':' ||
            pg_catalog.encode(pg_catalog.float4send(score),'hex') ORDER BY score DESC,ctid)
            FROM plumb.top_k('topk_original.docs_idx','beer')";
        let expected = Spi::get_one::<Vec<String>>(sql).unwrap();
        crate::score::AFTER_CORPUS_PLAN.with(|hook| hook.set(Some(swap_corpus_schema_names)));
        let actual = Spi::get_one::<Vec<String>>(sql).unwrap();
        assert!(expected.is_some());
        assert_eq!(
            actual, expected,
            "validated plan must never be re-resolved by name"
        );
        assert!(crate::score::AFTER_CORPUS_PLAN.with(|hook| hook.get().is_none()));
    }

    #[pg_test(error = "plumb corpus relation identity changed while planning; retry")]
    fn topk_schema_swap_at_k_zero_still_rejects_empty_replacement() {
        schema_swap_fixture(false);
        Spi::run("SELECT * FROM plumb.top_k('topk_original.docs_idx','beer',0)").unwrap();
    }

    fn mutate_after_score_snapshot_capture() {
        Spi::run(
            "UPDATE topk_docs SET body='wine changed' WHERE id=2;
            DELETE FROM topk_docs WHERE id=3;
            INSERT INTO topk_docs VALUES(50,'beer wine wine wine')",
        )
        .unwrap();
    }

    #[pg_test]
    fn full_score_corpus_keeps_outer_snapshot_after_nested_write() {
        fixture("heap");
        // Keep scoring and its matching qualifier in the same planned query
        // level. The LIMIT fence prevents pull-up into the outer aggregate.
        let sql = "SELECT array_agg(ctid::text || ':' ||
            pg_catalog.encode(pg_catalog.float4send(score),'hex') ORDER BY score DESC,ctid)
            FROM (SELECT ctid,plumb.full_score(ctid) score FROM topk_docs
            WHERE body OPERATOR(pg_catalog.~~>) 'beer OR wine'
            ORDER BY score DESC,ctid LIMIT 10) s";
        let expected = Spi::get_one::<Vec<String>>(
            "SELECT array_agg(ctid::text || ':' ||
            pg_catalog.encode(pg_catalog.float4send(score),'hex') ORDER BY score DESC,ctid)
            FROM plumb.top_k('topk_docs_idx','beer OR wine',10)",
        )
        .unwrap();
        crate::score::BEFORE_CORPUS_READ
            .with(|hook| hook.set(Some(mutate_after_score_snapshot_capture)));
        let actual = Spi::get_one::<Vec<String>>(sql).unwrap();
        assert_eq!(
            actual, expected,
            "corpus must see pre-write outer snapshot including old document"
        );
        assert!(
            crate::score::BEFORE_CORPUS_READ.with(|hook| hook.get().is_none()),
            "hook must run on a cache miss"
        );
        // A later statement must not reuse the old corpus, and both ranking
        // paths must now agree on the committed-to-this-transaction writes.
        let later = Spi::get_one::<Vec<String>>(sql).unwrap();
        assert_ne!(later, expected);
        assert_reference("beer OR wine", 10);
    }

    #[pg_test]
    fn full_score_preserves_expression_and_partial_corpus_semantics() {
        Spi::run("CREATE TABLE score_expr_partial(id int,body text);
            INSERT INTO score_expr_partial VALUES
                (0,'BEER BEER BEER'),(1,'BEER WINE'),(2,'Beer'),(3,''),(4,NULL),(5,'Wine Wine');
            CREATE INDEX score_expr_partial_idx ON score_expr_partial
                USING plumb(lower(body)) WITH(storage='heap') WHERE id>0;
            CREATE TABLE score_expr_control AS
                SELECT id,lower(body) body FROM score_expr_partial WHERE id>0;
            CREATE INDEX score_expr_control_idx ON score_expr_control USING plumb(body) WITH(storage='heap');
            SET LOCAL enable_seqscan=off").unwrap();
        let actual = Spi::get_one::<Vec<String>>(
            "SELECT array_agg(id::text || ':' ||
            pg_catalog.encode(pg_catalog.float4send(score),'hex') ORDER BY id)
            FROM (SELECT id,plumb.full_score(ctid) score FROM score_expr_partial
                WHERE id>0 AND lower(body) OPERATOR(pg_catalog.~~>) 'beer OR wine'
                OFFSET 0) s",
        )
        .unwrap();
        let expected = Spi::get_one::<Vec<String>>(
            "SELECT array_agg(id::text || ':' ||
            pg_catalog.encode(pg_catalog.float4send(score),'hex') ORDER BY id)
            FROM (SELECT id,plumb.full_score(ctid) score FROM score_expr_control
                WHERE body OPERATOR(pg_catalog.~~>) 'beer OR wine' OFFSET 0) s",
        )
        .unwrap();
        assert!(expected.is_some());
        assert_eq!(actual, expected);
    }

    #[pg_test]
    fn topk_empty_corpus() {
        Spi::run("CREATE TABLE topk_empty(body text); CREATE INDEX topk_empty_idx ON topk_empty USING plumb(body)").unwrap();
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM plumb.top_k('topk_empty_idx','beer')")
                .unwrap(),
            Some(0)
        );
        Spi::run("INSERT INTO topk_empty VALUES(NULL),('')").unwrap();
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM plumb.top_k('topk_empty_idx','beer')")
                .unwrap(),
            Some(0)
        );
    }
}
