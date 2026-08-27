//! IMAP E2E integration tests
//!
//! These tests verify IMAP protocol implementation by:
//! - Starting NetGet in non-interactive mode with IMAP prompts
//! - Using raw TCP clients to send IMAP commands
//! - Validating IMAP responses against RFC 3501 expectations

use crate::helpers::{start_netget_server, wait_for_server_startup, E2EResult, NetGetConfig};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::time::{timeout, Duration};

/// Helper to send IMAP command and read response until tagged response
async fn send_imap_command(
    stream: &mut TcpStream,
    tag: &str,
    command: &str,
) -> E2EResult<Vec<String>> {
    // Send command
    let cmd = format!("{} {}\r\n", tag, command);
    stream.write_all(cmd.as_bytes()).await?;
    stream.flush().await?;

    // Read responses with timeout to prevent indefinite hangs
    let result = timeout(Duration::from_secs(60), async {
        let mut reader = BufReader::new(stream);
        let mut responses = Vec::new();

        loop {
            let mut line = String::new();
            let n = reader.read_line(&mut line).await?;
            if n == 0 {
                break; // EOF
            }

            responses.push(line.trim().to_string());

            // Check if this is the tagged response (A001 OK, A001 NO, A001 BAD)
            if line.starts_with(tag) {
                break;
            }
        }

        Ok::<Vec<String>, std::io::Error>(responses)
    })
    .await
    .map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "Timeout reading IMAP response after 60 seconds",
        )
    })??;

    Ok(result)
}

/// Helper to read greeting (untagged OK response)
async fn read_greeting(stream: &mut TcpStream) -> E2EResult<String> {
    // Read greeting with timeout to prevent indefinite hangs
    let result = timeout(Duration::from_secs(60), async {
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).await?;
        Ok::<String, std::io::Error>(line.trim().to_string())
    })
    .await
    .map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::TimedOut,
            "Timeout reading IMAP greeting after 60 seconds",
        )
    })??;

    Ok(result)
}

#[tokio::test]
async fn test_imap_greeting() -> E2EResult<()> {
    let prompt =
        "listen on port {AVAILABLE_PORT} via imap. Send greeting: * OK IMAP4rev1 Server Ready";

    let config = NetGetConfig::new(prompt).with_mock(|mock| {
        mock
            // Mock 1: Server startup
            .on_instruction_containing("listen on port")
            .and_instruction_containing("imap")
            .respond_with_actions(serde_json::json!([
                {
                    "type": "open_server",
                    "port": 0,
                    "base_stack": "IMAP",
                    "instruction": "Send greeting: * OK IMAP4rev1 Server Ready"
                }
            ]))
            .expect_calls(1)
            .and()
            // Mock 2: Connection accepted
            .on_event("imap_connection")
            .respond_with_actions(serde_json::json!([
                {
                    "type": "send_imap_response",
                    "response": "* OK IMAP4rev1 Server Ready"
                }
            ]))
            .expect_calls(1)
            .and()
    });

    let server = start_netget_server(config).await?;

    // Wait for server to start
    wait_for_server_startup(&server, Duration::from_secs(10), "IMAP").await?;

    // Connect and verify greeting
    let mut client = timeout(
        Duration::from_secs(5),
        TcpStream::connect(format!("127.0.0.1:{}", server.port)),
    )
    .await??;

    let greeting = read_greeting(&mut client).await?;

    // Verify greeting format: * OK [CAPABILITY ...] message
    assert!(
        greeting.starts_with("* OK"),
        "Greeting should start with '* OK', got: {}",
        greeting
    );
    assert!(
        greeting.contains("IMAP4rev1") || greeting.contains("Server Ready"),
        "Greeting should mention IMAP4rev1 or Server Ready, got: {}",
        greeting
    );

    // Wait for the exchange the mocks describe, rather than trusting a fixed
    // sleep to have covered it. Under load the last event routinely lands after
    // the sleep expires, and the test reports it as never having happened.
    server.wait_for_mocks(30).await;
    server.verify_mocks().await?;
    server.stop().await?;
    Ok(())
}

#[tokio::test]
async fn test_imap_capability() -> E2EResult<()> {
    let prompt = "listen on port {AVAILABLE_PORT} via imap. Support IMAP4rev1, IDLE, NAMESPACE capabilities.";

    let config = NetGetConfig::new(prompt)
        .with_mock(|mock| {
            mock
                // Mock 1: Server startup
                .on_instruction_containing("listen on port")
                .and_instruction_containing("imap")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "open_server",
                        "port": 0,
                        "base_stack": "IMAP",
                        "instruction": "Support IMAP4rev1, IDLE, NAMESPACE capabilities"
                    }
                ]))
                .expect_calls(1)
                .and()
                // Mock 2: Connection greeting
                .on_event("imap_connection")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "send_imap_response",
                        "response": "* OK [CAPABILITY IMAP4rev1 IDLE NAMESPACE] Server Ready"
                    }
                ]))
                .expect_calls(1)
                .and()
                // Mock 3: CAPABILITY command response
                .on_event("imap_command")
                .and_event_data_contains("command", "CAPABILITY")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "send_imap_response",
                        "response": "* CAPABILITY IMAP4rev1 IDLE NAMESPACE\r\nA001 OK CAPABILITY completed"
                    }
                ]))
                .expect_calls(1)
                .and()
        });

    let server = start_netget_server(config).await?;
    wait_for_server_startup(&server, Duration::from_secs(10), "IMAP").await?;

    let mut client = TcpStream::connect(format!("127.0.0.1:{}", server.port)).await?;
    let _greeting = read_greeting(&mut client).await?;

    // Send CAPABILITY command
    let responses = send_imap_command(&mut client, "A001", "CAPABILITY").await?;

    // Should have at least 2 lines: * CAPABILITY ... and A001 OK ...
    assert!(
        responses.len() >= 2,
        "Expected at least 2 responses, got: {:?}",
        responses
    );

    // Check for capability response
    let cap_line = responses
        .iter()
        .find(|l| l.starts_with("* CAPABILITY"))
        .expect("Should have CAPABILITY response");

    assert!(
        cap_line.contains("IMAP4rev1"),
        "CAPABILITY should include IMAP4rev1, got: {}",
        cap_line
    );

    // Check for tagged OK response
    let ok_line = responses
        .iter()
        .find(|l| l.starts_with("A001 OK"))
        .expect("Should have tagged OK response");

    assert!(
        ok_line.contains("OK"),
        "Tagged response should be OK, got: {}",
        ok_line
    );

    // Wait for the exchange the mocks describe, rather than trusting a fixed
    // sleep to have covered it. Under load the last event routinely lands after
    // the sleep expires, and the test reports it as never having happened.
    server.wait_for_mocks(30).await;
    server.verify_mocks().await?;
    server.stop().await?;
    Ok(())
}

#[tokio::test]
async fn test_imap_login() -> E2EResult<()> {
    let prompt = "listen on port {AVAILABLE_PORT} via imap. Allow LOGIN for username 'testuser' with password 'testpass'. Any other credentials should fail.";

    let config = NetGetConfig::new(prompt).with_mock(|mock| {
        mock
            // Mock 1: Server startup
            .on_instruction_containing("listen on port")
            .and_instruction_containing("imap")
            .respond_with_actions(serde_json::json!([
                {
                    "type": "open_server",
                    "port": 0,
                    "base_stack": "IMAP",
                    "instruction": "Allow LOGIN for testuser/testpass"
                }
            ]))
            .expect_calls(1)
            .and()
            // Mock 2: Connection greeting
            .on_event("imap_connection")
            .respond_with_actions(serde_json::json!([
                {
                    "type": "send_imap_response",
                    "response": "* OK IMAP4rev1 Server Ready"
                }
            ]))
            .expect_calls(1)
            .and()
            // Mock 3: LOGIN auth response
            .on_event("imap_auth")
            .respond_with_actions(serde_json::json!([
                {
                    "type": "send_imap_response",
                    "response": "A001 OK LOGIN completed"
                }
            ]))
            .expect_calls(1)
            .and()
    });

    let server = start_netget_server(config).await?;
    wait_for_server_startup(&server, Duration::from_secs(10), "IMAP").await?;

    let mut client = TcpStream::connect(format!("127.0.0.1:{}", server.port)).await?;
    let _greeting = read_greeting(&mut client).await?;

    // Send LOGIN command with correct credentials
    let responses = send_imap_command(&mut client, "A001", "LOGIN testuser testpass").await?;

    // Should have tagged OK response
    let ok_line = responses
        .iter()
        .find(|l| l.starts_with("A001 OK"))
        .expect("Should have tagged OK response");

    assert!(
        ok_line.contains("OK"),
        "LOGIN should succeed with OK, got: {}",
        ok_line
    );

    // Wait for the exchange the mocks describe, rather than trusting a fixed
    // sleep to have covered it. Under load the last event routinely lands after
    // the sleep expires, and the test reports it as never having happened.
    server.wait_for_mocks(30).await;
    server.verify_mocks().await?;
    server.stop().await?;
    Ok(())
}

#[tokio::test]
async fn test_imap_login_failure() -> E2EResult<()> {
    let prompt = "listen on port {AVAILABLE_PORT} via imap. Allow LOGIN for username 'testuser' with password 'testpass'. Reject invalid credentials with 'A001 NO Invalid credentials'.";

    let config = NetGetConfig::new(prompt).with_mock(|mock| {
        mock
            // Mock 1: Server startup
            .on_instruction_containing("listen on port")
            .and_instruction_containing("imap")
            .respond_with_actions(serde_json::json!([
                {
                    "type": "open_server",
                    "port": 0,
                    "base_stack": "IMAP",
                    "instruction": "Allow LOGIN testuser/testpass, reject others"
                }
            ]))
            .expect_calls(1)
            .and()
            // Mock 2: Connection greeting
            .on_event("imap_connection")
            .respond_with_actions(serde_json::json!([
                {
                    "type": "send_imap_response",
                    "response": "* OK IMAP4rev1 Server Ready"
                }
            ]))
            .expect_calls(1)
            .and()
            // Mock 3: LOGIN failure response
            .on_event("imap_auth")
            .respond_with_actions(serde_json::json!([
                {
                    "type": "send_imap_response",
                    "response": "A001 NO Invalid credentials"
                }
            ]))
            .expect_calls(1)
            .and()
    });

    let server = start_netget_server(config).await?;
    wait_for_server_startup(&server, Duration::from_secs(10), "IMAP").await?;

    let mut client = TcpStream::connect(format!("127.0.0.1:{}", server.port)).await?;
    let _greeting = read_greeting(&mut client).await?;

    // Send LOGIN command with wrong credentials
    let responses = send_imap_command(&mut client, "A001", "LOGIN wronguser wrongpass").await?;

    // Should have tagged NO response
    let response_line = responses.last().expect("Should have at least one response");

    assert!(
        response_line.starts_with("A001 NO") || response_line.contains("Invalid"),
        "LOGIN should fail with NO, got: {}",
        response_line
    );

    // Wait for the exchange the mocks describe, rather than trusting a fixed
    // sleep to have covered it. Under load the last event routinely lands after
    // the sleep expires, and the test reports it as never having happened.
    server.wait_for_mocks(30).await;
    server.verify_mocks().await?;
    server.stop().await?;
    Ok(())
}

#[tokio::test]
async fn test_imap_select_mailbox() -> E2EResult<()> {
    let prompt = "listen on port {AVAILABLE_PORT} via imap. Allow LOGIN for 'alice' with password 'secret'. \
         INBOX has 5 messages, 2 recent. After SELECT INBOX, respond with: \
         * 5 EXISTS\r\n* 2 RECENT\r\n* FLAGS (\\Seen \\Answered \\Flagged \\Deleted \\Draft)\r\nA002 OK [READ-WRITE] SELECT completed";

    let config = NetGetConfig::new(prompt)
        .with_mock(|mock| {
            mock
                // Mock 1: Server startup
                .on_instruction_containing("listen on port")
                .and_instruction_containing("imap")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "open_server",
                        "port": 0,
                        "base_stack": "IMAP",
                        "instruction": "Allow LOGIN alice/secret, INBOX has 5 messages 2 recent"
                    }
                ]))
                .expect_calls(1)
                .and()
                // Mock 2: Connection greeting
                .on_event("imap_connection")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "send_imap_response",
                        "response": "* OK IMAP4rev1 Server Ready"
                    }
                ]))
                .expect_calls(1)
                .and()
                // Mock 3: LOGIN auth
                .on_event("imap_auth")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "send_imap_response",
                        "response": "A001 OK LOGIN completed"
                    }
                ]))
                .expect_calls(1)
                .and()
                // Mock 4: SELECT command
                .on_event("imap_command")
                .and_event_data_contains("command", "SELECT")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "send_imap_response",
                        "response": "* 5 EXISTS\r\n* 2 RECENT\r\n* FLAGS (\\Seen \\Answered \\Flagged \\Deleted \\Draft)\r\nA002 OK [READ-WRITE] SELECT completed"
                    }
                ]))
                .expect_calls(1)
                .and()
        });

    let server = start_netget_server(config).await?;
    wait_for_server_startup(&server, Duration::from_secs(10), "IMAP").await?;

    let mut client = TcpStream::connect(format!("127.0.0.1:{}", server.port)).await?;
    let _greeting = read_greeting(&mut client).await?;

    // Login first
    let _login_resp = send_imap_command(&mut client, "A001", "LOGIN alice secret").await?;

    // Select INBOX
    let responses = send_imap_command(&mut client, "A002", "SELECT INBOX").await?;

    // Check for EXISTS response
    let exists_line = responses
        .iter()
        .find(|l| l.contains("EXISTS"))
        .expect("Should have EXISTS response");

    assert!(
        exists_line.contains("5 EXISTS") || exists_line.contains("EXISTS"),
        "Should report message count, got: {}",
        exists_line
    );

    // Check for tagged OK response
    let ok_line = responses
        .iter()
        .find(|l| l.starts_with("A002 OK"))
        .expect("Should have tagged OK response");

    assert!(
        ok_line.contains("OK"),
        "SELECT should succeed with OK, got: {}",
        ok_line
    );

    // Wait for the exchange the mocks describe, rather than trusting a fixed
    // sleep to have covered it. Under load the last event routinely lands after
    // the sleep expires, and the test reports it as never having happened.
    server.wait_for_mocks(30).await;
    server.verify_mocks().await?;
    server.stop().await?;
    Ok(())
}

#[tokio::test]
async fn test_imap_list_mailboxes() -> E2EResult<()> {
    let prompt = "listen on port {AVAILABLE_PORT} via imap. Allow LOGIN for 'alice'. \
         Mailboxes: INBOX, Sent, Drafts. \
         After LIST \"\" \"*\", respond with: \
         * LIST () \"/\" \"INBOX\"\r\n* LIST () \"/\" \"Sent\"\r\n* LIST () \"/\" \"Drafts\"\r\nA003 OK LIST completed";

    let config = NetGetConfig::new(prompt)
        .with_mock(|mock| {
            mock
                // Mock 1: Server startup
                .on_instruction_containing("listen on port")
                .and_instruction_containing("imap")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "open_server",
                        "port": 0,
                        "base_stack": "IMAP",
                        "instruction": "Allow LOGIN alice, Mailboxes: INBOX, Sent, Drafts"
                    }
                ]))
                .expect_calls(1)
                .and()
                // Mock 2: Connection greeting
                .on_event("imap_connection")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "send_imap_response",
                        "response": "* OK IMAP4rev1 Server Ready"
                    }
                ]))
                .expect_calls(1)
                .and()
                // Mock 3: LOGIN auth
                .on_event("imap_auth")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "send_imap_response",
                        "response": "A001 OK LOGIN completed"
                    }
                ]))
                .expect_calls(1)
                .and()
                // Mock 4: LIST command
                .on_event("imap_command")
                .and_event_data_contains("command", "LIST")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "send_imap_response",
                        "response": "* LIST () \"/\" \"INBOX\"\r\n* LIST () \"/\" \"Sent\"\r\n* LIST () \"/\" \"Drafts\"\r\nA003 OK LIST completed"
                    }
                ]))
                .expect_calls(1)
                .and()
        });

    let server = start_netget_server(config).await?;
    wait_for_server_startup(&server, Duration::from_secs(10), "IMAP").await?;

    let mut client = TcpStream::connect(format!("127.0.0.1:{}", server.port)).await?;
    let _greeting = read_greeting(&mut client).await?;

    // Login first
    let _login_resp = send_imap_command(&mut client, "A001", "LOGIN alice secret").await?;

    // List mailboxes
    let responses = send_imap_command(&mut client, "A003", "LIST \"\" \"*\"").await?;

    // Check for LIST responses
    let list_lines: Vec<_> = responses
        .iter()
        .filter(|l| l.starts_with("* LIST"))
        .collect();

    assert!(
        list_lines.len() >= 1,
        "Should have at least 1 LIST response, got: {:?}",
        responses
    );

    // Check for INBOX
    let has_inbox = list_lines.iter().any(|l| l.contains("INBOX"));
    assert!(
        has_inbox,
        "LIST should include INBOX, got: {:?}",
        list_lines
    );

    // Check for tagged OK response
    let ok_line = responses
        .iter()
        .find(|l| l.starts_with("A003 OK"))
        .expect("Should have tagged OK response");

    assert!(
        ok_line.contains("OK"),
        "LIST should succeed with OK, got: {}",
        ok_line
    );

    // Wait for the exchange the mocks describe, rather than trusting a fixed
    // sleep to have covered it. Under load the last event routinely lands after
    // the sleep expires, and the test reports it as never having happened.
    server.wait_for_mocks(30).await;
    server.verify_mocks().await?;
    server.stop().await?;
    Ok(())
}

#[tokio::test]
async fn test_imap_fetch_message() -> E2EResult<()> {
    let prompt = "listen on port {AVAILABLE_PORT} via imap. Allow LOGIN for 'alice'. \
         After SELECT INBOX, respond with 1 EXISTS. \
         After FETCH 1 (FLAGS BODY[]), respond with: \
         * 1 FETCH (FLAGS (\\Seen) BODY[] {{50}}\r\nFrom: test@example.com\r\nSubject: Test\r\n\r\nHello)\r\nA004 OK FETCH completed";

    let config = NetGetConfig::new(prompt)
        .with_mock(|mock| {
            mock
                // Mock 1: Server startup
                .on_instruction_containing("listen on port")
                .and_instruction_containing("imap")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "open_server",
                        "port": 0,
                        "base_stack": "IMAP",
                        "instruction": "Allow LOGIN alice, INBOX has 1 message"
                    }
                ]))
                .expect_calls(1)
                .and()
                // Mock 2: Connection greeting
                .on_event("imap_connection")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "send_imap_response",
                        "response": "* OK IMAP4rev1 Server Ready"
                    }
                ]))
                .expect_calls(1)
                .and()
                // Mock 3: LOGIN auth
                .on_event("imap_auth")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "send_imap_response",
                        "response": "A001 OK LOGIN completed"
                    }
                ]))
                .expect_calls(1)
                .and()
                // Mock 4: SELECT command
                .on_event("imap_command")
                .and_event_data_contains("command", "SELECT")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "send_imap_response",
                        "response": "* 1 EXISTS\r\nA002 OK [READ-WRITE] SELECT completed"
                    }
                ]))
                .expect_calls(1)
                .and()
                // Mock 5: FETCH command
                .on_event("imap_command")
                .and_event_data_contains("command", "FETCH")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "send_imap_response",
                        "response": "* 1 FETCH (FLAGS (\\Seen) BODY[] {50}\r\nFrom: test@example.com\r\nSubject: Test\r\n\r\nHello)\r\nA004 OK FETCH completed"
                    }
                ]))
                .expect_calls(1)
                .and()
        });

    let server = start_netget_server(config).await?;
    wait_for_server_startup(&server, Duration::from_secs(10), "IMAP").await?;

    let mut client = TcpStream::connect(format!("127.0.0.1:{}", server.port)).await?;
    let _greeting = read_greeting(&mut client).await?;

    // Login and select
    let _login_resp = send_imap_command(&mut client, "A001", "LOGIN alice secret").await?;
    let _select_resp = send_imap_command(&mut client, "A002", "SELECT INBOX").await?;

    // Fetch message
    let responses = send_imap_command(&mut client, "A004", "FETCH 1 (FLAGS BODY[])").await?;

    // Check for FETCH response
    let fetch_line = responses
        .iter()
        .find(|l| l.contains("FETCH"))
        .expect("Should have FETCH response");

    assert!(
        fetch_line.contains("FETCH") && fetch_line.contains("FLAGS"),
        "FETCH should return message data, got: {}",
        fetch_line
    );

    // Check for tagged OK response
    let ok_line = responses
        .iter()
        .find(|l| l.starts_with("A004 OK"))
        .expect("Should have tagged OK response");

    assert!(
        ok_line.contains("OK"),
        "FETCH should succeed with OK, got: {}",
        ok_line
    );

    // Wait for the exchange the mocks describe, rather than trusting a fixed
    // sleep to have covered it. Under load the last event routinely lands after
    // the sleep expires, and the test reports it as never having happened.
    server.wait_for_mocks(30).await;
    server.verify_mocks().await?;
    server.stop().await?;
    Ok(())
}

#[tokio::test]
async fn test_imap_search() -> E2EResult<()> {
    let prompt = "listen on port {AVAILABLE_PORT} via imap. Allow LOGIN for 'alice'. \
         After SELECT INBOX, respond with 5 EXISTS. \
         After SEARCH ALL, respond with: \
         * SEARCH 1 2 3 4 5\r\nA005 OK SEARCH completed";

    let config = NetGetConfig::new(prompt).with_mock(|mock| {
        mock
            // Mock 1: Server startup
            .on_instruction_containing("listen on port")
            .and_instruction_containing("imap")
            .respond_with_actions(serde_json::json!([
                {
                    "type": "open_server",
                    "port": 0,
                    "base_stack": "IMAP",
                    "instruction": "Allow LOGIN alice, INBOX has 5 messages"
                }
            ]))
            .expect_calls(1)
            .and()
            // Mock 2: Connection greeting
            .on_event("imap_connection")
            .respond_with_actions(serde_json::json!([
                {
                    "type": "send_imap_response",
                    "response": "* OK IMAP4rev1 Server Ready"
                }
            ]))
            .expect_calls(1)
            .and()
            // Mock 3: LOGIN auth
            .on_event("imap_auth")
            .respond_with_actions(serde_json::json!([
                {
                    "type": "send_imap_response",
                    "response": "A001 OK LOGIN completed"
                }
            ]))
            .expect_calls(1)
            .and()
            // Mock 4: SELECT command
            .on_event("imap_command")
            .and_event_data_contains("command", "SELECT")
            .respond_with_actions(serde_json::json!([
                {
                    "type": "send_imap_response",
                    "response": "* 5 EXISTS\r\nA002 OK [READ-WRITE] SELECT completed"
                }
            ]))
            .expect_calls(1)
            .and()
            // Mock 5: SEARCH command
            .on_event("imap_command")
            .and_event_data_contains("command", "SEARCH")
            .respond_with_actions(serde_json::json!([
                {
                    "type": "send_imap_response",
                    "response": "* SEARCH 1 2 3 4 5\r\nA005 OK SEARCH completed"
                }
            ]))
            .expect_calls(1)
            .and()
    });

    let server = start_netget_server(config).await?;
    wait_for_server_startup(&server, Duration::from_secs(10), "IMAP").await?;

    let mut client = TcpStream::connect(format!("127.0.0.1:{}", server.port)).await?;
    let _greeting = read_greeting(&mut client).await?;

    // Login and select
    let _login_resp = send_imap_command(&mut client, "A001", "LOGIN alice secret").await?;
    let _select_resp = send_imap_command(&mut client, "A002", "SELECT INBOX").await?;

    // Search messages
    let responses = send_imap_command(&mut client, "A005", "SEARCH ALL").await?;

    // Check for SEARCH response
    let search_line = responses
        .iter()
        .find(|l| l.starts_with("* SEARCH"))
        .expect("Should have SEARCH response");

    assert!(
        search_line.contains("SEARCH"),
        "SEARCH should return results, got: {}",
        search_line
    );

    // Check for tagged OK response
    let ok_line = responses
        .iter()
        .find(|l| l.starts_with("A005 OK"))
        .expect("Should have tagged OK response");

    assert!(
        ok_line.contains("OK"),
        "SEARCH should succeed with OK, got: {}",
        ok_line
    );

    // Wait for the exchange the mocks describe, rather than trusting a fixed
    // sleep to have covered it. Under load the last event routinely lands after
    // the sleep expires, and the test reports it as never having happened.
    server.wait_for_mocks(30).await;
    server.verify_mocks().await?;
    server.stop().await?;
    Ok(())
}

#[tokio::test]
async fn test_imap_logout() -> E2EResult<()> {
    let prompt = "listen on port {AVAILABLE_PORT} via imap. \
         After LOGOUT, respond with: \
         * BYE IMAP4rev1 Server logging out\r\nA001 OK LOGOUT completed";

    let config = NetGetConfig::new(prompt).with_mock(|mock| {
        mock
            // Mock 1: Server startup
            .on_instruction_containing("listen on port")
            .and_instruction_containing("imap")
            .respond_with_actions(serde_json::json!([
                {
                    "type": "open_server",
                    "port": 0,
                    "base_stack": "IMAP",
                    "instruction": "Handle LOGOUT with BYE message"
                }
            ]))
            .expect_calls(1)
            .and()
            // Mock 2: Connection greeting
            .on_event("imap_connection")
            .respond_with_actions(serde_json::json!([
                {
                    "type": "send_imap_response",
                    "response": "* OK IMAP4rev1 Server Ready"
                }
            ]))
            .expect_calls(1)
            .and()
            // Mock 3: LOGOUT command
            .on_event("imap_command")
            .and_event_data_contains("command", "LOGOUT")
            .respond_with_actions(serde_json::json!([
                {
                    "type": "send_imap_response",
                    "response": "* BYE IMAP4rev1 Server logging out\r\nA001 OK LOGOUT completed"
                }
            ]))
            .expect_calls(1)
            .and()
    });

    let server = start_netget_server(config).await?;
    wait_for_server_startup(&server, Duration::from_secs(10), "IMAP").await?;

    let mut client = TcpStream::connect(format!("127.0.0.1:{}", server.port)).await?;
    let _greeting = read_greeting(&mut client).await?;

    // Send LOGOUT command
    let responses = send_imap_command(&mut client, "A001", "LOGOUT").await?;

    // Check for BYE response
    let has_bye = responses.iter().any(|l| l.contains("BYE"));
    assert!(has_bye, "LOGOUT should include BYE, got: {:?}", responses);

    // Check for tagged OK response
    let ok_line = responses
        .iter()
        .find(|l| l.starts_with("A001 OK"))
        .expect("Should have tagged OK response");

    assert!(
        ok_line.contains("OK"),
        "LOGOUT should succeed with OK, got: {}",
        ok_line
    );

    // Wait for the exchange the mocks describe, rather than trusting a fixed
    // sleep to have covered it. Under load the last event routinely lands after
    // the sleep expires, and the test reports it as never having happened.
    server.wait_for_mocks(30).await;
    server.verify_mocks().await?;
    server.stop().await?;
    Ok(())
}

#[tokio::test]
async fn test_imap_noop() -> E2EResult<()> {
    let prompt = "listen on port {AVAILABLE_PORT} via imap. Allow LOGIN for 'alice'. \
         NOOP command should respond with A003 OK NOOP completed";

    let config = NetGetConfig::new(prompt).with_mock(|mock| {
        mock
            // Mock 1: Server startup
            .on_instruction_containing("listen on port")
            .and_instruction_containing("imap")
            .respond_with_actions(serde_json::json!([
                {
                    "type": "open_server",
                    "port": 0,
                    "base_stack": "IMAP",
                    "instruction": "Allow LOGIN alice, handle NOOP"
                }
            ]))
            .expect_calls(1)
            .and()
            // Mock 2: Connection greeting
            .on_event("imap_connection")
            .respond_with_actions(serde_json::json!([
                {
                    "type": "send_imap_response",
                    "response": "* OK IMAP4rev1 Server Ready"
                }
            ]))
            .expect_calls(1)
            .and()
            // Mock 3: LOGIN auth
            .on_event("imap_auth")
            .respond_with_actions(serde_json::json!([
                {
                    "type": "send_imap_response",
                    "response": "A001 OK LOGIN completed"
                }
            ]))
            .expect_calls(1)
            .and()
            // Mock 4: NOOP command
            .on_event("imap_command")
            .and_event_data_contains("command", "NOOP")
            .respond_with_actions(serde_json::json!([
                {
                    "type": "send_imap_response",
                    "response": "A003 OK NOOP completed"
                }
            ]))
            .expect_calls(1)
            .and()
    });

    let server = start_netget_server(config).await?;
    wait_for_server_startup(&server, Duration::from_secs(10), "IMAP").await?;

    let mut client = TcpStream::connect(format!("127.0.0.1:{}", server.port)).await?;
    let _greeting = read_greeting(&mut client).await?;

    // Login first
    let _login_resp = send_imap_command(&mut client, "A001", "LOGIN alice secret").await?;

    // Send NOOP command
    let responses = send_imap_command(&mut client, "A003", "NOOP").await?;

    // Check for tagged OK response
    let ok_line = responses
        .iter()
        .find(|l| l.starts_with("A003 OK"))
        .expect("Should have tagged OK response");

    assert!(
        ok_line.contains("OK"),
        "NOOP should succeed with OK, got: {}",
        ok_line
    );

    // Wait for the exchange the mocks describe, rather than trusting a fixed
    // sleep to have covered it. Under load the last event routinely lands after
    // the sleep expires, and the test reports it as never having happened.
    server.wait_for_mocks(30).await;
    server.verify_mocks().await?;
    server.stop().await?;
    Ok(())
}

#[tokio::test]
async fn test_imap_status() -> E2EResult<()> {
    let prompt = "listen on port {AVAILABLE_PORT} via imap. Allow LOGIN for 'alice'. \
         After STATUS INBOX (MESSAGES RECENT), respond with: \
         * STATUS \"INBOX\" (MESSAGES 5 RECENT 2)\r\nA004 OK STATUS completed";

    let config = NetGetConfig::new(prompt)
        .with_mock(|mock| {
            mock
                // Mock 1: Server startup
                .on_instruction_containing("listen on port")
                .and_instruction_containing("imap")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "open_server",
                        "port": 0,
                        "base_stack": "IMAP",
                        "instruction": "Allow LOGIN alice, INBOX has 5 messages 2 recent"
                    }
                ]))
                .expect_calls(1)
                .and()
                // Mock 2: Connection greeting
                .on_event("imap_connection")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "send_imap_response",
                        "response": "* OK IMAP4rev1 Server Ready"
                    }
                ]))
                .expect_calls(1)
                .and()
                // Mock 3: LOGIN auth
                .on_event("imap_auth")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "send_imap_response",
                        "response": "A001 OK LOGIN completed"
                    }
                ]))
                .expect_calls(1)
                .and()
                // Mock 4: STATUS command
                .on_event("imap_command")
                .and_event_data_contains("command", "STATUS")
                .respond_with_actions(serde_json::json!([
                    {
                        "type": "send_imap_response",
                        "response": "* STATUS \"INBOX\" (MESSAGES 5 RECENT 2)\r\nA004 OK STATUS completed"
                    }
                ]))
                .expect_calls(1)
                .and()
        });

    let server = start_netget_server(config).await?;
    wait_for_server_startup(&server, Duration::from_secs(10), "IMAP").await?;

    let mut client = TcpStream::connect(format!("127.0.0.1:{}", server.port)).await?;
    let _greeting = read_greeting(&mut client).await?;

    // Login first
    let _login_resp = send_imap_command(&mut client, "A001", "LOGIN alice secret").await?;

    // Send STATUS command
    let responses =
        send_imap_command(&mut client, "A004", "STATUS INBOX (MESSAGES RECENT)").await?;

    // Check for STATUS response
    let status_line = responses
        .iter()
        .find(|l| l.starts_with("* STATUS"))
        .expect("Should have STATUS response");

    assert!(
        status_line.contains("STATUS") && status_line.contains("INBOX"),
        "STATUS should return mailbox info, got: {}",
        status_line
    );

    // Check for tagged OK response
    let ok_line = responses
        .iter()
        .find(|l| l.starts_with("A004 OK"))
        .expect("Should have tagged OK response");

    assert!(
        ok_line.contains("OK"),
        "STATUS should succeed with OK, got: {}",
        ok_line
    );

    // Wait for the exchange the mocks describe, rather than trusting a fixed
    // sleep to have covered it. Under load the last event routinely lands after
    // the sleep expires, and the test reports it as never having happened.
    server.wait_for_mocks(30).await;
    server.verify_mocks().await?;
    server.stop().await?;
    Ok(())
}
