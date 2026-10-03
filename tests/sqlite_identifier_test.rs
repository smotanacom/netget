//! Legal quoted table names must survive schema and row-count refreshes.
#![cfg(feature = "sqlite")]

use netget::state::sqlite::{DatabaseConnection, DatabaseId, DatabaseInstance, DatabaseOwner};

#[test]
fn quoted_table_names_survive_schema_and_dml_refreshes() {
    let instance = DatabaseInstance::new(
        DatabaseId::new(1),
        "identifier_test".into(),
        ":memory:".into(),
        DatabaseOwner::Global,
    );
    let mut database = DatabaseConnection::new(instance).expect("in-memory database");
    for name in ["O'Brien", "double\"quote", "semi;colon", "雪 table"] {
        let identifier = format!("\"{}\"", name.replace('"', "\"\""));
        database
            .execute_query(&format!(
                "CREATE TABLE {identifier} (id INTEGER PRIMARY KEY, value TEXT)"
            ))
            .unwrap_or_else(|error| panic!("schema refresh failed for {name:?}: {error}"));
        database
            .execute_query(&format!(
                "INSERT INTO {identifier} (value) VALUES ('first'), ('second')"
            ))
            .unwrap_or_else(|error| panic!("row-count refresh failed for {name:?}: {error}"));
        let table = database
            .instance()
            .tables
            .iter()
            .find(|table| table.name == name)
            .expect("table in schema");
        assert_eq!(table.row_count, 2);
        assert_eq!(table.columns.len(), 2);
        database
            .execute_query(&format!("DELETE FROM {identifier} WHERE id = 1"))
            .expect("delete refresh");
        assert_eq!(
            database
                .instance()
                .tables
                .iter()
                .find(|table| table.name == name)
                .unwrap()
                .row_count,
            1
        );
    }
    database.refresh_schema().expect("refresh complete schema");
    assert_eq!(database.instance().tables.len(), 4);
}
