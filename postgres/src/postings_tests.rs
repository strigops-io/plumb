// SPDX-License-Identifier: AGPL-3.0-or-later
// Added by Plumb contributors on 2026-09-20: experimental postings AM regressions.
// Modified by Plumb contributors on 2026-09-20: default transition, batched build, and merge regressions.
#[pgrx::pg_schema]
mod tests {
    use pgrx::prelude::*;

    // Planner may inspect a candidate it will not use. Application CRC errors
    // fall back, but an actual index scan of the same relation remains fail-closed.
    fn planner_corruption_fixture(metapage: bool) {
        setup();
        Spi::run("ANALYZE p_docs; SET LOCAL enable_seqscan=on; SET LOCAL enable_bitmapscan=off")
            .unwrap();
        let oid = Spi::get_one::<pg_sys::Oid>("SELECT 'p_docs_idx'::regclass::oid")
            .unwrap()
            .unwrap();
        unsafe {
            let index = pgrx::PgRelation::with_lock(oid, pg_sys::AccessShareLock as _);
            crate::storage::damage_crc_for_test(index.as_ptr(), metapage);
        }
        let plan = Spi::get_one::<pgrx::Json>(
            "EXPLAIN (FORMAT JSON) SELECT * FROM p_docs WHERE body ~~> 'beer'",
        )
        .unwrap()
        .unwrap()
        .0;
        assert_eq!(plan[0]["Plan"]["Node Type"], "Seq Scan");
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM p_docs WHERE body ~~> 'beer'").unwrap(),
            Some(2)
        );
        Spi::run("SET LOCAL enable_bitmapscan=on; SET LOCAL enable_seqscan=off").unwrap();
        // EXPLAIN alone still succeeds while costing the corrupted index.
        let plan = Spi::get_one::<pgrx::Json>(
            "EXPLAIN (FORMAT JSON) SELECT * FROM p_docs WHERE body ~~> 'beer'",
        )
        .unwrap()
        .unwrap()
        .0;
        assert_eq!(
            plan[0]["Plan"]["Plans"][0]["Node Type"],
            "Bitmap Index Scan"
        );
        let _ = Spi::get_one::<i64>("SELECT count(*) FROM p_docs WHERE body ~~> 'beer'");
    }

    #[pg_test(
        error = "plumb postings storage is corrupt or unsupported: invalid metapage magic/version/length/checksum"
    )]
    fn postings_planner_metapage_corruption_fallback_executor_errors() {
        planner_corruption_fixture(true);
    }

    #[pg_test(
        error = "plumb postings storage is corrupt or unsupported: invalid segment chunk length/checksum"
    )]
    fn postings_planner_chunk_corruption_fallback_executor_errors() {
        planner_corruption_fixture(false);
    }

    #[pg_test]
    fn postings_planner_segment_budget_fallback_is_read_only() {
        Spi::run("CREATE TABLE p_budget(body text); CREATE INDEX p_budget_idx ON p_budget USING plumb(body)").unwrap();
        // Each command appends a separate one-term segment.
        for _ in 0..33 {
            Spi::run("INSERT INTO p_budget VALUES ('beer')").unwrap();
        }
        Spi::run("ANALYZE p_budget; SET LOCAL enable_seqscan=off").unwrap();
        let before = Spi::get_one::<pgrx::JsonB>("SELECT plumb.index_stats('p_budget_idx')")
            .unwrap()
            .unwrap()
            .0;
        assert_eq!(before["segments"], 33);
        let plan = Spi::get_one::<pgrx::Json>(
            "EXPLAIN (FORMAT JSON) SELECT * FROM p_budget WHERE body ~~> 'beer'",
        )
        .unwrap()
        .unwrap()
        .0;
        let bitmap = &plan[0]["Plan"]["Plans"][0];
        assert_eq!(bitmap["Node Type"], "Bitmap Index Scan");
        assert_eq!(bitmap["Plan Rows"], 3); // fallback selectivity .1, not physical count 33
        let after = Spi::get_one::<pgrx::JsonB>("SELECT plumb.index_stats('p_budget_idx')")
            .unwrap()
            .unwrap()
            .0;
        assert_eq!(before, after);
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM p_budget WHERE body ~~> 'beer'").unwrap(),
            Some(33)
        );
    }

    #[pg_test]
    fn postings_planner_directory_and_legacy_byte_budget_fallback() {
        use plumb_postings::{Ctid, Postings, codec};
        for version in [1, 2] {
            Spi::run("CREATE TABLE p_byte_budget(body text); INSERT INTO p_byte_budget SELECT 'beer' FROM generate_series(1,100); CREATE INDEX p_byte_budget_idx ON p_byte_budget USING plumb(body); ANALYZE p_byte_budget; SET LOCAL enable_seqscan=off").unwrap();
            let bytes = if version == 2 {
                let mut builder = crate::term_index::Builder::default();
                for n in 0..12000 {
                    builder
                        .add(&format!("word{n:06}"), Ctid::new(0, 1).unwrap())
                        .unwrap();
                }
                let bytes = builder.finish().unwrap();
                let directory_len = u32::from_le_bytes(bytes[20..24].try_into().unwrap());
                assert!(directory_len > 256 * 1024);
                bytes
            } else {
                // Valid v1 singleton dictionary with a large, sparse codec frame.
                // This unqueried term is stale physical history, not heap truth.
                let frame = codec::encode(&Postings::from_ctids(
                    (0..10000).map(|n| Ctid::new(n * 65536, 1).unwrap()),
                ))
                .unwrap();
                let term = b"unqueried";
                let total = 20 + 8 + term.len() + frame.len() + 4;
                assert!(total > 256 * 1024);
                let mut bytes = b"PLMBTRM\0".to_vec();
                for n in [1u32, 1, total as u32, term.len() as u32, frame.len() as u32] {
                    bytes.extend_from_slice(&n.to_le_bytes());
                }
                bytes.extend_from_slice(term);
                bytes.extend_from_slice(&frame);
                let mut crc = plumb_postings::accel::Crc32c::new();
                crc.update(&bytes);
                bytes.extend_from_slice(&crc.finish().to_le_bytes());
                bytes
            };
            let oid = Spi::get_one::<pg_sys::Oid>("SELECT 'p_byte_budget_idx'::regclass::oid")
                .unwrap()
                .unwrap();
            unsafe {
                let index = pgrx::PgRelation::with_lock(oid, pg_sys::AccessExclusiveLock as _);
                crate::storage::append(index.as_ptr(), &bytes);
                let q = crate::term_index::query("beer").unwrap().unwrap();
                let mut counts = std::collections::BTreeMap::new();
                crate::term_index::collect_terms(&q, &mut counts);
                let error = crate::storage::visit_ranges_fallible(
                    index.as_ptr(),
                    32,
                    256 * 1024,
                    |reader| {
                        crate::term_index::load_directory_fallible(reader.len(), |at, n| {
                            reader.read_fallible(at, n)
                        })
                        .map(|_| ())
                    },
                )
                .unwrap_err();
                assert!(matches!(error, crate::storage::ReadError::Budget(_)));
            }
            let before =
                Spi::get_one::<pgrx::JsonB>("SELECT plumb.index_stats('p_byte_budget_idx')")
                    .unwrap()
                    .unwrap()
                    .0;
            let plan = Spi::get_one::<pgrx::Json>(
                "EXPLAIN (FORMAT JSON) SELECT * FROM p_byte_budget WHERE body ~~> 'beer'",
            )
            .unwrap()
            .unwrap()
            .0;
            assert_eq!(
                plan[0]["Plan"]["Plans"][0]["Node Type"],
                "Bitmap Index Scan"
            );
            assert_eq!(plan[0]["Plan"]["Plans"][0]["Plan Rows"], 10);
            let after =
                Spi::get_one::<pgrx::JsonB>("SELECT plumb.index_stats('p_byte_budget_idx')")
                    .unwrap()
                    .unwrap()
                    .0;
            assert_eq!(before, after);
            assert_eq!(
                Spi::get_one::<i64>("SELECT count(*) FROM p_byte_budget WHERE body ~~> 'beer'")
                    .unwrap(),
                Some(100)
            );
            Spi::run("DROP TABLE p_byte_budget").unwrap();
        }
    }

    // Added by Plumb contributors on 2026-09-20: v2 selective IO and advisory stats.
    #[pg_test]
    fn postings_v2_selective_absent_rare_buffer_io() {
        Spi::run("CREATE TABLE p_selective(id int, body text);
            INSERT INTO p_selective SELECT n, CASE WHEN n=100000 THEN 'common zzrare' ELSE 'common' END FROM generate_series(1,100000) n;
            CREATE INDEX p_selective_idx ON p_selective USING plumb(body);
            ANALYZE p_selective;
            SET LOCAL enable_seqscan=off;").unwrap();
        let blocks = Spi::get_one::<i64>(
            "SELECT (plumb.index_stats('p_selective_idx')->>'relation_blocks')::bigint",
        )
        .unwrap()
        .unwrap();
        assert!(blocks > 20);
        for (text, expected) in [
            ("absent", 0),
            ("zzrare", 1),
            ("zzrare OR absent", 1),
            ("common AND zzrare", 1),
        ] {
            let plan = Spi::get_one::<pgrx::Json>(&format!(
                "EXPLAIN (ANALYZE,BUFFERS,FORMAT JSON) SELECT * FROM p_selective WHERE body ~~> {}",
                pgrx::spi::quote_literal(text)
            ))
            .unwrap()
            .unwrap()
            .0;
            let bitmap = &plan[0]["Plan"]["Plans"][0];
            assert_eq!(bitmap["Node Type"], "Bitmap Index Scan");
            // PG18 emits actual row counts as JSON floats; compare exact numeric values.
            assert_eq!(
                plan[0]["Plan"]["Actual Rows"].as_f64(),
                Some(f64::from(expected))
            );
            if !text.starts_with("common") {
                let buffers = bitmap["Shared Hit Blocks"].as_i64().unwrap_or(0)
                    + bitmap["Shared Read Blocks"].as_i64().unwrap_or(0);
                assert!(
                    buffers < blocks / 2,
                    "{text}: {buffers} buffers vs {blocks} full relation blocks"
                );
            }
        }
        let stats = Spi::get_one::<pgrx::JsonB>(
            "SELECT plumb.term_stats('p_selective_idx','zzrare OR absent')",
        )
        .unwrap()
        .unwrap()
        .0;
        assert_eq!(stats["physical_count_upper_bounds"]["zzrare"], 1);
        assert_eq!(stats["physical_count_upper_bounds"]["absent"], 0);
        assert_eq!(stats["term_versions"]["v1"], 0);
        assert!(stats["term_versions"]["v2"].as_i64().unwrap() > 0);
        assert!(stats["read_payload_bytes"].as_i64().unwrap() < blocks * 8192 / 2);
    }

    #[pg_test]
    fn postings_term_stats_stale_after_delete_abort_and_default_mutations() {
        setup();
        Spi::run("ANALYZE p_docs; DELETE FROM p_docs WHERE id=1;
            DO $$ BEGIN BEGIN INSERT INTO p_docs VALUES(99,'aborted beer');
            RAISE EXCEPTION 'rollback stats test'; EXCEPTION WHEN raise_exception THEN NULL; END; END $$;
            INSERT INTO p_docs VALUES(6,'craft beer');").unwrap();
        let stats =
            Spi::get_one::<pgrx::JsonB>("SELECT plumb.term_stats('p_docs_idx','beer OR aborted')")
                .unwrap()
                .unwrap()
                .0;
        assert_eq!(stats["physical_count_upper_bounds"]["beer"], 4);
        assert_eq!(stats["physical_count_upper_bounds"]["aborted"], 1);
        assert_eq!(stats["exact_visible_df"], false);
        assert_eq!(stats["stale_history"], true);
        assert_eq!(ids("beer"), vec![3, 6]);
        assert!(ids("aborted").is_empty());
        Spi::run("SELECT plumb.merge_index('p_docs_idx')").unwrap();
        let after =
            Spi::get_one::<pgrx::JsonB>("SELECT plumb.term_stats('p_docs_idx','beer OR aborted')")
                .unwrap()
                .unwrap()
                .0;
        assert_eq!(after["segments"], 1);
        assert_eq!(
            after["physical_count_upper_bounds"],
            stats["physical_count_upper_bounds"]
        );
    }

    #[pg_test(error = "must be owner of index to inspect plumb term statistics")]
    fn postings_term_stats_requires_owner() {
        setup();
        Spi::run("CREATE ROLE p_stats_nonowner; GRANT USAGE ON SCHEMA plumb TO p_stats_nonowner; SET LOCAL ROLE p_stats_nonowner").unwrap();
        let _ = Spi::get_one::<pgrx::JsonB>("SELECT plumb.term_stats('p_docs_idx','beer')");
    }

    #[pg_test(error = "term_stats supports only positive term/AND/OR/boost queries")]
    fn postings_term_stats_rejects_unsupported_shape() {
        setup();
        let _ = Spi::get_one::<pgrx::JsonB>("SELECT plumb.term_stats('p_docs_idx','bee*')");
    }

    fn ids(query: &str) -> Vec<i32> {
        Spi::get_one::<Vec<i32>>(&format!(
            "SELECT coalesce(array_agg(id ORDER BY id), '{{}}'::int[]) FROM p_docs WHERE body ~~> {}",
            pgrx::spi::quote_literal(query)
        )).unwrap().unwrap()
    }
    fn setup() {
        Spi::run("CREATE TABLE p_docs(id int, body text) WITH (fillfactor=60);
            INSERT INTO p_docs VALUES (1,'craft beer'),(2,'wine'),(3,'beer festival'),(4,NULL),(5,'...');
            CREATE INDEX p_docs_idx ON p_docs USING plumb(body);
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
        assert_eq!(
            plan[0]["Plan"]["Plans"][0]["Actual Rows"].as_f64(),
            Some(2.0)
        );
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
            CREATE INDEX p_legacy_idx ON p_legacy USING plumb(body) WITH(storage='heap');
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
            CREATE INDEX p_legacy_write_idx ON p_legacy_write USING plumb(body) WITH(storage='heap');
            ALTER INDEX p_legacy_write_idx SET(storage='postings_v1');
            INSERT INTO p_legacy_write VALUES('wine');",
        )
        .unwrap();
    }

    #[pg_test(
        error = "postings_v1 supports only the default analyzer; restore default tokenizer options and REINDEX"
    )]
    fn postings_rejects_nondefault_analyzer_build() {
        Spi::run("CREATE TABLE p_analyzer(body text); CREATE INDEX p_analyzer_idx ON p_analyzer USING plumb(body) WITH(tokenizer='whitespace');").unwrap();
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
        Spi::run("CREATE TEMP TABLE p_temp(body text); CREATE INDEX p_temp_idx ON p_temp USING plumb(body);").unwrap();
    }

    #[pg_test(
        error = "postings_v1 requires a permanent heap; temporary/unlogged relations are unsupported"
    )]
    fn postings_rejects_unlogged_heap() {
        Spi::run("CREATE UNLOGGED TABLE p_unlogged(body text); CREATE INDEX p_unlogged_idx ON p_unlogged USING plumb(body);").unwrap();
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
                USING plumb(body p_heap_inequality_ops) WITH(storage='heap');
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

    #[pg_test]
    fn default_storage_with_unrelated_reloptions_is_postings() {
        setup();
        Spi::run(
            "DROP INDEX p_docs_idx;
            CREATE INDEX p_docs_idx ON p_docs USING plumb(body)
                WITH(initial_segment_count=8,k1=2,b=0.5);",
        )
        .unwrap();
        assert_eq!(ids("beer"), vec![1, 3]);
        assert_eq!(segments(), 1);
        assert_eq!(
            Spi::get_one::<i32>("SELECT (plumb.index_stats('p_docs_idx')->>'format_version')::int")
                .unwrap(),
            Some(1)
        );
        let plan = Spi::get_one::<pgrx::Json>(
            "EXPLAIN (ANALYZE,FORMAT JSON) SELECT * FROM p_docs WHERE body ~~> 'beer'",
        )
        .unwrap()
        .unwrap()
        .0;
        assert_eq!(plan[0]["Plan"]["Lossy Heap Blocks"], 0);
        assert_eq!(
            plan[0]["Plan"]["Plans"][0]["Actual Rows"].as_f64(),
            Some(2.0)
        );
    }

    #[pg_test]
    fn explicit_heap_keeps_temporary_and_analyzer_compatibility() {
        Spi::run(
            "CREATE TEMP TABLE p_heap_compat(body text);
            INSERT INTO p_heap_compat VALUES ('beer'),('wine');
            CREATE INDEX p_heap_compat_idx ON p_heap_compat USING plumb(body)
                WITH(storage='heap',tokenizer='whitespace');
            INSERT INTO p_heap_compat VALUES ('craft beer');
            SET LOCAL enable_seqscan=off;",
        )
        .unwrap();
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM p_heap_compat WHERE body ~~> 'beer'")
                .unwrap(),
            Some(2)
        );
        assert_eq!(
            Spi::get_one::<String>("SELECT plumb.index_stats('p_heap_compat_idx')->>'storage'")
                .unwrap()
                .as_deref(),
            Some("heap")
        );
    }

    #[pg_test(
        error = "postings_v1 metadata is missing; ALTER INDEX cannot convert a heap-baseline index; REINDEX is required"
    )]
    fn default_rejects_legacy_zero_page_scan_without_storage_option() {
        Spi::run(
            "CREATE TABLE p_old_default(body text);
            INSERT INTO p_old_default VALUES ('beer');
            CREATE INDEX p_old_default_idx ON p_old_default USING plumb(body) WITH(storage='heap');
            ALTER INDEX p_old_default_idx RESET(storage);
            SET LOCAL enable_seqscan=off;",
        )
        .unwrap();
        let _ = Spi::get_one::<i64>("SELECT count(*) FROM p_old_default WHERE body ~~> 'beer'");
    }

    #[pg_test(
        error = "postings_v1 metadata is missing; ALTER INDEX cannot convert a heap-baseline index; REINDEX is required"
    )]
    fn default_rejects_legacy_zero_page_insert_without_storage_option() {
        Spi::run("CREATE TABLE p_old_default_write(body text);
            CREATE INDEX p_old_default_write_idx ON p_old_default_write USING plumb(body) WITH(storage='heap');
            ALTER INDEX p_old_default_write_idx RESET(storage);
            INSERT INTO p_old_default_write VALUES ('beer');").unwrap();
    }

    #[pg_test]
    fn reindex_migrates_legacy_zero_page_to_default() {
        Spi::run(
            "CREATE TABLE p_old_reindex(body text);
            INSERT INTO p_old_reindex VALUES ('beer');
            CREATE INDEX p_old_reindex_idx ON p_old_reindex USING plumb(body) WITH(storage='heap');
            ALTER INDEX p_old_reindex_idx RESET(storage);
            REINDEX INDEX p_old_reindex_idx;
            INSERT INTO p_old_reindex VALUES ('craft beer');
            SET LOCAL enable_seqscan=off;",
        )
        .unwrap();
        assert_eq!(
            Spi::get_one::<i64>("SELECT count(*) FROM p_old_reindex WHERE body ~~> 'beer'")
                .unwrap(),
            Some(2)
        );
        assert_eq!(
            Spi::get_one::<String>("SELECT plumb.index_stats('p_old_reindex_idx')->>'storage'")
                .unwrap()
                .as_deref(),
            Some("postings_v1")
        );
    }

    fn merge() -> pgrx::JsonB {
        Spi::get_one::<pgrx::JsonB>("SELECT plumb.merge_index('p_docs_idx')")
            .unwrap()
            .unwrap()
    }

    #[pg_test]
    fn merge_keeps_regclass_sql_signature_without_oid_overload() {
        assert_eq!(
            Spi::get_one::<bool>(
                "SELECT p.proargtypes[0] = 'regclass'::regtype
                    AND p.prorettype = 'jsonb'::regtype AND p.proisstrict
                    AND p.provolatile = 'v' AND p.proparallel = 'u'
                    AND to_regprocedure('plumb.merge_index(oid)') IS NULL
                FROM pg_proc p WHERE p.oid = 'plumb.merge_index(regclass)'::regprocedure",
            )
            .unwrap(),
            Some(true)
        );
        setup();
        // Both the documented untyped-name call and an explicit regclass datum
        // must still resolve the one public signature.
        assert_eq!(merge().0["changed"], false);
        assert_eq!(
            Spi::get_one::<pgrx::JsonB>("SELECT plumb.merge_index('public.p_docs_idx'::regclass)",)
                .unwrap()
                .unwrap()
                .0["changed"],
            false
        );
        assert!(
            Spi::get_one::<pgrx::JsonB>("SELECT plumb.merge_index(NULL::regclass)")
                .unwrap()
                .is_none()
        );
    }

    #[pg_test]
    fn merge_rejects_dropped_index_oid_without_opening_replacement() {
        setup();
        Spi::run(
            "DO $$ DECLARE old_index regclass := 'p_docs_idx'::regclass;
            BEGIN
                DROP INDEX p_docs_idx;
                CREATE INDEX p_docs_idx ON p_docs USING plumb(body);
                BEGIN
                    PERFORM plumb.merge_index(old_index);
                EXCEPTION WHEN internal_error THEN
                    IF SQLERRM <> 'plumb.merge_index requires an index using the plumb access method'
                    THEN RAISE; END IF;
                    RETURN;
                END;
                RAISE EXCEPTION 'dropped index OID unexpectedly merged';
            END $$;",
        )
        .unwrap();
        assert_eq!(merge().0["changed"], false);
        assert_eq!(ids("beer"), vec![1, 3]);
    }

    #[pg_test]
    fn merge_changed_requires_owner_after_heap_first_open() {
        setup();
        Spi::run(
            "INSERT INTO p_docs VALUES (6,'beer');
            CREATE ROLE p_merge_lock_owner NOLOGIN;
            CREATE ROLE p_merge_lock_member NOLOGIN INHERIT;
            CREATE ROLE p_merge_lock_outsider NOLOGIN;
            GRANT p_merge_lock_owner TO p_merge_lock_member;
            GRANT USAGE ON SCHEMA plumb,public TO
                p_merge_lock_owner,p_merge_lock_member,p_merge_lock_outsider;
            GRANT EXECUTE ON FUNCTION plumb.merge_index(regclass) TO
                p_merge_lock_owner,p_merge_lock_member,p_merge_lock_outsider;
            ALTER TABLE p_docs OWNER TO p_merge_lock_owner;
            SET LOCAL ROLE p_merge_lock_outsider;
            DO $$ BEGIN
                BEGIN
                    PERFORM plumb.merge_index('public.p_docs_idx');
                EXCEPTION WHEN insufficient_privilege THEN
                    IF SQLERRM <> 'must be owner of index to merge plumb postings' THEN RAISE; END IF;
                    RETURN;
                END;
                RAISE EXCEPTION 'nonowner merge unexpectedly succeeded';
            END $$;
            RESET ROLE;",
        )
        .unwrap();
        assert!(segments() > 1);
        Spi::run("SET LOCAL ROLE p_merge_lock_member").unwrap();
        assert_eq!(merge().0["changed"], true);
        Spi::run(
            "RESET ROLE;
            INSERT INTO p_docs VALUES (7,'beer');
            SET LOCAL ROLE p_merge_lock_owner;",
        )
        .unwrap();
        assert_eq!(merge().0["changed"], true);
        Spi::run("RESET ROLE").unwrap();
        assert_eq!(segments(), 1);
        assert_eq!(ids("beer"), vec![1, 3, 6, 7]);
    }

    #[pg_test]
    fn merge_many_segments_preserves_results_and_continued_inserts() {
        setup();
        Spi::run(
            "INSERT INTO p_docs SELECT n, CASE WHEN n%2=0 THEN 'beer craft' ELSE 'wine' END
            FROM generate_series(6,45) n;
            UPDATE p_docs SET body='wine' WHERE id=1;
            DELETE FROM p_docs WHERE id=3;
            DO $$ BEGIN BEGIN
                INSERT INTO p_docs VALUES(999,'rollback beer');
                RAISE EXCEPTION 'rollback';
            EXCEPTION WHEN raise_exception THEN NULL; END; END $$;",
        )
        .unwrap();
        let queries = [
            "beer",
            "craft AND beer",
            "wine OR beer",
            "\"craft beer\"",
            "beer AND NOT wine",
            "*",
        ];
        let before: Vec<_> = queries.iter().map(|q| ids(q)).collect();
        assert!(segments() > 20);
        let result = merge().0;
        assert_eq!(result["changed"], true);
        assert!(result["before"]["segments"].as_u64().unwrap() > 20);
        assert_eq!(result["after"]["segments"], 1);
        assert_eq!(result["after"]["format_version"], 1);
        assert!(result["after"]["payload_bytes"].as_u64().unwrap() > 0);
        assert!(
            result["after"]["relation_blocks"].as_u64().unwrap()
                > result["before"]["relation_blocks"].as_u64().unwrap()
        );
        assert_eq!(segments(), 1);
        for (query, expected) in queries.iter().zip(before) {
            assert_eq!(ids(query), expected, "{query}");
        }
        assert_eq!(ids("rollback"), Vec::<i32>::new());
        Spi::run("INSERT INTO p_docs VALUES (1000,'beer newmarker')").unwrap();
        assert_eq!(segments(), 2);
        assert_eq!(ids("newmarker AND beer"), vec![1000]);
    }

    #[pg_test]
    fn merge_zero_and_one_segments_are_noops() {
        Spi::run(
            "CREATE TABLE p_docs(id int,body text);
            INSERT INTO p_docs VALUES (1,NULL),(2,'...');
            CREATE INDEX p_docs_idx ON p_docs USING plumb(body);",
        )
        .unwrap();
        let empty = merge().0;
        assert_eq!(empty["changed"], false);
        assert_eq!(empty["before"], empty["after"]);
        assert_eq!(empty["before"]["segments"], 0);
        assert_eq!(empty["before"]["relation_blocks"], 1);
        Spi::run("INSERT INTO p_docs VALUES (3,'beer')").unwrap();
        let one = merge().0;
        assert_eq!(one["changed"], false);
        assert_eq!(one["before"], one["after"]);
        assert_eq!(one["after"]["segments"], 1);
    }

    #[pg_test]
    fn merge_uses_persisted_identity_not_heap_reloption() {
        setup();
        Spi::run(
            "ALTER INDEX p_docs_idx SET(storage='heap');
            INSERT INTO p_docs VALUES (6,'beer');",
        )
        .unwrap();
        assert_eq!(merge().0["changed"], true);
        assert_eq!(ids("beer"), vec![1, 3, 6]);
        assert_eq!(segments(), 1);
    }

    #[pg_test(error = "plumb.merge_index requires an index using the plumb access method")]
    fn merge_rejects_table() {
        setup();
        let _ = Spi::get_one::<pgrx::JsonB>("SELECT plumb.merge_index('p_docs')");
    }

    #[pg_test(error = "plumb.merge_index requires an index using the plumb access method")]
    fn merge_rejects_other_access_methods() {
        Spi::run(
            "CREATE TABLE p_merge_wrong(id int);
            CREATE INDEX p_merge_wrong_idx ON p_merge_wrong(id);",
        )
        .unwrap();
        let _ = Spi::get_one::<pgrx::JsonB>("SELECT plumb.merge_index('p_merge_wrong_idx')");
    }

    #[pg_test(
        error = "plumb.merge_index requires persisted postings_v1 storage; REINDEX is required"
    )]
    fn merge_rejects_heap_storage() {
        Spi::run(
            "CREATE TABLE p_merge_heap(body text);
            CREATE INDEX p_merge_heap_idx ON p_merge_heap USING plumb(body) WITH(storage='heap');",
        )
        .unwrap();
        let _ = Spi::get_one::<pgrx::JsonB>("SELECT plumb.merge_index('p_merge_heap_idx')");
    }

    #[pg_test]
    fn merge_requires_owner_or_inherited_ownership() {
        setup();
        // All roles and grants are transactional. SET LOCAL plus RESET ROLE
        // also prevents a caught negative test from contaminating later checks.
        Spi::run("CREATE ROLE p_merge_owner NOLOGIN;
            CREATE ROLE p_merge_member NOLOGIN INHERIT;
            CREATE ROLE p_merge_outsider NOLOGIN;
            GRANT p_merge_owner TO p_merge_member;
            GRANT USAGE ON SCHEMA plumb,public TO p_merge_owner,p_merge_member,p_merge_outsider;
            GRANT EXECUTE ON FUNCTION plumb.merge_index(regclass) TO p_merge_owner,p_merge_member,p_merge_outsider;
            ALTER TABLE p_docs OWNER TO p_merge_owner;
            SET LOCAL ROLE p_merge_member;").unwrap();
        assert_eq!(merge().0["changed"], false);
        Spi::run("RESET ROLE; SET LOCAL ROLE p_merge_outsider;
            DO $$ BEGIN
                BEGIN
                    PERFORM plumb.merge_index('public.p_docs_idx');
                EXCEPTION WHEN insufficient_privilege THEN
                    IF SQLERRM <> 'must be owner of index to merge plumb postings' THEN RAISE; END IF;
                    RETURN;
                END;
                RAISE EXCEPTION 'nonowner merge unexpectedly succeeded';
            END $$;
            RESET ROLE;").unwrap();
        assert_eq!(segments(), 1);
        assert_eq!(ids("beer"), vec![1, 3]);
    }

    #[pg_test]
    fn default_bulk_build_exceeds_old_whole_index_pair_limit() {
        // 400,000 distinct term/CTID pairs exceed the former 262,144-pair
        // whole-build cap while each bounded segment stays below it.
        Spi::run(
            "CREATE TABLE p_docs(id int,body text);
            INSERT INTO p_docs SELECT n,'alpha beta gamma ' ||
                CASE WHEN n%997=0 THEN 'rare' ELSE 'ordinary' END
                FROM generate_series(1,100000) n;
            CREATE INDEX p_docs_idx ON p_docs USING plumb(body);",
        )
        .unwrap();
        assert!(segments() > 1);
        for query in ["rare", "alpha AND rare", "rare OR ordinary"] {
            Spi::run("SET LOCAL enable_bitmapscan=off; SET LOCAL enable_seqscan=on;").unwrap();
            let expected = ids(query);
            Spi::run("SET LOCAL enable_bitmapscan=on; SET LOCAL enable_seqscan=off;").unwrap();
            assert_eq!(ids(query), expected, "{query}");
        }
        assert_eq!(ids("rare").len(), 100);
    }

    #[pg_test(error = "plumb.index_stats requires an index using the plumb access method")]
    fn postings_stats_rejects_other_access_methods() {
        Spi::run("CREATE TABLE p_wrong(id int); CREATE INDEX p_wrong_idx ON p_wrong(id)").unwrap();
        let _ = Spi::get_one::<pgrx::JsonB>("SELECT plumb.index_stats('p_wrong_idx')");
    }
}
