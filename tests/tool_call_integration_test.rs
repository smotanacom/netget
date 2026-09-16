//! Integration tests for tool calls
//!
//! These tests demonstrate the LLM using tools (read_file, web_search) to gather
//! information and use it in network protocol responses.

/// The death tie that keeps a spawned `netget` from outliving this binary.
///
/// Pulled in by path rather than through `mod helpers;` because this file needs exactly one
/// module out of the shared harness and nothing else in it: `child_guard.rs` depends only on
/// `std` and `libc`, so it compiles standalone. `Drop` alone is not enough — it does not run
/// when the test binary is `SIGKILL`ed, aborts, or is interrupted, which is precisely how 78
/// orphaned `netget` processes accumulated on one machine in ten hours. These tests are the
/// worst case for it: they wait a fixed sixty seconds against a real model, so an interrupted
/// run leaves a netget holding a MySQL port for as long as the machine stays up.
#[cfg(all(test, feature = "mysql"))]
#[path = "helpers/child_guard.rs"]
mod child_guard;

#[cfg(all(test, feature = "mysql"))]
mod tool_call_integration_tests {
    use super::child_guard;
    use mysql_async::prelude::*;
    use std::process::{Command, Stdio};
    use std::time::Duration;
    use tokio::net::TcpListener;
    use tokio::time::sleep;

    /// Helper to get an available port
    async fn get_available_port() -> u16 {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("Failed to bind to port 0");
        let port = listener
            .local_addr()
            .expect("Failed to get local addr")
            .port();
        drop(listener); // Release the port
        port
    }

    /// Test that MySQL server can read schema.sql and count records
    ///
    /// NOTE: This test requires a real LLM (Ollama) and is skipped by default.
    /// Run with: USE_OLLAMA=1 cargo test --features mysql --test tool_call_integration_test
    #[tokio::test]
    async fn test_mysql_reads_schema_and_counts_records() {
        // Skip if USE_OLLAMA is not set
        if std::env::var("USE_OLLAMA").unwrap_or_default() != "1" {
            eprintln!("Skipping test (requires USE_OLLAMA=1 and real Ollama server)");
            return;
        }

        // 1. Get an available port to avoid conflicts
        let port = get_available_port().await;

        // Start NetGet MySQL server with prompt to read schema
        let prompt = format!(
            "Start a MySQL server on port {}. Use the read_file tool to read tests/fixtures/schema.sql and assume its contents have been applied to the database. When clients query the database, respond based on the schema and data in that file.",
            port
        );

        println!("Starting NetGet with prompt: {}", prompt);

        let netget_bin = env!("CARGO_BIN_EXE_netget");
        let mut child = Command::new(netget_bin)
            .arg(prompt)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("Failed to start netget");
        let pid = child.id();
        child_guard::tie_child(pid);

        // 2. Wait for server to start (give it time for LLM processing and server startup)
        println!("Waiting for server to start (this takes time with real LLM)...");
        sleep(Duration::from_secs(60)).await;

        // 3. Check that the process is still running
        match child.try_wait() {
            Ok(Some(status)) => {
                // Process exited - capture output for debugging
                let output = child.wait_with_output().unwrap();
                // Exited on its own, so the pid is free; release the tie before it is
                // recycled onto some other process.
                child_guard::untie_child(pid);
                eprintln!(
                    "NetGet stdout:\n{}",
                    String::from_utf8_lossy(&output.stdout)
                );
                eprintln!(
                    "NetGet stderr:\n{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                panic!("NetGet exited early with status: {}", status);
            }
            Ok(None) => {
                // Still running, good
                println!("NetGet is running, proceeding with test...");
            }
            Err(e) => {
                panic!("Error checking NetGet status: {}", e);
            }
        }

        // 4. Connect with Rust MySQL client and count records
        println!("Connecting to MySQL with Rust client on port {}...", port);

        let database_url = format!("mysql://root@127.0.0.1:{}/test", port);
        let pool = mysql_async::Pool::new(database_url.as_str());

        let result = async {
            let mut conn = pool.get_conn().await?;

            // Query for count - simple aggregation
            let rows: Vec<u64> = conn.query("SELECT COUNT(*) FROM users").await?;

            Ok::<_, mysql_async::Error>(rows)
        }
        .await;

        // 5. Kill the NetGet process
        let _ = child.kill();
        let _ = child.wait();
        // After the kill, never before: a tie released first leaves nothing watching.
        child_guard::untie_child(pid);

        // 6. Assert the query succeeded and count is correct
        match result {
            Ok(rows) => {
                assert_eq!(
                    rows.len(),
                    1,
                    "Expected exactly 1 row for COUNT query, got {}",
                    rows.len()
                );

                let count = rows[0];

                println!("✓ MySQL query succeeded!");
                println!("Retrieved count: {}", count);

                assert_eq!(
                    count, 7,
                    "Expected count=7 (7 records in schema.sql), got {}",
                    count
                );

                println!("✓ All assertions passed! LLM successfully read schema.sql and returned correct count.");
            }
            Err(e) => {
                panic!(
                    "MySQL query failed: {}. This could mean:\n\
                        1. Server didn't start (check if Ollama is running)\n\
                        2. LLM didn't read schema.sql correctly\n\
                        3. LLM didn't understand the COUNT query\n\
                        Error details: {}",
                    e, e
                );
            }
        }
    }

    /// Test that MySQL server can read its instructions from a file
    ///
    /// NOTE: This test requires a real LLM (Ollama) and is skipped by default.
    /// Run with: USE_OLLAMA=1 cargo test --features mysql --test tool_call_integration_test
    #[tokio::test]
    async fn test_mysql_reads_instructions_from_file() {
        // Skip if USE_OLLAMA is not set
        if std::env::var("USE_OLLAMA").unwrap_or_default() != "1" {
            eprintln!("Skipping test (requires USE_OLLAMA=1 and real Ollama server)");
            return;
        }

        // 1. Get an available port to avoid conflicts
        let port = get_available_port().await;

        // Start NetGet with prompt to read instructions from file and use specified port
        let prompt = format!(
            "Read the file tests/fixtures/mysql_prompt.txt using read_file tool, then execute the instructions you find in that file. Use port {} for the server.",
            port
        );

        println!("Starting NetGet with meta-prompt: {}", prompt);

        let netget_bin = env!("CARGO_BIN_EXE_netget");
        let mut child = Command::new(netget_bin)
            .arg(prompt)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .expect("Failed to start netget");
        let pid = child.id();
        child_guard::tie_child(pid);

        // 2. Wait for server to start and process the file instructions
        println!(
            "Waiting for server to read prompt file and start (this takes time with real LLM)..."
        );
        sleep(Duration::from_secs(60)).await;

        // 3. Check that the process is still running
        match child.try_wait() {
            Ok(Some(status)) => {
                let output = child.wait_with_output().unwrap();
                // Exited on its own, so the pid is free; release the tie before it is
                // recycled onto some other process.
                child_guard::untie_child(pid);
                eprintln!(
                    "NetGet stdout:\n{}",
                    String::from_utf8_lossy(&output.stdout)
                );
                eprintln!(
                    "NetGet stderr:\n{}",
                    String::from_utf8_lossy(&output.stderr)
                );
                panic!("NetGet exited early with status: {}", status);
            }
            Ok(None) => {
                println!("NetGet is running, proceeding with test...");
            }
            Err(e) => {
                panic!("Error checking NetGet status: {}", e);
            }
        }

        // 4. Connect to MySQL on the dynamic port
        println!(
            "Connecting to MySQL on port {} (from file instructions)...",
            port
        );

        let database_url = format!("mysql://root@127.0.0.1:{}/test", port);
        let pool = mysql_async::Pool::new(database_url.as_str());

        let result = async {
            let mut conn = pool.get_conn().await?;

            // Query anything - the file says to respond with specific data
            let rows: Vec<(u32, String, String)> = conn
                .query("SELECT id, message, status FROM test_table")
                .await?;

            Ok::<_, mysql_async::Error>(rows)
        }
        .await;

        // 5. Kill the NetGet process
        let _ = child.kill();
        let _ = child.wait();
        // After the kill, never before: a tie released first leaves nothing watching.
        child_guard::untie_child(pid);

        // 6. Assert the query succeeded and returned data from file prompt
        match result {
            Ok(rows) => {
                assert!(
                    !rows.is_empty(),
                    "Expected at least 1 row, got 0. LLM may not have followed file instructions."
                );

                let (id, message, status) = &rows[0];

                println!("✓ MySQL query succeeded!");
                println!(
                    "Retrieved row: id={}, message={}, status={}",
                    id, message, status
                );

                // Verify the response matches what was instructed in mysql_prompt.txt
                assert_eq!(*id, 42, "Expected id=42 as specified in mysql_prompt.txt");
                assert_eq!(
                    message, "Hello from file prompt",
                    "Expected message='Hello from file prompt' as specified in mysql_prompt.txt"
                );
                assert_eq!(
                    status, "active",
                    "Expected status='active' as specified in mysql_prompt.txt"
                );

                println!("✓ All assertions passed! LLM successfully read mysql_prompt.txt and followed its instructions.");
            }
            Err(e) => {
                panic!(
                    "MySQL query failed: {}. This could mean:\n\
                        1. Server didn't start on port {} (check if LLM read the file)\n\
                        2. LLM didn't follow instructions in mysql_prompt.txt\n\
                        3. LLM didn't understand the meta-prompt\n\
                        Error details: {}",
                    e, port, e
                );
            }
        }
    }

    /// Helper test to verify the schema.sql file exists
    #[test]
    fn test_schema_file_exists() {
        let schema_path = "tests/fixtures/schema.sql";
        assert!(
            std::path::Path::new(schema_path).exists(),
            "Schema file not found at: {}",
            schema_path
        );

        let content = std::fs::read_to_string(schema_path).expect("Failed to read schema.sql");

        assert!(content.contains("CREATE TABLE users"));
        assert!(content.contains("INSERT INTO users"));

        // Verify it contains all 7 test users
        assert!(content.contains("alice"));
        assert!(content.contains("bob"));
        assert!(content.contains("charlie"));
        assert!(content.contains("diana"));
        assert!(content.contains("eve"));
        assert!(content.contains("frank"));
        assert!(content.contains("grace"));
    }

    /// Helper test to verify the mysql_prompt.txt file exists
    #[test]
    fn test_mysql_prompt_file_exists() {
        let prompt_path = "tests/fixtures/mysql_prompt.txt";
        assert!(
            std::path::Path::new(prompt_path).exists(),
            "Prompt file not found at: {}",
            prompt_path
        );

        let content =
            std::fs::read_to_string(prompt_path).expect("Failed to read mysql_prompt.txt");

        assert!(content.contains("Start a MySQL server"));
        assert!(content.contains("Hello from file prompt"));
        assert!(content.contains("id: 42"));
        assert!(content.contains("status: \"active\""));
    }
}
