use super::*;

#[test]
fn full_document_survives_preview_budget_and_preserves_literal_identifiers() {
    let root = tempfile::tempdir().unwrap();
    let spool = ai_output::Spool::create(root.path(), "conversation", "output-1").unwrap();
    let text = (0..5000)
        .map(|i| format!("row {i}: 输出 id literal-user-uuid\n"))
        .collect::<String>();
    spool.write(text.as_bytes(), false).unwrap();
    spool.finish().unwrap();
    let cancel = AtomicBool::new(false);
    let doc = Document::load(root.path(), "conversation", "output-1", &cancel).unwrap();
    assert_eq!(doc.rows, 5000);
    assert_eq!(doc.info.completeness, Completeness::Complete);
    assert_eq!(doc.copy(0..doc.bytes, &cancel).unwrap(), text);
    let middle = doc.segment(2500, 0, 200).unwrap();
    assert!(middle.text.contains("row 2500"));
    let last = doc.segment(4999, 0, 200).unwrap();
    assert!(last.text.contains("row 4999"));
    assert!(
        doc.copy(middle.carets[0].1..last.carets[0].1, &cancel)
            .unwrap()
            .contains("row 4000")
    );
}
#[test]
fn normalization_matches_existing_rules_across_chunks_and_carriage_returns() {
    struct Chunks<'a>(&'a [u8]);
    impl Read for Chunks<'_> {
        fn read(&mut self, out: &mut [u8]) -> io::Result<usize> {
            let n = self.0.len().min(out.len()).min(1);
            out[..n].copy_from_slice(&self.0[..n]);
            self.0 = &self.0[n..];
            Ok(n)
        }
    }
    let input=b"\x1b[31mred\x1b[0m\x1b]0;title\x07\x1b]8;;url\x1b\\link\x1b]8;;\x1b\\\n100%\r20%\r9\n\xe4\xb8\xad\xe6\x96\x87\t\xff\n";
    let info = SnapshotInfo {
        total_bytes: input.len() as u64,
        retained_bytes: input.len() as u64,
        completeness: Completeness::Complete,
        finished: true,
    };
    let cancel = AtomicBool::new(false);
    let doc = Document::from_reader(Chunks(input), info, &cancel).unwrap();
    assert_eq!(
        doc.copy(0..doc.bytes, &cancel).unwrap(),
        shellmate_ai::output::clean(input)
    );
    assert!(doc.segment(2, 0, 30).unwrap().text.starts_with("中文"));
}
#[test]
fn giant_line_is_indexed_and_windowed_and_cr_keeps_the_suffix() {
    let text = format!("{}END\rSTART", "x".repeat(2 * 1024 * 1024));
    let cancel = AtomicBool::new(false);
    let doc = Document::inline(&text, true, &cancel).unwrap();
    assert_eq!(doc.rows, 1);
    assert!(doc.segment(0, 0, 10).unwrap().text.starts_with("START"));
    let tail = doc.segment(0, 2 * 1024 * 1024 - 10, 30).unwrap();
    assert!(tail.text.ends_with("END"));
    assert!(tail.text.len() < 40);
    assert!(doc.marks.len() < 5000);
}
#[test]
fn missing_output_is_not_recreated_and_cancellation_stops_loading_or_copying() {
    let root = tempfile::tempdir().unwrap();
    let cancel = AtomicBool::new(false);
    assert_eq!(
        Document::load(root.path(), "c", "missing", &cancel)
            .err()
            .unwrap()
            .kind(),
        io::ErrorKind::NotFound
    );
    assert!(!root.path().join("ai-outputs").exists());
    let doc = Document::inline("abc", true, &cancel).unwrap();
    cancel.store(true, Ordering::Relaxed);
    assert_eq!(
        doc.copy(0..doc.bytes, &cancel).unwrap_err().kind(),
        io::ErrorKind::Interrupted
    );
    assert_eq!(
        Document::inline("abc", true, &cancel).err().unwrap().kind(),
        io::ErrorKind::Interrupted
    );
}

#[test]
fn live_refresh_preserves_old_snapshot_and_appends_or_rebuilds_partial_lines() {
    let root = tempfile::tempdir().unwrap();
    let spool = ai_output::Spool::create(root.path(), "c", "out").unwrap();
    let cancel = AtomicBool::new(false);
    spool.write(b"first\n", false).unwrap();
    let first = Document::load(root.path(), "c", "out", &cancel).unwrap();
    let same = Document::refresh(root.path(), "c", "out", Some(&first), &cancel).unwrap();
    assert!(Arc::ptr_eq(&first, &same));
    spool.write(b"second\nprogress 100%", false).unwrap();
    let next = Document::refresh(root.path(), "c", "out", Some(&first), &cancel).unwrap();
    assert_eq!(first.copy(0..first.bytes, &cancel).unwrap(), "first\n");
    assert_eq!(
        next.copy(0..next.bytes, &cancel).unwrap(),
        "first\nsecond\nprogress 100%"
    );
    spool.write(b"\rDONE\n", false).unwrap();
    spool.finish().unwrap();
    let final_doc = Document::refresh(root.path(), "c", "out", Some(&next), &cancel).unwrap();
    assert_eq!(
        final_doc.copy(0..final_doc.bytes, &cancel).unwrap(),
        "first\nsecond\nDONEress 100%\n"
    );
    assert!(final_doc.info.finished);
    assert_eq!(final_doc.rows, 3);
}

#[test]
fn near_retention_boundary_uses_bounded_pages_and_a_sparse_index() {
    let root = tempfile::tempdir().unwrap();
    let spool = ai_output::Spool::create(root.path(), "large", "out").unwrap();
    let chunk = (0..4096)
        .map(|n| format!("{n:04} {}\n", "x".repeat(250)))
        .collect::<String>();
    for _ in 0..60 {
        spool.write(chunk.as_bytes(), false).unwrap();
    }
    spool.write(b"FINAL-64-MIB-BOUNDARY\n", false).unwrap();
    spool.finish().unwrap();
    let cancel = AtomicBool::new(false);
    let start = std::time::Instant::now();
    let doc = Document::load(root.path(), "large", "out", &cancel).unwrap();
    assert_eq!(doc.info.completeness, Completeness::Complete);
    assert_eq!(doc.rows, 4096 * 60 + 1);
    assert!(doc.marks.len() < 2500);
    assert!(
        doc.segment(doc.rows - 1, 0, 100)
            .unwrap()
            .text
            .contains("FINAL-64-MIB-BOUNDARY")
    );
    assert!(
        doc.region(&[0, 12345, doc.rows - 1], 10, 80, &cancel)
            .unwrap()
            .iter()
            .all(|s| s.text.len() <= 82)
    );
    eprintln!(
        "60 MiB output indexed in {:?}; {} index marks; {} rows",
        start.elapsed(),
        doc.marks.len(),
        doc.rows
    );
}

#[test]
fn conversation_lifetime_cancels_work_while_old_snapshots_are_still_retained() {
    let lifetime = Lifetime::default();
    let token = lifetime.0.clone();
    let doc = Document::inline("old conversation", true, &token).unwrap();
    drop(lifetime);
    assert_eq!(
        doc.copy(0..doc.bytes, &token).unwrap_err().kind(),
        io::ErrorKind::Interrupted
    );
}

#[test]
fn rendered_carets_keep_unicode_and_expanded_tabs_mapped_to_original_bytes() {
    let cancel = AtomicBool::new(false);
    let doc = Document::inline("中\t文", true, &cancel).unwrap();
    let segment = doc.segment(0, 0, 30).unwrap();
    assert_eq!(segment.text, "中  文");
    assert_eq!(segment.carets, [(0, 0), (3, 3), (5, 4), (8, 7)]);
    assert_eq!(segment.selected_range(&(3..4)), Some(3..5));
    assert_eq!(doc.copy(3..4, &cancel).unwrap(), "\t");
}
