//! Process-wide SQLite settings applied by the store. A separate test binary, so the store's
//! open functions are guaranteed to be the first thing that touches SQLite.

/// SQLite's allocation statistics sit behind one process-wide mutex that every allocation
/// takes; with the writer and many readers busy at once it serialises them (measured in the
/// Task 10 service concurrency test). The store turns the statistics off.
#[test]
fn opening_through_the_store_turns_off_sqlite_memory_statistics() {
    let d = tempfile::tempdir().unwrap();
    let c = alice_core::store::open_writer(&d.path().join("a.db")).unwrap();
    c.execute_batch("CREATE TABLE t(x); INSERT INTO t VALUES (randomblob(100000));")
        .unwrap();
    let r = alice_core::store::open_reader(&d.path().join("a.db")).unwrap();
    let n: i64 = r
        .query_row("SELECT length(x) FROM t", [], |row| row.get(0))
        .unwrap();
    assert_eq!(n, 100_000);
    let used = unsafe { rusqlite::ffi::sqlite3_memory_used() };
    assert_eq!(
        used, 0,
        "SQLite still counts its allocations ({used} bytes)"
    );
}
