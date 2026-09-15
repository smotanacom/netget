//! LDAP client E2E tests
//!
//! Tests the LDAP client against a real OpenLDAP server running in Docker.

#![cfg(all(test, feature = "ldap"))]

use anyhow::Result;
use netget::llm::ollama_client::OllamaClient;
use netget::protocol::{ConnectContext, StartupParams, CLIENT_REGISTRY};
use netget::state::app_state::AppState;
use netget::state::{ClientId, ClientInstance, ClientStatus};
use std::sync::Arc;
use tokio::sync::mpsc;

/// Helper to create test app state with LDAP client
async fn create_test_app_state() -> (Arc<AppState>, ClientId) {
    let state = Arc::new(AppState::new());

    let client = ClientInstance::new(
        ClientId::new(0), // overwritten by add_client with the real allocated id
        "localhost:1389".to_string(),
        "LDAP".to_string(),
        "Connect to LDAP server and search for users".to_string(),
    );
    let client_id = state.add_client(client).await;

    (state, client_id)
}

#[tokio::test]
#[ignore = "Requires Docker OpenLDAP container"]
async fn test_ldap_client_connect() -> Result<()> {
    // This test requires an OpenLDAP container running on localhost:1389
    // Start with: docker run -d -p 1389:389 -e LDAP_ORGANISATION="Example Inc" \
    //   -e LDAP_DOMAIN="example.com" -e LDAP_ADMIN_PASSWORD="admin" \
    //   --name openldap osixia/openldap:1.5.0

    let (state, client_id) = create_test_app_state().await;
    let (status_tx, mut _status_rx) = mpsc::unbounded_channel();

    // Get protocol from registry
    let protocol = CLIENT_REGISTRY
        .get("LDAP")
        .expect("LDAP protocol not found");

    // Create LLM client (mocked for testing)
    let llm_client = OllamaClient::new("http://localhost:11434".to_string());

    // Create connect context
    let connect_ctx = ConnectContext {
        remote_addr: "localhost:1389".to_string(),
        llm_client,
        state: Arc::clone(&state),
        status_tx,
        client_id,
        startup_params: None,
    };

    // Connect to LDAP
    let _local_addr = protocol.connect(connect_ctx).await?;

    // Wait a moment for connection to establish
    tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

    // Verify client status is connected
    let client = state.get_client(client_id).await.expect("Client not found");
    assert!(matches!(client.status, ClientStatus::Connected));

    Ok(())
}

#[tokio::test]
#[ignore = "Requires Docker OpenLDAP container and Ollama"]
async fn test_ldap_client_bind_and_search() -> Result<()> {
    // This test requires:
    // 1. OpenLDAP container (see above)
    // 2. Ollama running with a model

    let (state, client_id) = create_test_app_state().await;
    let (status_tx, mut status_rx) = mpsc::unbounded_channel();

    // Update instruction to bind and search
    state
        .set_instruction_for_client(
            client_id,
            "Connect to LDAP, bind as cn=admin,dc=example,dc=com with password 'admin', \
         then search for all entries under dc=example,dc=com"
                .to_string(),
        )
        .await;

    // Get protocol from registry
    let protocol = CLIENT_REGISTRY
        .get("LDAP")
        .expect("LDAP protocol not found");

    // Create LLM client
    let llm_client = OllamaClient::new("http://localhost:11434".to_string());

    // Create connect context
    let connect_ctx = ConnectContext {
        remote_addr: "localhost:1389".to_string(),
        llm_client,
        state: Arc::clone(&state),
        status_tx,
        client_id,
        startup_params: None,
    };

    // Connect to LDAP
    let _local_addr = protocol.connect(connect_ctx).await?;

    // Wait for LLM to process events and execute actions
    tokio::time::sleep(tokio::time::Duration::from_secs(5)).await;

    // Check status messages
    let mut messages = Vec::new();
    while let Ok(msg) = status_rx.try_recv() {
        messages.push(msg);
    }

    // Verify we got some status messages (at least connected)
    assert!(!messages.is_empty(), "Expected status messages");

    // Print messages for debugging
    for msg in &messages {
        println!("Status: {}", msg);
    }

    Ok(())
}

#[tokio::test]
#[ignore = "Requires Docker OpenLDAP container and Ollama"]
async fn test_ldap_client_add_modify_delete() -> Result<()> {
    // This test requires:
    // 1. OpenLDAP container (see above)
    // 2. Ollama running with a model

    let (state, client_id) = create_test_app_state().await;
    let (status_tx, mut _status_rx) = mpsc::unbounded_channel();

    // Update instruction to perform CRUD operations
    state
        .set_instruction_for_client(
            client_id,
            "Connect to LDAP, bind as cn=admin,dc=example,dc=com with password 'admin', \
         add a new user cn=testuser,dc=example,dc=com with mail testuser@example.com, \
         modify the mail to newemail@example.com, then delete the user"
                .to_string(),
        )
        .await;

    // Get protocol from registry
    let protocol = CLIENT_REGISTRY
        .get("LDAP")
        .expect("LDAP protocol not found");

    // Create LLM client
    let llm_client = OllamaClient::new("http://localhost:11434".to_string());

    // Create connect context
    let connect_ctx = ConnectContext {
        remote_addr: "localhost:1389".to_string(),
        llm_client,
        state: Arc::clone(&state),
        status_tx,
        client_id,
        startup_params: None,
    };

    // Connect to LDAP
    let _local_addr = protocol.connect(connect_ctx).await?;

    // Wait for LLM to process events and execute actions
    tokio::time::sleep(tokio::time::Duration::from_secs(10)).await;

    // Verify client is still connected (no errors)
    let client = state.get_client(client_id).await.expect("Client not found");
    assert!(
        !matches!(client.status, ClientStatus::Error(_)),
        "Client should not be in error state"
    );

    Ok(())
}

#[test]
fn test_ldap_client_protocol_metadata() {
    use netget::client::ldap::LdapClientProtocol;
    use netget::llm::actions::{Client, Protocol};

    let protocol = LdapClientProtocol::new();

    // Verify protocol name
    assert_eq!(protocol.protocol_name(), "LDAP");

    // Verify stack name
    assert_eq!(protocol.stack_name(), "ETH>IP>TCP>LDAP");

    // Verify keywords
    let keywords = protocol.keywords();
    assert!(keywords.contains(&"ldap"));
    assert!(keywords.contains(&"ldap client"));

    // Verify description exists
    assert!(!protocol.description().is_empty());

    // Verify example prompt exists
    assert!(!protocol.example_prompt().is_empty());
}

#[test]
fn test_ldap_client_actions() {
    use netget::client::ldap::LdapClientProtocol;
    use netget::llm::actions::{Client, Protocol};
    use netget::state::app_state::AppState;

    let protocol = LdapClientProtocol::new();
    let state = AppState::new();

    // Get async actions
    let async_actions = protocol.get_async_actions(&state);

    // Verify essential actions exist
    let action_names: Vec<String> = async_actions.iter().map(|a| a.name.clone()).collect();
    assert!(action_names.contains(&"bind".to_string()));
    assert!(action_names.contains(&"search".to_string()));
    assert!(action_names.contains(&"add".to_string()));
    assert!(action_names.contains(&"modify".to_string()));
    assert!(action_names.contains(&"delete".to_string()));
    assert!(action_names.contains(&"disconnect".to_string()));

    // Verify all actions have examples
    for action in &async_actions {
        assert!(
            !action.example.is_null(),
            "Action {} missing example",
            action.name
        );
    }
}

#[test]
fn test_ldap_client_execute_bind_action() {
    use netget::client::ldap::LdapClientProtocol;
    use netget::llm::actions::Client;
    use serde_json::json;

    let protocol = LdapClientProtocol::new();

    let action = json!({
        "type": "bind",
        "dn": "cn=admin,dc=example,dc=com",
        "password": "secret"
    });

    let result = protocol.execute_action(action);
    assert!(result.is_ok());
}

#[test]
fn test_ldap_client_execute_search_action() {
    use netget::client::ldap::LdapClientProtocol;
    use netget::llm::actions::Client;
    use serde_json::json;

    let protocol = LdapClientProtocol::new();

    let action = json!({
        "type": "search",
        "base_dn": "dc=example,dc=com",
        "filter": "(objectClass=person)",
        "attributes": ["cn", "mail"],
        "scope": "subtree"
    });

    let result = protocol.execute_action(action);
    assert!(result.is_ok());
}
