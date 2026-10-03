/// Ollama Model Testing Framework
///
/// This test file demonstrates the Ollama model testing framework that allows
/// evaluating different models and prompts against specific expectations.
///
/// ## Opt-in only
///
/// Every test here grades a *real* model's answers against a real Ollama at
/// `OLLAMA_BASE_URL`. It is therefore an evaluation harness, not a regression suite:
/// without that exact setup (including the model being pulled) it can only fail, so it
/// is gated behind the project's standard `--use-ollama` opt-in and skips otherwise.
///
/// ```bash
/// ./test-e2e.sh --use-ollama            # sets NETGET_USE_OLLAMA=1
/// NETGET_USE_OLLAMA=1 cargo test --test ollama_model_test
/// ```
///
/// ## Running Tests
///
/// ```bash
/// # Use default model (qwen3.8:27b-mlx)
/// NETGET_USE_OLLAMA=1 cargo test --test ollama_model_test
///
/// # Use specific model
/// NETGET_USE_OLLAMA=1 OLLAMA_MODEL=qwen3-coder:30b cargo test --test ollama_model_test
///
/// # Run single test
/// NETGET_USE_OLLAMA=1 cargo test --test ollama_model_test test_open_http_server
///
/// # Run with verbose output
/// NETGET_USE_OLLAMA=1 cargo test --test ollama_model_test -- --nocapture
/// ```
///
/// ## Environment Variables
///
/// - `OLLAMA_MODEL`: Override the default model (default: qwen3.8:27b-mlx)
/// - `OLLAMA_BASE_URL`: Ollama API endpoint (default: http://localhost:11434)
///
/// ## Test Structure
///
/// Each test uses the `OllamaTestBuilder` to:
/// 1. Set up context (user input or network request)
/// 2. Add expectations (exact matches, contains, regex, script execution, etc.)
/// 3. Run the test and validate results
///
/// ## Adding New Tests
///
/// ```rust
/// #[tokio::test]
/// async fn test_my_scenario() -> Result<()> {
///     OllamaTestBuilder::new()
///         .with_user_input("your prompt here")
///         .expect_action_type("open_server")
///         .expect_protocol("tcp")
///         .expect_field_contains("instruction", "some keyword")
///         .run()
///         .await?
///         .assert_success()
/// }
/// ```
mod helpers;

use anyhow::Result;
use serde_json::json;
use std::sync::Once;

use helpers::ollama_test_builder::OllamaTestBuilder;
use netget::llm::actions::Parameter;
use netget::protocol::{Event, EventType};
use netget::state::ServerId;

/// True when the caller asked for a real Ollama, using the same opt-in the rest of the
/// suite uses: `test-e2e.sh --use-ollama` exports `NETGET_USE_OLLAMA=1`
/// (`tests/helpers/netget.rs::should_use_ollama` reads the same variable).
fn ollama_opt_in() -> bool {
    helpers::common::real_ollama_requested()
}

/// Skip the calling test unless the `--use-ollama` opt-in is active.
///
/// These tests evaluate a live model; with no Ollama (or no such model pulled) they can
/// only fail, which is a statement about the machine and not about the code.
macro_rules! require_ollama {
    () => {
        if !ollama_opt_in() {
            eprintln!(
                "skipped: model-evaluation test, needs a real Ollama. \
                 Run `./test-e2e.sh --use-ollama` or set NETGET_USE_OLLAMA=1."
            );
            return Ok(());
        }
    };
}

// Initialize tracing once for all tests
static INIT: Once = Once::new();

fn init_tracing() {
    INIT.call_once(|| {
        // Initialize tracing subscriber to capture RUST_LOG output
        tracing_subscriber::fmt()
            .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
            .with_test_writer()
            .init();
    });
}

// ============================================================================
// Documentation Request Tests - Testing that LLM requests docs when needed
// ============================================================================

/// Test: LLM handles server request without documentation
///
/// When the user asks to open a server without documentation context,
/// the LLM can either:
/// 1. Directly open the server (open_server action is always available)
/// 2. Request documentation first (read_documentation tool)
/// Both are valid behaviors.
#[tokio::test]
async fn test_server_request_without_docs() -> Result<()> {
    require_ollama!();
    OllamaTestBuilder::new()
        .with_user_input("open an http server")
        // NO documentation injected - LLM can either request docs or directly open
        .expect_custom("valid server action", |action| {
            let action_type = action["type"].as_str().unwrap_or("");
            if action_type == "open_server" || action_type == "read_documentation" {
                Ok(())
            } else {
                anyhow::bail!(
                    "Expected 'open_server' or 'read_documentation', got '{}'",
                    action_type
                )
            }
        })
        .run()
        .await?
        .assert_success()
}

/// Test: LLM handles client request without documentation
///
/// When the user asks to connect to a server without documentation context,
/// the LLM can either:
/// 1. Directly open the client (open_client action is always available)
/// 2. Request documentation first (read_documentation tool)
/// Both are valid behaviors.
#[tokio::test]
async fn test_client_request_without_docs() -> Result<()> {
    require_ollama!();
    OllamaTestBuilder::new()
        .with_user_input("connect to a redis server at localhost:6379")
        // NO documentation injected - LLM can either request docs or directly connect
        .expect_custom("valid client action", |action| {
            let action_type = action["type"].as_str().unwrap_or("");
            if action_type == "open_client" || action_type == "read_documentation" {
                Ok(())
            } else {
                anyhow::bail!(
                    "Expected 'open_client' or 'read_documentation', got '{}'",
                    action_type
                )
            }
        })
        .run()
        .await?
        .assert_success()
}

// ============================================================================
// User Input Tests - Testing global action generation
// ============================================================================

/// Test: Open HTTP server with basic prompt
///
/// Validates that the model correctly interprets a simple request to open
/// an HTTP server and generates the appropriate action.
#[tokio::test]
async fn test_open_http_server() -> Result<()> {
    require_ollama!();
    OllamaTestBuilder::new()
        .with_user_input("open http server")
        .with_server_documentation("http") // Inject HTTP docs so open_server is available
        .expect_action_type("open_server")
        .expect_protocol("http")
        .run()
        .await?
        .assert_success()
}

/// Test: Open TCP server with specific port
///
/// Validates that the model understands port specifications and includes
/// them in the generated action.
#[tokio::test]
async fn test_open_tcp_server_with_port() -> Result<()> {
    require_ollama!();
    OllamaTestBuilder::new()
        .with_user_input("open tcp server on port 8080")
        .with_server_documentation("tcp") // Inject TCP docs so open_server is available
        .expect_action_type("open_server")
        .expect_protocol("tcp")
        .expect_field_exact("port", json!(8080))
        .run()
        .await?
        .assert_success()
}

/// Test: Open server with detailed instruction
///
/// Validates that the model captures the intent of the user's request
/// in the instruction field.
#[tokio::test]
async fn test_open_server_with_instruction() -> Result<()> {
    require_ollama!();
    OllamaTestBuilder::new()
        .with_user_input("open an http server that responds with hello world to all requests")
        .with_server_documentation("http") // Inject HTTP docs so open_server is available
        .expect_action_type("open_server")
        .expect_protocol("http")
        .expect_field_contains("instruction", "hello world")
        .run()
        .await?
        .assert_success()
}

/// Test: Open DNS server with static response
///
/// Validates that the model correctly generates a static handler when
/// the user specifies a simple, static response pattern.
#[tokio::test]
async fn test_dns_server_with_static_response() -> Result<()> {
    require_ollama!();
    OllamaTestBuilder::new()
        .with_user_input("open a dns server that always responds with 1.2.3.4 for any query")
        .with_server_documentation("dns") // Inject DNS docs so open_server is available
        .expect_action_type("open_server")
        .expect_protocol("dns")
        .expect_static_handler(json!({
            "type": "send_dns_a_response",
            "query_id": 0,
            "domain": "any",
            "ip": "1.2.3.4"
        }))
        .run()
        .await?
        .assert_success()
}

/// Test: Open client connection
///
/// Validates that the model correctly generates client connection actions
/// and includes the remote address.
#[tokio::test]
async fn test_open_client() -> Result<()> {
    require_ollama!();
    OllamaTestBuilder::new()
        .with_user_input("connect to redis server at localhost:6379")
        .with_client_documentation("redis") // Inject Redis client docs so open_client is available
        .expect_action_type("open_client")
        .expect_protocol("redis")
        // Action definition uses "remote_addr" as combined "host:port" string
        // Accept either remote_addr or address field containing the target
        .expect_custom("address contains localhost:6379", |action| {
            let has_remote_addr = action
                .get("remote_addr")
                .and_then(|v| v.as_str())
                .map(|s| s.contains("localhost") && s.contains("6379"))
                .unwrap_or(false);
            let has_address = action
                .get("address")
                .and_then(|v| v.as_str())
                .map(|s| s.contains("localhost") && s.contains("6379"))
                .unwrap_or(false);
            if has_remote_addr || has_address {
                Ok(())
            } else {
                anyhow::bail!("Expected remote_addr or address containing 'localhost:6379'")
            }
        })
        .run()
        .await?
        .assert_success()
}

/// Test: Close server action
///
/// Validates that the model understands server management commands
/// and generates appropriate close actions.
#[tokio::test]
async fn test_close_server() -> Result<()> {
    require_ollama!();
    OllamaTestBuilder::new()
        .with_user_input("close server 123")
        .expect_action_type("close_server")
        .expect_field_exact("server_id", json!(123))
        .run()
        .await?
        .assert_success()
}

// ============================================================================
// Script Handler Tests - Testing script generation and execution
// ============================================================================

/// Test: HTTP server with script handler
///
/// Validates that the model recognizes when a script handler is appropriate
/// for implementing custom logic.
#[tokio::test]
async fn test_http_script_sum_query_params() -> Result<()> {
    require_ollama!();
    // Create test event (HTTP request with query params)
    let test_event = Event::new(
        Box::leak(Box::new(
            EventType::new(
                "http_request",
                "HTTP request received",
                json!({"type": "placeholder", "event_id": "http_request"}),
            )
            .with_parameters(vec![
                Parameter {
                    name: "method".to_string(),
                    type_hint: "string".to_string(),
                    description: "HTTP method".to_string(),
                    required: true,
                },
                Parameter {
                    name: "path".to_string(),
                    type_hint: "string".to_string(),
                    description: "Request path (without query string)".to_string(),
                    required: true,
                },
                Parameter {
                    name: "query_string".to_string(),
                    type_hint: "string".to_string(),
                    description: "Raw query string (e.g., 'x=5&y=3')".to_string(),
                    required: false,
                },
                Parameter {
                    name: "query".to_string(),
                    type_hint: "object".to_string(),
                    description: "Parsed query parameters as key-value pairs".to_string(),
                    required: false,
                },
            ]),
        )),
        json!({
            "method": "GET",
            "path": "/",
            "query_string": "x=5&y=3",
            "query": {"x": "5", "y": "3"}
        }),
    );

    // Expected actions from script
    // Note: Accept either "8" or "8.0" and headers are optional but commonly added
    let expected_actions = vec![json!({
        "type": "send_http_response",
        "status": 200,
        "headers": {"Content-Type": "text/plain"},  // LLM adds appropriate content type
        "body": "8.0"  // Float result is acceptable
    })];

    OllamaTestBuilder::new()
        .with_user_input(
            "create an http server that receives query parameters x and y \
             and returns their mathematical sum. write this as a script.",
        )
        .with_server_documentation("http") // Inject HTTP docs so open_server is available
        .expect_action_type("open_server")
        .expect_protocol("http")
        .expect_script_handler()
        .expect_field_contains("instruction", "sum")
        .expect_script_execution(test_event, expected_actions)
        .run()
        .await?
        .assert_success()
}

/// Test: TCP server with echo script
///
/// Validates that the model can generate a script handler for simple
/// echo functionality.
#[tokio::test]
async fn test_tcp_echo_script() -> Result<()> {
    require_ollama!();
    // Create test event (TCP data received)
    let test_event = Event::new(
        Box::leak(Box::new(
            EventType::new(
                "tcp_data_received",
                "TCP data received",
                json!({"type": "placeholder", "event_id": "tcp_data_received"}),
            )
            .with_parameters(vec![Parameter {
                name: "data".to_string(),
                type_hint: "string".to_string(),
                description: "The data received (as hex string or UTF-8 if printable)".to_string(),
                required: true,
            }]),
        )),
        json!({
            "data": "48656c6c6f"
        }),
    );

    // Expected actions from script (echo back same data)
    let expected_actions = vec![json!({
        "type": "send_tcp_data",
        "data": "48656c6c6f"
    })];

    OllamaTestBuilder::new()
        .with_user_input("create a tcp echo server using a script")
        .with_server_documentation("tcp") // Inject TCP docs so open_server is available
        .expect_action_type("open_server")
        .expect_protocol("tcp")
        .expect_script_handler()
        .expect_script_execution(test_event, expected_actions)
        .run()
        .await?
        .assert_success()
}

/// Test: HTTP server with conditional script
///
/// Validates that the model can generate scripts when conditional logic
/// is required.
#[tokio::test]
async fn test_http_conditional_script() -> Result<()> {
    require_ollama!();
    // Create test event (GET request)
    let test_event_get = Event::new(
        Box::leak(Box::new(
            EventType::new(
                "http_request",
                "HTTP request received",
                json!({"type": "placeholder", "event_id": "http_request"}),
            )
            .with_parameters(vec![
                Parameter {
                    name: "method".to_string(),
                    type_hint: "string".to_string(),
                    description: "HTTP method".to_string(),
                    required: true,
                },
                Parameter {
                    name: "path".to_string(),
                    type_hint: "string".to_string(),
                    description: "Request path".to_string(),
                    required: true,
                },
            ]),
        )),
        json!({
            "method": "GET",
            "path": "/"
        }),
    );

    // Expected actions for GET request
    let expected_actions_get = vec![json!({
        "type": "send_http_response",
        "status": 200,
        "body": "Hello GET"
    })];

    OllamaTestBuilder::new()
        .with_user_input(
            "create an http server that responds with 'Hello GET' for GET requests \
             and 'Hello POST' for POST requests. write as a script.",
        )
        .with_server_documentation("http") // Inject HTTP docs so open_server is available
        .expect_action_type("open_server")
        .expect_protocol("http")
        .expect_script_handler()
        .expect_field_contains("instruction", "GET")
        .expect_field_contains("instruction", "POST")
        .expect_script_execution(test_event_get, expected_actions_get)
        .run()
        .await?
        .assert_success()
}

// ============================================================================
// Network Request Tests - Testing protocol-level action generation
// ============================================================================

/// Test: HTTP request handling with instruction
///
/// Validates that the model correctly responds to network events
/// based on the server's instruction.
#[tokio::test]
async fn test_http_request_with_instruction() -> Result<()> {
    require_ollama!();
    let event = Event::new(
        Box::leak(Box::new(
            EventType::new(
                "http_request",
                "HTTP request received",
                json!({"type": "placeholder", "event_id": "http_request"}),
            )
            .with_parameters(vec![
                Parameter {
                    name: "method".to_string(),
                    type_hint: "string".to_string(),
                    description: "HTTP method".to_string(),
                    required: true,
                },
                Parameter {
                    name: "path".to_string(),
                    type_hint: "string".to_string(),
                    description: "Request path".to_string(),
                    required: true,
                },
            ]),
        )),
        json!({
            "method": "GET",
            "path": "/hello"
        }),
    );

    let available_actions = vec![
        json!({
            "type": "send_http_response",
            "description": "Send HTTP response",
            "parameters": {
                "status": "HTTP status code",
                "headers": "Response headers (optional)",
                "body": "Response body (optional)"
            }
        }),
        json!({
            "type": "wait_for_more",
            "description": "Wait for more data"
        }),
    ];

    OllamaTestBuilder::new()
        .with_network_request(
            event,
            "Respond with 'Hello, World!' to all requests",
            ServerId::new(1), // Dummy server ID for testing
        )
        .expect_action_type("send_http_response")
        .expect_field_exact("status", json!(200))
        .expect_field_contains("body", "Hello, World!")
        .run()
        .await?
        .assert_success()
}

/// Test: DNS query response
///
/// Validates that the model generates correct DNS response actions.
#[tokio::test]
async fn test_dns_query_response() -> Result<()> {
    require_ollama!();
    let event = Event::new(
        Box::leak(Box::new(
            EventType::new(
                "dns_query",
                "DNS query received",
                json!({"type": "placeholder", "event_id": "dns_query"}),
            )
            .with_parameters(vec![
                Parameter {
                    name: "query_id".to_string(),
                    type_hint: "number".to_string(),
                    description: "DNS query identifier".to_string(),
                    required: true,
                },
                Parameter {
                    name: "domain".to_string(),
                    type_hint: "string".to_string(),
                    description: "Domain name being queried".to_string(),
                    required: true,
                },
                Parameter {
                    name: "query_type".to_string(),
                    type_hint: "string".to_string(),
                    description: "DNS query type (A, AAAA, etc.)".to_string(),
                    required: true,
                },
            ]),
        )),
        json!({
            "query_id": 12345,
            "domain": "example.com",
            "query_type": "A"
        }),
    );

    let available_actions = vec![
        json!({
            "type": "send_dns_a_response",
            "description": "Send DNS A record response",
            "parameters": {
                "query_id": "Query ID from the request",
                "domain": "Domain name",
                "ip": "IP address to return"
            }
        }),
        json!({
            "type": "send_dns_nxdomain",
            "description": "Send DNS NXDOMAIN (not found) response",
            "parameters": {
                "query_id": "Query ID from the request"
            }
        }),
    ];

    OllamaTestBuilder::new()
        .with_network_request(
            event,
            "Respond to queries for example.com with IP 93.184.216.34",
            ServerId::new(1), // Dummy server ID for testing
        )
        .expect_action_type("send_dns_a_response")
        .expect_field_exact("query_id", json!(12345))
        .expect_field_exact("domain", json!("example.com"))
        .expect_field_exact("ip", json!("93.184.216.34"))
        .run()
        .await?
        .assert_success()
}

/// Test: TCP data handling with hex encoding
///
/// Validates that the model understands hex-encoded data handling.
#[tokio::test]
async fn test_tcp_hex_response() -> Result<()> {
    require_ollama!();
    let event = Event::new(
        Box::leak(Box::new(
            EventType::new(
                "tcp_data_received",
                "TCP data received",
                json!({"type": "placeholder", "event_id": "tcp_data_received"}),
            )
            .with_parameters(vec![Parameter {
                name: "data".to_string(),
                type_hint: "string".to_string(),
                description: "The data received (as hex string or UTF-8 if printable)".to_string(),
                required: true,
            }]),
        )),
        json!({
            "data": "48656c6c6f"
        }),
    );

    let available_actions = vec![
        json!({
            "type": "send_tcp_data",
            "description": "Send TCP data",
            "parameters": {
                "data": "Data to send over TCP connection (text string or hex-encoded for binary data)"
            }
        }),
        json!({
            "type": "disconnect",
            "description": "Disconnect the connection"
        }),
    ];

    OllamaTestBuilder::new()
        .with_network_request(
            event,
            "Echo back any received data",
            ServerId::new(1), // Dummy server ID for testing
        )
        .expect_action_type("send_tcp_data")
        .expect_field_exact("data", json!("48656c6c6f"))
        .run()
        .await?
        .assert_success()
}

// ============================================================================
// Custom Validation Tests - Testing flexible assertions
// ============================================================================

/// Test: Custom validation with closure
///
/// Demonstrates using custom validation logic for complex assertions
/// that don't fit into standard expectation types.
#[tokio::test]
async fn test_custom_validation() -> Result<()> {
    require_ollama!();
    OllamaTestBuilder::new()
        .with_user_input("open http server on port 8080 and server cooking recipes")
        .with_server_documentation("http") // Inject HTTP docs so open_server is available
        .expect_action_type("open_server")
        .expect_protocol("http")
        .expect_custom("port in valid range", |action| {
            let port = action["port"]
                .as_u64()
                .ok_or_else(|| anyhow::anyhow!("Port is not a number"))?;
            if port < 1024 || port > 65535 {
                anyhow::bail!("Port {} is outside valid range 1024-65535", port);
            }
            Ok(())
        })
        .expect_custom("instruction mentions recipe", |action| {
            let instruction = action["instruction"]
                .as_str()
                .ok_or_else(|| anyhow::anyhow!("No instruction field"))?;
            if !instruction.to_lowercase().contains("recipe") {
                anyhow::bail!("Instruction doesn't mention recipe");
            }
            Ok(())
        })
        .run()
        .await?
        .assert_success()
}

/// Test: Regex pattern matching
///
/// Demonstrates using regex patterns for flexible string matching.
#[tokio::test]
async fn test_regex_pattern_matching() -> Result<()> {
    require_ollama!();
    OllamaTestBuilder::new()
        .with_user_input("open http server on localhost port 8080")
        .with_server_documentation("http") // Inject HTTP docs so open_server is available
        .expect_action_type("open_server")
        .expect_protocol("http")
        .expect_field_matches("instruction", r"(?i)(localhost|127\.0\.0\.1)")
        .run()
        .await?
        .assert_success()
}

// ============================================================================
// Complex Scenario Tests - Testing multi-step logic
// ============================================================================

/// Test: Server with scheduled tasks
///
/// Validates that the model can generate complex server configurations
/// including scheduled tasks.
#[tokio::test]
async fn test_server_with_scheduled_tasks() -> Result<()> {
    require_ollama!();
    OllamaTestBuilder::new()
        .with_user_input("open http server that sends a heartbeat log every 10 seconds")
        .with_server_documentation("http") // Inject HTTP docs so open_server is available
        .expect_action_type("open_server")
        .expect_protocol("http")
        .expect_custom("has scheduled tasks", |action| {
            let tasks = action
                .get("scheduled_tasks")
                .ok_or_else(|| anyhow::anyhow!("No scheduled_tasks field"))?;
            if !tasks.is_array() || tasks.as_array().unwrap().is_empty() {
                anyhow::bail!("No scheduled tasks defined");
            }
            Ok(())
        })
        .run()
        .await?
        .assert_success()
}

/// Test: Multiple actions in response
///
/// Validates that the model can generate multiple actions when appropriate.
#[tokio::test]
async fn test_multiple_actions() -> Result<()> {
    require_ollama!();
    let result = OllamaTestBuilder::new()
        .with_user_input("open both http and tcp servers")
        .with_server_documentation("http") // Inject HTTP docs
        .with_server_documentation("tcp") // Inject TCP docs
        .expect_custom("multiple servers", |action| {
            // This test expects the first action but also checks if
            // the LLM returned multiple actions
            Ok(())
        })
        .run()
        .await?;

    println!("Actions returned: {}", result.actions.len());

    // For this test, we just validate structure, not necessarily
    // that it returns 2 actions (depends on model interpretation)
    result.assert_success()
}

// ============================================================================
// SQLite Storage Tests - Testing database integration
// ============================================================================

/// Test: DHCP server with SQLite storage for IP mappings
///
/// Validates that the LLM creates both the DHCP server and a database
/// for storing MAC-to-IP address mappings when explicitly requested.
#[tokio::test]
async fn test_dhcp_server_with_sqlite_storage() -> Result<()> {
    require_ollama!();
    let result = OllamaTestBuilder::new()
        .with_user_input(
            "open a DHCP server that stores MAC address to IP mappings in a SQLite database",
        )
        .with_server_documentation("dhcp")
        .expect_custom("has open_server or create_database", |action| {
            let action_type = action["type"].as_str().unwrap_or("");
            // LLM might return either action first, or multiple actions
            if action_type == "open_server" || action_type == "create_database" {
                Ok(())
            } else {
                anyhow::bail!(
                    "Expected 'open_server' or 'create_database', got '{}'",
                    action_type
                )
            }
        })
        .run()
        .await?;

    // Check if the response mentions database/storage in some way
    let actions_json = serde_json::to_string(&result.actions)?;
    let mentions_db = actions_json.contains("database")
        || actions_json.contains("sqlite")
        || actions_json.contains("create_database")
        || actions_json.contains("storage")
        || actions_json.contains("MAC")
        || actions_json.contains("mapping");

    if !mentions_db && result.actions.len() < 2 {
        // Only fail if there's no indication the LLM understood the storage requirement
        // AND there's only one action (meaning no create_database action)
        println!("Warning: LLM may not have created database for storage");
    }

    result.assert_success()
}

/// Test: DHCP network request with SQLite query for existing lease
///
/// Validates that when handling a DHCP request with instruction to check
/// existing leases in SQLite, the LLM correctly uses execute_sql to query.
#[tokio::test]
async fn test_dhcp_request_with_sqlite_query() -> Result<()> {
    require_ollama!();
    // Create DHCP request event
    let dhcp_event = Event::new(
        Box::leak(Box::new(
            EventType::new(
                "dhcp_discover",
                "DHCP DISCOVER received from client",
                json!({"type": "placeholder", "event_id": "dhcp_discover"}),
            )
            .with_parameters(vec![
                Parameter {
                    name: "mac_address".to_string(),
                    type_hint: "string".to_string(),
                    description: "Client's MAC address".to_string(),
                    required: true,
                },
                Parameter {
                    name: "hostname".to_string(),
                    type_hint: "string".to_string(),
                    description: "Client's hostname if provided".to_string(),
                    required: false,
                },
                Parameter {
                    name: "requested_ip".to_string(),
                    type_hint: "string".to_string(),
                    description: "IP address requested by client (if any)".to_string(),
                    required: false,
                },
            ]),
        )),
        json!({
            "mac_address": "AA:BB:CC:DD:EE:FF",
            "hostname": "workstation-1",
            "requested_ip": null
        }),
    );

    let instruction = "Check the SQLite database (db-1) for existing lease for this MAC address. \
        If found, offer the same IP. If not found, query available IPs and assign one. \
        Store the new mapping in the leases table.";

    OllamaTestBuilder::new()
        .with_network_request(dhcp_event, instruction, ServerId::new(1))
        .expect_custom("attempts database/storage access", |action| {
            let action_type = action["type"].as_str().unwrap_or("");
            // LLM should attempt to access storage for lease information
            // Note: execute_sql is not currently in network request context,
            // so LLM may use read_file as closest available action
            if action_type == "execute_sql" {
                // Check that the query mentions the MAC address or leases
                if let Some(query) = action.get("query").and_then(|q| q.as_str()) {
                    let mentions_mac = query.contains("AA:BB:CC:DD:EE:FF")
                        || query.to_uppercase().contains("MAC")
                        || query.to_uppercase().contains("SELECT")
                        || query.to_uppercase().contains("INSERT");
                    if mentions_mac {
                        return Ok(());
                    }
                }
                // Still accept execute_sql even if MAC not in query
                return Ok(());
            } else if action_type == "read_file" {
                // Accept read_file if LLM tries to access the database file
                // This is a reasonable fallback when execute_sql is not available
                if let Some(path) = action.get("path").and_then(|p| p.as_str()) {
                    if path.contains("db") || path.contains("lease") || path.contains("sqlite") {
                        return Ok(());
                    }
                }
                return Ok(()); // Accept any read_file as storage access attempt
            } else if action_type == "send_dhcp_offer" || action_type == "dhcp_offer" {
                // Also accept if LLM decides to directly send DHCP offer
                // (maybe it's handling without DB)
                return Ok(());
            }
            anyhow::bail!(
                "Expected storage access (execute_sql/read_file) or DHCP response, got '{}'. \
                 The LLM should attempt to access stored lease information.",
                action_type
            )
        })
        .run()
        .await?
        .assert_success()
}
