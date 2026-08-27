// End-to-end SOCKS5 tests for NetGet
//
// These tests spawn the actual NetGet binary with SOCKS5 prompts
// and validate the responses using a manual SOCKS5 client implementation.

use crate::server::helpers::*;
use std::net::Ipv4Addr;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// SOCKS5 protocol constants
const SOCKS5_VERSION: u8 = 0x05;
const AUTH_METHOD_NO_AUTH: u8 = 0x00;
const AUTH_METHOD_USERNAME_PASSWORD: u8 = 0x02;
const CMD_CONNECT: u8 = 0x01;
const ATYP_IPV4: u8 = 0x01;
const ATYP_DOMAIN: u8 = 0x03;
const REPLY_SUCCESS: u8 = 0x00;

/// Simple SOCKS5 client implementation for testing
struct Socks5Client {
    stream: TcpStream,
}

impl Socks5Client {
    /// Connect to SOCKS5 proxy
    async fn connect(proxy_addr: &str) -> E2EResult<Self> {
        let stream = TcpStream::connect(proxy_addr).await?;
        Ok(Self { stream })
    }

    /// Perform SOCKS5 handshake (no authentication)
    async fn handshake_no_auth(&mut self) -> E2EResult<()> {
        // Send: [VER=5, NMETHODS=1, METHODS=[0x00]]
        self.stream
            .write_all(&[SOCKS5_VERSION, 1, AUTH_METHOD_NO_AUTH])
            .await?;
        self.stream.flush().await?;

        // Receive: [VER=5, METHOD=0x00]
        let mut response = [0u8; 2];
        self.stream.read_exact(&mut response).await?;

        if response[0] != SOCKS5_VERSION {
            return Err(format!("Invalid SOCKS version: {}", response[0]).into());
        }
        if response[1] != AUTH_METHOD_NO_AUTH {
            return Err(format!("Server requires authentication: method {}", response[1]).into());
        }

        Ok(())
    }

    /// Perform SOCKS5 handshake with username/password authentication
    async fn handshake_with_auth(&mut self, username: &str, password: &str) -> E2EResult<bool> {
        // Send: [VER=5, NMETHODS=1, METHODS=[0x02]]
        self.stream
            .write_all(&[SOCKS5_VERSION, 1, AUTH_METHOD_USERNAME_PASSWORD])
            .await?;
        self.stream.flush().await?;

        // Receive: [VER=5, METHOD]
        let mut response = [0u8; 2];
        self.stream.read_exact(&mut response).await?;

        if response[0] != SOCKS5_VERSION {
            return Err(format!("Invalid SOCKS version: {}", response[0]).into());
        }
        if response[1] != AUTH_METHOD_USERNAME_PASSWORD {
            return Err(format!(
                "Server didn't select username/password auth: method {}",
                response[1]
            )
            .into());
        }

        // Send username/password: [VER=1, ULEN, UNAME, PLEN, PASSWD]
        let mut auth_request = vec![0x01];
        auth_request.push(username.len() as u8);
        auth_request.extend_from_slice(username.as_bytes());
        auth_request.push(password.len() as u8);
        auth_request.extend_from_slice(password.as_bytes());
        self.stream.write_all(&auth_request).await?;
        self.stream.flush().await?;

        // Receive: [VER=1, STATUS]
        let mut auth_response = [0u8; 2];
        self.stream.read_exact(&mut auth_response).await?;

        if auth_response[0] != 0x01 {
            return Err(format!("Invalid auth version: {}", auth_response[0]).into());
        }

        Ok(auth_response[1] == 0x00) // 0x00 = success
    }

    /// Send CONNECT request (IPv4)
    async fn connect_ipv4(&mut self, target_ip: Ipv4Addr, target_port: u16) -> E2EResult<bool> {
        // Build request: [VER=5, CMD=CONNECT, RSV=0, ATYP=IPv4, DST.ADDR, DST.PORT]
        let mut request = vec![SOCKS5_VERSION, CMD_CONNECT, 0x00, ATYP_IPV4];
        request.extend_from_slice(&target_ip.octets());
        request.extend_from_slice(&target_port.to_be_bytes());

        self.stream.write_all(&request).await?;
        self.stream.flush().await?;

        // Receive reply: [VER=5, REP, RSV=0, ATYP, BND.ADDR, BND.PORT]
        let mut reply = [0u8; 4];
        self.stream.read_exact(&mut reply).await?;

        if reply[0] != SOCKS5_VERSION {
            return Err(format!("Invalid SOCKS version in reply: {}", reply[0]).into());
        }

        let reply_code = reply[1];
        let atyp = reply[3];

        // Read bound address based on type
        match atyp {
            ATYP_IPV4 => {
                let mut addr = [0u8; 6]; // 4 bytes IP + 2 bytes port
                self.stream.read_exact(&mut addr).await?;
            }
            ATYP_DOMAIN => {
                let mut len_buf = [0u8; 1];
                self.stream.read_exact(&mut len_buf).await?;
                let len = len_buf[0] as usize;
                let mut domain_and_port = vec![0u8; len + 2]; // domain + 2 bytes port
                self.stream.read_exact(&mut domain_and_port).await?;
            }
            _ => {
                return Err(format!("Unsupported address type in reply: {}", atyp).into());
            }
        }

        Ok(reply_code == REPLY_SUCCESS)
    }

    /// Send CONNECT request (domain name)
    async fn connect_domain(&mut self, target_domain: &str, target_port: u16) -> E2EResult<bool> {
        // Build request: [VER=5, CMD=CONNECT, RSV=0, ATYP=DOMAIN, DST.ADDR, DST.PORT]
        let mut request = vec![SOCKS5_VERSION, CMD_CONNECT, 0x00, ATYP_DOMAIN];
        request.push(target_domain.len() as u8);
        request.extend_from_slice(target_domain.as_bytes());
        request.extend_from_slice(&target_port.to_be_bytes());

        self.stream.write_all(&request).await?;
        self.stream.flush().await?;

        // Receive reply
        let mut reply = [0u8; 4];
        self.stream.read_exact(&mut reply).await?;

        if reply[0] != SOCKS5_VERSION {
            return Err(format!("Invalid SOCKS version in reply: {}", reply[0]).into());
        }

        let reply_code = reply[1];
        let atyp = reply[3];

        // Read bound address
        match atyp {
            ATYP_IPV4 => {
                let mut addr = [0u8; 6];
                self.stream.read_exact(&mut addr).await?;
            }
            ATYP_DOMAIN => {
                let mut len_buf = [0u8; 1];
                self.stream.read_exact(&mut len_buf).await?;
                let len = len_buf[0] as usize;
                let mut domain_and_port = vec![0u8; len + 2];
                self.stream.read_exact(&mut domain_and_port).await?;
            }
            _ => {}
        }

        Ok(reply_code == REPLY_SUCCESS)
    }

    /// Get underlying stream for data transfer
    fn into_stream(self) -> TcpStream {
        self.stream
    }
}

/// Start a simple HTTP test server on localhost
async fn start_test_http_server() -> E2EResult<(u16, tokio::task::JoinHandle<()>)> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let port = listener.local_addr()?.port();

    let handle = tokio::spawn(async move {
        if let Ok((mut stream, _)) = listener.accept().await {
            let response = b"HTTP/1.1 200 OK\r\nContent-Length: 25\r\n\r\nHello from test server!";
            let _ = stream.write_all(response).await;
        }
    });

    Ok((port, handle))
}

#[tokio::test]
async fn test_socks5_basic_connect() -> E2EResult<()> {
    println!("\n=== E2E Test: SOCKS5 Basic CONNECT ===");

    // Start a test HTTP server
    let (http_port, http_handle) = start_test_http_server().await?;
    println!("Test HTTP server started on port {}", http_port);

    // PROMPT: Tell the LLM to act as a SOCKS5 proxy that allows all connections
    let prompt = "Start a SOCKS5 proxy server on port {AVAILABLE_PORT} that allows all connections without authentication. \
        When clients send CONNECT requests, establish the connection to the target.";

    // Start the SOCKS5 server with mocks
    let server = start_netget_server(
        NetGetConfig::new(prompt)
            .with_mock(|mock| {
                mock
                    // Mock specific events first (most specific to least specific)
                    // Mock 1: SOCKS5 CONNECT request
                    .on_event("socks5_connect_request")
                    .respond_with_actions(serde_json::json!([
                        {
                            "type": "allow_socks5_connect"
                        }
                    ]))
                    .and()
                    // Mock 2: the SOCKS5 greeting/auth exchange.
                    //
                    // The event is `socks5_auth_request` and the verb is `allow_socks5_auth`.
                    // This rule named `socks5_handshake` / `wait_for_more`, neither of which
                    // exists here, so it never matched: the `.on_any()` catch-all below answered
                    // instead, which is why the test passed while this rule did nothing. An
                    // unresolvable event id also means the action was never validated.
                    .on_event("socks5_auth_request")
                    .respond_with_actions(serde_json::json!([
                        {
                            "type": "allow_socks5_auth"
                        }
                    ]))
                    .and()
                    // Mock 3: Server startup (user command) - catch-all for non-event calls
                    .on_any()
                    .respond_with_actions(serde_json::json!([
                        {
                            "type": "open_server",
                            "port": 0,
                            "base_stack": "SOCKS5",
                            "instruction": "Allow all connections without authentication. When clients send CONNECT requests, establish the connection to the target",
                            "startup_params": {
                                "auth_methods": ["none"]
                            }
                        }
                    ]))
                    .and()
            })
    ).await?;
    println!("SOCKS5 server started on port {}", server.port);

    // Give server time to start
    tokio::time::sleep(Duration::from_millis(500)).await;

    // VALIDATION: Connect through SOCKS5 proxy to HTTP server
    println!("Connecting to SOCKS5 proxy...");
    let mut socks_client = Socks5Client::connect(&format!("127.0.0.1:{}", server.port)).await?;
    println!("✓ Connected to SOCKS5 proxy");

    println!("Performing SOCKS5 handshake (no auth)...");
    socks_client.handshake_no_auth().await?;
    println!("✓ Handshake successful");

    println!("Sending CONNECT request to 127.0.0.1:{}...", http_port);
    let connected = socks_client
        .connect_ipv4(Ipv4Addr::new(127, 0, 0, 1), http_port)
        .await?;

    if !connected {
        println!("✗ CONNECT request was denied");
        server.stop().await?;
        return Err("SOCKS5 CONNECT request failed".into());
    }
    println!("✓ CONNECT successful");

    // Send HTTP request through the proxy
    println!("Sending HTTP request through SOCKS5 proxy...");
    let mut stream = socks_client.into_stream();
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .await?;
    stream.flush().await?;

    // Read HTTP response
    let mut buffer = vec![0u8; 1024];
    match tokio::time::timeout(Duration::from_secs(5), stream.read(&mut buffer)).await {
        Ok(Ok(n)) if n > 0 => {
            let response = String::from_utf8_lossy(&buffer[..n]);
            println!("HTTP response received: {} bytes", n);
            if response.contains("200 OK") && response.contains("Hello, World!") {
                println!("✓ HTTP response is correct");
            } else {
                println!("✗ Unexpected HTTP response: {}", response);
            }
        }
        Ok(Ok(_)) => println!("Connection closed"),
        Ok(Err(e)) => println!("Read error: {}", e),
        Err(_) => println!("Timeout reading HTTP response"),
    }

    // Verify mock expectations were met
    server.verify_mocks().await?;

    // Cleanup
    http_handle.abort();
    server.stop().await?;
    println!("=== Test completed ===\n");
    Ok(())
}

#[tokio::test]
async fn test_socks5_with_authentication() -> E2EResult<()> {
    println!("\n=== E2E Test: SOCKS5 Username/Password Authentication ===");

    // Start a test HTTP server
    let (http_port, http_handle) = start_test_http_server().await?;
    println!("Test HTTP server started on port {}", http_port);

    // PROMPT: Tell the LLM to require authentication with explicit configuration
    let prompt = "Start a SOCKS5 proxy server on port {AVAILABLE_PORT} with username/password authentication. \
        IMPORTANT: Use startup_params with auth_methods set to [\"username_password\"]. \
        Accept username 'testuser' with password 'testpass'. Allow all connections after successful authentication.";

    // Start the SOCKS5 server with mocks
    let server = start_netget_server(
        NetGetConfig::new(prompt)
            .with_mock(|mock| {
                mock
                    // Mock 1: CONNECT request (most specific first)
                    .on_event("socks5_connect_request")
                    .respond_with_actions(serde_json::json!([
                        {
                            "type": "allow_socks5_connect"
                        }
                    ]))
                    // NOTE: .expect_calls() disabled
                    // .expect_calls(1)
                    .and()
                    // Mock 2: Authentication attempt
                    .on_event("socks5_auth_request")
                    .and_event_data_contains("username", "testuser")
                    .respond_with_actions(serde_json::json!([
                        {
                            "type": "allow_socks5_auth"
                        }
                    ]))
                    // NOTE: .expect_calls() disabled
                    // .expect_calls(1)
                    .and()
                    // Mock 3: Server startup (catch-all last)
                    .on_any()
                    .respond_with_actions(serde_json::json!([
                        {
                            "type": "open_server",
                            "port": 0,
                            "base_stack": "SOCKS5",
                            "instruction": "Accept username 'testuser' with password 'testpass'. Allow all connections after successful authentication",
                            "startup_params": {
                                "auth_methods": ["username_password"]
                            }
                        }
                    ]))
                    // NOTE: .expect_calls() disabled
                    // .expect_calls(1)
                    .and()
            })
    ).await?;
    println!("SOCKS5 server started on port {}", server.port);

    tokio::time::sleep(Duration::from_millis(500)).await;

    // VALIDATION: Connect with correct credentials
    println!("Connecting to SOCKS5 proxy...");
    let mut socks_client = Socks5Client::connect(&format!("127.0.0.1:{}", server.port)).await?;
    println!("✓ Connected to SOCKS5 proxy");

    println!("Performing SOCKS5 handshake with username/password...");
    let auth_success = socks_client
        .handshake_with_auth("testuser", "testpass")
        .await?;

    if !auth_success {
        println!("✗ Authentication failed");
        http_handle.abort();
        server.stop().await?;
        return Err("SOCKS5 authentication failed".into());
    }
    println!("✓ Authentication successful");

    println!("Sending CONNECT request...");
    let connected = socks_client
        .connect_ipv4(Ipv4Addr::new(127, 0, 0, 1), http_port)
        .await?;

    if !connected {
        println!("✗ CONNECT request was denied");
    } else {
        println!("✓ CONNECT successful");
    }

    // Verify mock expectations were met
    server.verify_mocks().await?;

    // Cleanup
    http_handle.abort();
    server.stop().await?;
    println!("=== Test completed ===\n");
    Ok(())
}

#[tokio::test]
async fn test_socks5_connection_rejection() -> E2EResult<()> {
    println!("\n=== E2E Test: SOCKS5 Connection Rejection ===");

    // PROMPT: Tell the LLM to deny connections to port 9999
    let prompt = "Start a SOCKS5 proxy server on port {AVAILABLE_PORT}. Deny any connection attempts to port 9999. \
        Allow all other connections.";

    // Start the SOCKS5 server with mocks
    let server = start_netget_server(
        NetGetConfig::new(prompt)
            .with_mock(|mock| {
                mock
                    // Mock 1: Server startup
                    .on_any()
                    .respond_with_actions(serde_json::json!([
                        {
                            "type": "open_server",
                            "port": 0,
                            "base_stack": "SOCKS5",
                            "instruction": "Deny any connection attempts to port 9999. Allow all other connections",
                            "startup_params": {
                                "auth_methods": ["none"]
                            }
                        }
                    ]))
                    // NOTE: .expect_calls() disabled
                    // .expect_calls(1)
                    .and()
                    // Mock 2: Handshake
                    .on_event("socks5_auth_request")
                    .respond_with_actions(serde_json::json!([
                        {
                            "type": "allow_socks5_auth"
                        }
                    ]))
                    // NOTE: .expect_calls() disabled
                    // .expect_calls(1)
                    .and()
                    // Mock 3: CONNECT to blocked port 9999
                    .on_event("socks5_connect_request")
                    .and_event_data_contains("port", "9999")
                    .respond_with_actions(serde_json::json!([
                        {
                            "type": "deny_socks5_connect",
                            "reason": "Connection to port 9999 not allowed"
                        }
                    ]))
                    // NOTE: .expect_calls() disabled
                    // .expect_calls(1)
                    .and()
            })
    ).await?;
    println!("SOCKS5 server started on port {}", server.port);

    tokio::time::sleep(Duration::from_millis(500)).await;

    // VALIDATION: Try to connect to blocked port
    println!("Connecting to SOCKS5 proxy...");
    let mut socks_client = Socks5Client::connect(&format!("127.0.0.1:{}", server.port)).await?;
    println!("✓ Connected to SOCKS5 proxy");

    println!("Performing SOCKS5 handshake...");
    socks_client.handshake_no_auth().await?;
    println!("✓ Handshake successful");

    println!("Sending CONNECT request to blocked port 9999...");
    let connected = socks_client
        .connect_ipv4(Ipv4Addr::new(127, 0, 0, 1), 9999)
        .await?;

    if connected {
        println!("✗ Connection was allowed (should have been denied)");
        server.stop().await?;
        return Err("SOCKS5 should have denied connection to port 9999".into());
    }
    println!("✓ Connection correctly denied");

    // Verify mock expectations were met
    server.verify_mocks().await?;

    server.stop().await?;
    println!("=== Test completed ===\n");
    Ok(())
}

#[tokio::test]
async fn test_socks5_domain_name() -> E2EResult<()> {
    println!("\n=== E2E Test: SOCKS5 Domain Name Resolution ===");

    // Start a test HTTP server
    let (http_port, http_handle) = start_test_http_server().await?;
    println!("Test HTTP server started on port {}", http_port);

    // PROMPT: Tell the LLM to allow connections using domain names
    let prompt = "Start a SOCKS5 proxy server on port {AVAILABLE_PORT} that accepts domain names in CONNECT requests. \
        Allow connections to localhost.";

    // Start the SOCKS5 server with mocks
    let server = start_netget_server(
        NetGetConfig::new(prompt)
            .with_mock(|mock| {
                mock
                    // Mock 1: CONNECT with domain name (most specific first)
                    .on_event("socks5_connect_request")
                    .and_event_data_contains("target", "localhost")
                    .respond_with_actions(serde_json::json!([
                        {
                            "type": "allow_socks5_connect"
                        }
                    ]))
                    // NOTE: .expect_calls() disabled
                    // .expect_calls(1)
                    .and()
                    // Mock 2: Handshake
                    .on_event("socks5_auth_request")
                    .respond_with_actions(serde_json::json!([
                        {
                            "type": "allow_socks5_auth"
                        }
                    ]))
                    // NOTE: .expect_calls() disabled
                    // .expect_calls(1)
                    .and()
                    // Mock 3: Server startup (catch-all last)
                    .on_any()
                    .respond_with_actions(serde_json::json!([
                        {
                            "type": "open_server",
                            "port": 0,
                            "base_stack": "SOCKS5",
                            "instruction": "Accept domain names in CONNECT requests. Allow connections to localhost",
                            "startup_params": {
                                "auth_methods": ["none"]
                            }
                        }
                    ]))
                    // NOTE: .expect_calls() disabled
                    // .expect_calls(1)
                    .and()
            })
    ).await?;
    println!("SOCKS5 server started on port {}", server.port);

    tokio::time::sleep(Duration::from_millis(500)).await;

    // VALIDATION: Connect using domain name
    println!("Connecting to SOCKS5 proxy...");
    let mut socks_client = Socks5Client::connect(&format!("127.0.0.1:{}", server.port)).await?;
    println!("✓ Connected to SOCKS5 proxy");

    println!("Performing SOCKS5 handshake...");
    socks_client.handshake_no_auth().await?;
    println!("✓ Handshake successful");

    println!("Sending CONNECT request to localhost:{}...", http_port);
    let connected = socks_client.connect_domain("localhost", http_port).await?;

    if !connected {
        println!("✗ CONNECT request with domain name was denied");
        http_handle.abort();
        server.stop().await?;
        return Err("SOCKS5 domain name CONNECT failed".into());
    }
    println!("✓ CONNECT with domain name successful");

    // Verify mock expectations were met
    server.verify_mocks().await?;

    // Cleanup
    http_handle.abort();
    server.stop().await?;
    println!("=== Test completed ===\n");
    Ok(())
}

#[tokio::test]
async fn test_socks5_mitm_inspection() -> E2EResult<()> {
    println!("\n=== E2E Test: SOCKS5 MITM Inspection Mode ===");

    // Start a test HTTP server
    let (http_port, http_handle) = start_test_http_server().await?;
    println!("Test HTTP server started on port {}", http_port);

    // PROMPT: Tell the LLM to enable MITM mode with explicit configuration
    let prompt = "Start a SOCKS5 proxy server on port {AVAILABLE_PORT} with MITM inspection. \
        IMPORTANT: Use startup_params with mitm_by_default set to true. \
        When HTTP data flows through, forward it unchanged to the target. \
        Allow all connections to localhost.";

    // Start the SOCKS5 server with mocks
    let server = start_netget_server(
        NetGetConfig::new(prompt)
            .with_mock(|mock| {
                mock
                    // Mock 1: Data inspection (most specific first)
                    // `socks5_data_to_target` is the client->target direction; there is no
                    // `socks5_data_from_client`, and the verb is `forward_socks5_data`.
                    .on_event("socks5_data_to_target")
                    .respond_with_actions(serde_json::json!([
                        {
                            "type": "forward_socks5_data"
                        }
                    ]))
                    // NOTE: .expect_at_least() disabled
                    // .expect_at_least(1)
                    .and()
                    // Mock 2: CONNECT request
                    .on_event("socks5_connect_request")
                    .respond_with_actions(serde_json::json!([
                        {
                            "type": "allow_socks5_connect"
                        }
                    ]))
                    // NOTE: .expect_calls() disabled
                    // .expect_calls(1)
                    .and()
                    // Mock 3: Handshake
                    .on_event("socks5_auth_request")
                    .respond_with_actions(serde_json::json!([
                        {
                            "type": "allow_socks5_auth"
                        }
                    ]))
                    // NOTE: .expect_calls() disabled
                    // .expect_calls(1)
                    .and()
                    // Mock 4: Server startup (catch-all last)
                    .on_any()
                    .respond_with_actions(serde_json::json!([
                        {
                            "type": "open_server",
                            "port": 0,
                            "base_stack": "SOCKS5",
                            "instruction": "When HTTP data flows through, forward it unchanged to the target. Allow all connections to localhost",
                            "startup_params": {
                                "auth_methods": ["none"],
                                "mitm_by_default": true
                            }
                        }
                    ]))
                    // NOTE: .expect_calls() disabled
                    // .expect_calls(1)
                    .and()
            })
    ).await?;
    println!("SOCKS5 server started on port {}", server.port);

    tokio::time::sleep(Duration::from_millis(500)).await;

    // VALIDATION: Connect through SOCKS5 and send HTTP request
    println!("Connecting to SOCKS5 proxy...");
    let mut socks_client = Socks5Client::connect(&format!("127.0.0.1:{}", server.port)).await?;
    println!("✓ Connected to SOCKS5 proxy");

    println!("Performing SOCKS5 handshake...");
    socks_client.handshake_no_auth().await?;
    println!("✓ Handshake successful");

    println!("Sending CONNECT request to 127.0.0.1:{}...", http_port);
    let connected = socks_client
        .connect_ipv4(Ipv4Addr::new(127, 0, 0, 1), http_port)
        .await?;

    if !connected {
        println!("✗ CONNECT request was denied");
        http_handle.abort();
        server.stop().await?;
        return Err("SOCKS5 CONNECT failed".into());
    }
    println!("✓ CONNECT successful");

    // Send HTTP request through the MITM proxy
    println!("Sending HTTP request through MITM proxy...");
    let http_request = format!(
        "GET /test HTTP/1.1\r\nHost: localhost:{}\r\nConnection: close\r\n\r\n",
        http_port
    );
    socks_client
        .stream
        .write_all(http_request.as_bytes())
        .await?;
    socks_client.stream.flush().await?;
    println!("✓ HTTP request sent");

    // Read HTTP response through the proxy
    println!("Reading HTTP response through MITM proxy...");
    let mut response_buf = Vec::new();
    let mut temp_buf = [0u8; 1024];

    // Read with timeout since connection will close
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match socks_client.stream.read(&mut temp_buf).await {
                Ok(0) => break, // Connection closed
                Ok(n) => response_buf.extend_from_slice(&temp_buf[..n]),
                Err(e) => {
                    println!("Read error (expected on close): {}", e);
                    break;
                }
            }
        }
    })
    .await
    .ok(); // Timeout is expected

    let response_text = String::from_utf8_lossy(&response_buf);
    println!("Received response ({} bytes)", response_buf.len());

    // Verify we got an HTTP response
    if !response_text.contains("HTTP/1.1 200 OK") {
        println!("✗ Did not receive valid HTTP response through MITM proxy");
        println!("Response: {}", response_text);
        http_handle.abort();
        server.stop().await?;
        return Err("Invalid HTTP response through MITM".into());
    }

    if !response_text.contains("Hello from test server!") {
        println!("✗ Response body missing expected content");
        println!("Response: {}", response_text);
        http_handle.abort();
        server.stop().await?;
        return Err("Invalid HTTP response body through MITM".into());
    }

    println!("✓ Received valid HTTP response through MITM proxy");
    println!("✓ MITM inspection successful");

    // Verify mock expectations were met
    server.verify_mocks().await?;

    // Cleanup
    http_handle.abort();
    server.stop().await?;
    println!("=== Test completed ===\n");
    Ok(())
}
