//! In-memory SQL only; no model, GPU, external service or persistent database.
#![cfg(feature = "sqlite")]
use netget::state::sqlite::{
    DatabaseConnection, DatabaseId, DatabaseInstance, DatabaseOwner, QueryResult, MAX_QUERY_ROWS,
    MAX_QUERY_VALUE_BYTES,
};

fn database() -> DatabaseConnection {
    DatabaseConnection::new(DatabaseInstance::new(
        DatabaseId::new(1),
        "bounds".into(),
        ":memory:".into(),
        DatabaseOwner::Global,
    ))
    .unwrap()
}

#[test]
fn prepared_statement_metadata_handles_comments_ctes_and_returning() {
    let mut db = database();
    db.execute_query("-- schema\nCREATE TABLE items(id INTEGER PRIMARY KEY, value TEXT)")
        .unwrap();
    let result = db.execute_query("WITH source(v) AS (SELECT 'first') INSERT INTO items(value) SELECT v FROM source RETURNING id, value").unwrap();
    assert!(
        matches!(result, QueryResult::Select { rows, .. } if rows == vec![vec![serde_json::json!(1), serde_json::json!("first")]])
    );
    assert_eq!(db.instance().tables[0].row_count, 1);
    let result = db
        .execute_query("/* query */ WITH data AS (SELECT value FROM items) SELECT * FROM data")
        .unwrap();
    assert_eq!(result.row_count(), 1);
    db.execute_query("/* mutation */ REPLACE INTO items VALUES (1, 'replacement')")
        .unwrap();
    assert_eq!(db.instance().tables[0].row_count, 1);
    db.execute_query("WITH ids AS (SELECT id FROM items) DELETE FROM items WHERE id IN ids")
        .unwrap();
    assert_eq!(db.instance().tables[0].row_count, 0);
}

#[test]
fn result_rows_bytes_and_sqlite_value_allocations_are_bounded() {
    let mut db = database();
    let rows = db.execute_query(&format!("WITH RECURSIVE data(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM data WHERE n < {}) SELECT n FROM data", MAX_QUERY_ROWS + 1)).unwrap_err();
    assert!(format!("{rows:#}").contains("rows; use LIMIT"));
    let bytes = db.execute_query("WITH RECURSIVE data(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM data WHERE n < 2000) SELECT printf('%01024d', n) FROM data").unwrap_err();
    assert!(format!("{bytes:#}").contains("bytes; select fewer"));
    assert!(db
        .execute_query(&format!("SELECT zeroblob({})", MAX_QUERY_VALUE_BYTES + 1))
        .is_err());
    assert_eq!(db.execute_query("SELECT 42").unwrap().row_count(), 1);
}

#[test]
fn work_budget_interrupts_unbounded_cte_and_resets_for_next_query() {
    let mut db = database();
    let error = db.execute_query("WITH RECURSIVE forever(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM forever) SELECT SUM(n) FROM forever").unwrap_err();
    assert!(format!("{error:#}").contains("interrupted"), "{error:#}");
    assert_eq!(db.execute_query("SELECT 7").unwrap().row_count(), 1);
}

#[test]
fn multiple_statements_are_rejected_before_execution() {
    let mut db = database();
    assert!(db
        .execute_query("CREATE TABLE should_not_exist(x); SELECT 1")
        .is_err());
    assert!(db.execute_query("SELECT * FROM should_not_exist").is_err());
}
