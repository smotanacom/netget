//! An interpreter child never sees this process's credentials.
//!
//! Handler scripts run as the operator and are not sandboxed, but nothing a handler does
//! needs the model backend's API key, and a handler supplied over MCP, loaded from a
//! `.netget` file or written by the model in operator chat could print it into its stderr,
//! which is logged. `ProcessGroup::configure`, which every interpreter spawn goes through,
//! withholds the names in `STRIPPED_ENV` and anything credential-shaped; ordinary variables
//! still pass, so interpreters find their modules.
//!
//! Needs `python3`, which the scripting suite already requires.

use netget::scripting::executor::execute_script;
use netget::scripting::process_io::{env_is_stripped, STRIPPED_ENV};
use netget::scripting::types::{
    ScriptConfig, ScriptInput, ScriptLanguage, ScriptSource, ServerContext,
};

#[test]
fn credential_shaped_names_are_withheld_and_ordinary_ones_are_not() {
    for name in STRIPPED_ENV {
        assert!(env_is_stripped(name), "{name}");
    }
    for name in [
        "netget_api_key",
        "MY_SERVICE_SECRET",
        "DB_PASSWORD",
        "X_API_KEY",
        "VAULT_AUTH_TOKEN",
        "SSH_PRIVATE_KEY",
        "GOOGLE_APPLICATION_CREDENTIALS",
    ] {
        assert!(env_is_stripped(name), "{name}");
    }
    for name in [
        "PATH",
        "HOME",
        "LANG",
        "PYTHONPATH",
        "NODE_PATH",
        "TERM",
        "USER",
        "NETGET_CLIENT_LLM_CALL_LIMIT",
    ] {
        assert!(!env_is_stripped(name), "{name} must pass through");
    }
}

#[test]
fn a_python_handler_cannot_read_the_api_key_but_sees_an_ordinary_variable() {
    // Set in this process so the child would inherit them without the strip. Test binaries
    // run tests on threads, so these are process-wide; the names are unique to this file.
    std::env::set_var("NETGET_API_KEY", "sk-should-never-reach-a-script");
    std::env::set_var("NETGET_ENV_TEST_PLAIN", "visible");
    std::env::set_var("NETGET_ENV_TEST_DB_PASSWORD", "hidden");

    let code = r#"
import json, os, sys
json.load(sys.stdin)
print(json.dumps([{
    "type": "show_message",
    "message": json.dumps({
        "api_key": os.environ.get("NETGET_API_KEY"),
        "openai": os.environ.get("OPENAI_API_KEY"),
        "plain": os.environ.get("NETGET_ENV_TEST_PLAIN"),
        "shaped": os.environ.get("NETGET_ENV_TEST_DB_PASSWORD"),
        "path": os.environ.get("PATH"),
    })
}]))
"#;
    let config = ScriptConfig {
        language: ScriptLanguage::Python,
        source: ScriptSource::Inline(code.to_string()),
        handles_contexts: vec!["test".to_string()],
    };
    let input = ScriptInput {
        event_type_id: "test".to_string(),
        client: None,
        server: Some(ServerContext {
            id: 1,
            port: 8080,
            stack: "HTTP".to_string(),
            memory: String::new(),
            instruction: "Test".to_string(),
        }),
        connection: None,
        event: serde_json::json!({}),
    };
    let response = execute_script(&config, &input).expect("python3 handler runs");
    let message = response.actions[0]["message"]
        .as_str()
        .expect("message string");
    let seen: serde_json::Value = serde_json::from_str(message).unwrap();
    assert!(
        seen["api_key"].is_null(),
        "NETGET_API_KEY reached the script: {seen}"
    );
    assert!(
        seen["openai"].is_null(),
        "OPENAI_API_KEY reached the script: {seen}"
    );
    assert!(
        seen["shaped"].is_null(),
        "a *_PASSWORD variable reached the script: {seen}"
    );
    assert_eq!(
        seen["plain"], "visible",
        "ordinary variables must pass: {seen}"
    );
    assert!(
        seen["path"].as_str().is_some_and(|p| !p.is_empty()),
        "PATH must pass: {seen}"
    );
}
