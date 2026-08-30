//! Validation tests for protocol StartupExamples
//!
//! These tests ensure that all protocols with startup examples have valid
//! and correctly structured examples for all three handler modes:
//! - LLM mode: direct LLM-controlled responses
//! - Script mode: code-based deterministic handlers
//! - Static mode: fixed, predetermined responses

use netget::protocol::server_registry::registry;

/// Validate that LLM mode example has required structure
fn validate_llm_mode(protocol_name: &str, example: &serde_json::Value) -> Result<(), String> {
    // Must have type: "open_server"
    let action_type = example
        .get("type")
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("{}: LLM mode missing 'type' field", protocol_name))?;

    if action_type != "open_server" {
        return Err(format!(
            "{}: LLM mode 'type' should be 'open_server', got '{}'",
            protocol_name, action_type
        ));
    }

    // Must have base_stack
    if example.get("base_stack").is_none() {
        return Err(format!(
            "{}: LLM mode missing 'base_stack' field",
            protocol_name
        ));
    }

    // Must have instruction OR event_handlers (but for LLM mode, typically instruction)
    // LLM mode should NOT have event_handlers with script/static - it uses LLM handler by default
    if let Some(handlers) = example.get("event_handlers") {
        if let Some(handlers_arr) = handlers.as_array() {
            for handler in handlers_arr {
                if let Some(handler_obj) = handler.get("handler") {
                    let handler_type = handler_obj.get("type").and_then(|v| v.as_str());
                    if handler_type == Some("script") || handler_type == Some("static") {
                        return Err(format!(
                            "{}: LLM mode should not have script/static handlers in event_handlers",
                            protocol_name
                        ));
                    }
                }
            }
        }
    }

    Ok(())
}

/// Validate that Script mode example has required structure
fn validate_script_mode(protocol_name: &str, example: &serde_json::Value) -> Result<(), String> {
    // Must have type: "open_server"
    let action_type = example
        .get("type")
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("{}: Script mode missing 'type' field", protocol_name))?;

    if action_type != "open_server" {
        return Err(format!(
            "{}: Script mode 'type' should be 'open_server', got '{}'",
            protocol_name, action_type
        ));
    }

    // Must have base_stack
    if example.get("base_stack").is_none() {
        return Err(format!(
            "{}: Script mode missing 'base_stack' field",
            protocol_name
        ));
    }

    // Must have event_handlers with at least one script handler
    let handlers = example
        .get("event_handlers")
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            format!(
                "{}: Script mode missing 'event_handlers' array",
                protocol_name
            )
        })?;

    if handlers.is_empty() {
        return Err(format!(
            "{}: Script mode 'event_handlers' array is empty",
            protocol_name
        ));
    }

    // Check that at least one handler has type: "script"
    let has_script_handler = handlers.iter().any(|h| {
        h.get("handler")
            .and_then(|handler| handler.get("type"))
            .and_then(|t| t.as_str())
            == Some("script")
    });

    if !has_script_handler {
        return Err(format!(
            "{}: Script mode must have at least one handler with type: 'script'",
            protocol_name
        ));
    }

    // Validate script handlers have required fields
    for handler in handlers {
        if let Some(handler_obj) = handler.get("handler") {
            if handler_obj.get("type").and_then(|t| t.as_str()) == Some("script") {
                // Script handler must have language and code
                if handler_obj.get("language").is_none() {
                    return Err(format!(
                        "{}: Script handler missing 'language' field",
                        protocol_name
                    ));
                }
                if handler_obj.get("code").is_none() {
                    return Err(format!(
                        "{}: Script handler missing 'code' field",
                        protocol_name
                    ));
                }
            }
        }
    }

    Ok(())
}

/// Validate that Static mode example has required structure
fn validate_static_mode(protocol_name: &str, example: &serde_json::Value) -> Result<(), String> {
    // Must have type: "open_server"
    let action_type = example
        .get("type")
        .and_then(|v| v.as_str())
        .ok_or_else(|| format!("{}: Static mode missing 'type' field", protocol_name))?;

    if action_type != "open_server" {
        return Err(format!(
            "{}: Static mode 'type' should be 'open_server', got '{}'",
            protocol_name, action_type
        ));
    }

    // Must have base_stack
    if example.get("base_stack").is_none() {
        return Err(format!(
            "{}: Static mode missing 'base_stack' field",
            protocol_name
        ));
    }

    // Must have event_handlers with at least one static handler
    let handlers = example
        .get("event_handlers")
        .and_then(|v| v.as_array())
        .ok_or_else(|| {
            format!(
                "{}: Static mode missing 'event_handlers' array",
                protocol_name
            )
        })?;

    if handlers.is_empty() {
        return Err(format!(
            "{}: Static mode 'event_handlers' array is empty",
            protocol_name
        ));
    }

    // Check that at least one handler has type: "static"
    let has_static_handler = handlers.iter().any(|h| {
        h.get("handler")
            .and_then(|handler| handler.get("type"))
            .and_then(|t| t.as_str())
            == Some("static")
    });

    if !has_static_handler {
        return Err(format!(
            "{}: Static mode must have at least one handler with type: 'static'",
            protocol_name
        ));
    }

    // Validate static handlers have required fields
    for handler in handlers {
        if let Some(handler_obj) = handler.get("handler") {
            if handler_obj.get("type").and_then(|t| t.as_str()) == Some("static") {
                // Static handler must have actions array
                let actions = handler_obj.get("actions").and_then(|a| a.as_array());
                if actions.is_none() {
                    return Err(format!(
                        "{}: Static handler missing 'actions' array",
                        protocol_name
                    ));
                }
                if actions.map(|a| a.is_empty()).unwrap_or(true) {
                    return Err(format!(
                        "{}: Static handler 'actions' array is empty",
                        protocol_name
                    ));
                }
                // Each action should have a type field
                for action in actions.unwrap() {
                    if action.get("type").is_none() {
                        return Err(format!(
                            "{}: Static handler action missing 'type' field",
                            protocol_name
                        ));
                    }
                }
            }
        }
    }

    Ok(())
}

#[test]
fn test_all_startup_examples_are_valid() {
    let reg = registry();
    let mut errors = Vec::new();
    let mut protocol_count = 0;

    for (protocol_name, protocol) in reg.all_protocols() {
        protocol_count += 1;
        let examples = protocol.get_startup_examples();

        // Validate LLM mode
        if let Err(e) = validate_llm_mode(&protocol_name, &examples.llm_mode) {
            errors.push(e);
        }

        // Validate Script mode
        if let Err(e) = validate_script_mode(&protocol_name, &examples.script_mode) {
            errors.push(e);
        }

        // Validate Static mode
        if let Err(e) = validate_static_mode(&protocol_name, &examples.static_mode) {
            errors.push(e);
        }
    }

    // Print summary
    println!("\n=== StartupExamples Validation Summary ===");
    println!("Protocols validated: {}", protocol_count);

    // Report errors
    if !errors.is_empty() {
        println!("\n=== Validation Errors ({} total) ===", errors.len());
        for error in &errors {
            println!("  ERROR: {}", error);
        }
        panic!(
            "Found {} validation errors in startup examples. See above for details.",
            errors.len()
        );
    }

    println!(
        "\n✓ All {} protocols have valid startup examples!",
        protocol_count
    );
}

#[test]
fn test_event_types_have_response_examples() {
    let reg = registry();
    let mut errors = Vec::new();
    let mut total_event_types = 0;

    for (protocol_name, protocol) in reg.all_protocols() {
        let event_types = protocol.get_event_types();

        for event_type in event_types {
            total_event_types += 1;

            // EventType should have a valid response_example
            // The response_example is required in the constructor, so it should always exist
            // But we validate it's not null/empty
            if event_type.response_example.is_null() {
                errors.push(format!(
                    "{}: Event type '{}' has null response_example",
                    protocol_name, event_type.id
                ));
            }

            // The response_example must name executable actions: either one action object
            // with a "type", or an **array** of them.
            //
            // The array case is deliberate rather than a loosening. The model answers with
            // `{"actions": [...]}`, so a sequence is a legitimate answer, and one event in the
            // tree genuinely needs to show one: IMAP's `imap_command` says in its own
            // description that untagged responses come first and then exactly one tagged
            // `send_imap_response` completes the command. Demanding a single object there
            // would force the example to under-describe the answer. Every element is still
            // checked for a `type`, so the property the rule exists for — the example names
            // actions the model can actually send — is unchanged.
            if !event_type.response_example.is_null() {
                let mut check_one =
                    |value: &serde_json::Value, where_: &str| match value.as_object() {
                        Some(obj) if obj.get("type").is_some() => {}
                        Some(_) => errors.push(format!(
                            "{}: Event type '{}' response_example{} missing 'type' field",
                            protocol_name, event_type.id, where_
                        )),
                        None => errors.push(format!(
                            "{}: Event type '{}' response_example{} is not an action object",
                            protocol_name, event_type.id, where_
                        )),
                    };
                match event_type.response_example.as_array() {
                    Some(actions) if actions.is_empty() => errors.push(format!(
                        "{}: Event type '{}' response_example is an empty array, which shows \
                         the model nothing",
                        protocol_name, event_type.id
                    )),
                    Some(actions) => {
                        for (i, action) in actions.iter().enumerate() {
                            check_one(action, &format!(" element {i}"));
                        }
                    }
                    None => check_one(&event_type.response_example, ""),
                }
            }
        }
    }

    println!("\n=== EventType Validation Summary ===");
    println!("Total event types checked: {}", total_event_types);

    if !errors.is_empty() {
        println!("\n=== Validation Errors ===");
        for error in &errors {
            println!("  ERROR: {}", error);
        }
        panic!(
            "Found {} validation errors in event types. See above for details.",
            errors.len()
        );
    }

    println!("✓ All event types have valid response_examples!");
}

#[test]
fn test_startup_examples_base_stack_matches_protocol() {
    let reg = registry();
    let mut errors = Vec::new();

    for (protocol_name, protocol) in reg.all_protocols() {
        let examples = protocol.get_startup_examples();
        // The base_stack in examples should match the protocol name (case insensitive)
        // or be a valid variant for the protocol
        for (mode_name, example) in [
            ("llm_mode", &examples.llm_mode),
            ("script_mode", &examples.script_mode),
            ("static_mode", &examples.static_mode),
        ] {
            if let Some(base_stack) = example.get("base_stack").and_then(|v| v.as_str()) {
                // The base_stack should be related to the protocol
                // For example, TCP protocol should have base_stack "tcp"
                // This is a loose check - we just ensure it's not completely unrelated
                let protocol_name_lower = protocol_name.to_lowercase();
                let base_stack_lower = base_stack.to_lowercase();

                // Common mappings: http -> http, tcp -> tcp, dns -> dns
                // Some protocols might have different base stacks (e.g., ftp uses tcp)
                // So we just check that something reasonable is specified
                if base_stack_lower.is_empty() {
                    errors.push(format!(
                        "{}: {} has empty base_stack",
                        protocol_name, mode_name
                    ));
                }

                // Log the mapping for visibility
                println!(
                    "  {}: {} base_stack='{}' (protocol='{}')",
                    protocol_name, mode_name, base_stack, protocol_name_lower
                );
            }
        }
    }

    if !errors.is_empty() {
        println!("\n=== Validation Errors ===");
        for error in &errors {
            println!("  ERROR: {}", error);
        }
        panic!(
            "Found {} validation errors. See above for details.",
            errors.len()
        );
    }

    println!("\n✓ All base_stack values are valid!");
}
