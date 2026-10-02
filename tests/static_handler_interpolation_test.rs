//! Tests for `{{event.…}}` interpolation in static event handlers.
//!
//! Static handlers emit configured actions with no LLM call. Without substitution they
//! cannot echo a client-chosen correlation id (DNS `query_id`, DHCP `xid`, SNMP
//! `request-id`, STUN transaction id), which made static mode unusable for the whole
//! request/response UDP family. These tests pin the three rules that make it usable:
//! type-preserving whole-string references, text-splicing embedded references, and
//! byte-identical pass-through for everything else.
//!
//! ```bash
//! ./cargo-isolated.sh test --no-default-features --features tcp \
//!   --test static_handler_interpolation_test -- --test-threads=100
//! ```

use netget::scripting::event_handler::{
    contains_event_reference, interpolate_actions, interpolate_value, validate_event_references,
};
use netget::scripting::{EventHandler, EventHandlerConfig, EventHandlerType, EventPattern};
use serde_json::{json, Value};

/// Interpolate a single value against an event, panicking on error.
fn interp(value: Value, event: Value) -> Value {
    interpolate_value(&value, Some(&event)).expect("interpolation should succeed")
}

/// Interpolate expecting failure, returning the rendered error message.
fn interp_err(value: Value, event: Value) -> String {
    interpolate_value(&value, Some(&event))
        .expect_err("interpolation should have failed")
        .to_string()
}

// ---------------------------------------------------------------------------
// Rule 1: whole-string reference preserves the referenced JSON type
// ---------------------------------------------------------------------------

#[test]
fn whole_string_reference_preserves_number_type() {
    let out = interp(
        json!({"query_id": "{{event.query_id}}"}),
        json!({"query_id": 4660}),
    );

    assert_eq!(out["query_id"], json!(4660));
    assert!(
        out["query_id"].is_number(),
        "query_id must stay a number, got {:?} — the action executor rejects strings here",
        out["query_id"]
    );
}

#[test]
fn whole_string_reference_preserves_every_json_type() {
    let event = json!({
        "num": 42,
        "float": 1.5,
        "flag": true,
        "nothing": null,
        "text": "hello",
        "list": [1, 2, 3],
        "obj": {"a": 1}
    });

    let out = interp(
        json!({
            "num": "{{event.num}}",
            "float": "{{event.float}}",
            "flag": "{{event.flag}}",
            "nothing": "{{event.nothing}}",
            "text": "{{event.text}}",
            "list": "{{event.list}}",
            "obj": "{{event.obj}}",
            "whole": "{{event}}"
        }),
        event.clone(),
    );

    assert!(out["num"].is_number() && out["num"] == json!(42));
    assert!(out["float"].is_f64() && out["float"] == json!(1.5));
    assert!(out["flag"].is_boolean() && out["flag"] == json!(true));
    assert!(out["nothing"].is_null());
    assert_eq!(out["text"], json!("hello"));
    assert!(out["list"].is_array() && out["list"] == json!([1, 2, 3]));
    assert!(out["obj"].is_object() && out["obj"] == json!({"a": 1}));
    assert_eq!(
        out["whole"], event,
        "`{{{{event}}}}` yields the whole payload"
    );
}

#[test]
fn whole_string_reference_tolerates_inner_whitespace() {
    let out = interp(json!("{{ event.xid }}"), json!({"xid": 305419896u64}));
    assert_eq!(out, json!(305419896u64));
    assert!(out.is_number());
}

#[test]
fn reference_surrounded_by_whitespace_is_embedded_not_whole() {
    // The string is not *entirely* a reference, so it splices as text.
    let out = interp(json!(" {{event.n}} "), json!({"n": 7}));
    assert_eq!(out, json!(" 7 "));
}

// ---------------------------------------------------------------------------
// Rule 2: embedded reference interpolates text
// ---------------------------------------------------------------------------

#[test]
fn embedded_reference_produces_a_string() {
    let out = interp(
        json!("reply to {{event.domain}}"),
        json!({"domain": "example.com"}),
    );
    assert_eq!(out, json!("reply to example.com"));
    assert!(out.is_string());
}

#[test]
fn embedded_non_string_values_render_in_json_form() {
    let event = json!({"n": 42, "flag": false, "nothing": null, "obj": {"a": 1}});
    assert_eq!(
        interp(json!("n=@{{event.n}}@"), event.clone()),
        json!("n=@42@")
    );
    assert_eq!(
        interp(json!("flag=@{{event.flag}}@"), event.clone()),
        json!("flag=@false@")
    );
    assert_eq!(
        interp(json!("nil=@{{event.nothing}}@"), event.clone()),
        json!("nil=@null@")
    );
    assert_eq!(
        interp(json!("obj=@{{event.obj}}@"), event),
        json!("obj=@{\"a\":1}@")
    );
}

#[test]
fn multiple_embedded_references_all_substitute() {
    let out = interp(
        json!("{{event.method}} {{event.path}} from {{event.peer}}"),
        json!({"method": "GET", "path": "/x", "peer": "127.0.0.1"}),
    );
    assert_eq!(out, json!("GET /x from 127.0.0.1"));
}

#[test]
fn adjacent_references_concatenate_as_text() {
    // Starts with `{{` and ends with `}}` but is two references, so it is not whole-string.
    let out = interp(json!("{{event.a}}{{event.b}}"), json!({"a": 1, "b": 2}));
    assert_eq!(out, json!("12"));
}

// ---------------------------------------------------------------------------
// Nested and indexed paths
// ---------------------------------------------------------------------------

#[test]
fn nested_path_resolves_through_objects() {
    let event = json!({"headers": {"host": "netget.local", "port": 8080}});
    assert_eq!(
        interp(json!("{{event.headers.host}}"), event.clone()),
        json!("netget.local")
    );
    let port = interp(json!("{{event.headers.port}}"), event);
    assert_eq!(port, json!(8080));
    assert!(port.is_number(), "nested numbers keep their type too");
}

#[test]
fn deeply_nested_path_resolves() {
    let event = json!({"a": {"b": {"c": {"d": "deep"}}}});
    assert_eq!(interp(json!("{{event.a.b.c.d}}"), event), json!("deep"));
}

#[test]
fn numeric_segment_indexes_arrays() {
    let event = json!({"questions": [{"name": "one.test"}, {"name": "two.test"}]});
    assert_eq!(
        interp(json!("{{event.questions.1.name}}"), event),
        json!("two.test")
    );
}

#[test]
fn references_resolve_inside_nested_action_structures() {
    let out = interp(
        json!({
            "type": "send_http_response",
            "headers": {"X-Echo": "{{event.headers.host}}"},
            "answers": [{"ttl": "{{event.ttl}}"}, {"ttl": "{{event.ttl}}"}]
        }),
        json!({"headers": {"host": "h"}, "ttl": 300}),
    );
    assert_eq!(out["headers"]["X-Echo"], json!("h"));
    assert!(out["answers"][0]["ttl"].is_number());
    assert_eq!(out["answers"][1]["ttl"], json!(300));
}

#[test]
fn object_keys_are_interpolated_as_text() {
    let out = interp(
        json!({"headers": {"X-{{event.name}}": "v"}}),
        json!({"name": "Trace"}),
    );
    assert_eq!(out["headers"]["X-Trace"], json!("v"));
}

// ---------------------------------------------------------------------------
// Missing / malformed references fail loudly
// ---------------------------------------------------------------------------

#[test]
fn missing_field_errors_and_names_the_field_and_alternatives() {
    let msg = interp_err(
        json!({"query_id": "{{event.querid}}"}),
        json!({"query_id": 1, "domain": "example.com"}),
    );

    assert!(
        msg.contains("{{event.querid}}"),
        "must quote the reference: {msg}"
    );
    assert!(msg.contains("querid"), "must name the missing field: {msg}");
    assert!(
        msg.contains("query_id") && msg.contains("domain"),
        "must list what the event does offer: {msg}"
    );
}

#[test]
fn missing_nested_field_reports_the_resolved_prefix() {
    let msg = interp_err(
        json!("{{event.headers.hsot}}"),
        json!({"headers": {"host": "h", "accept": "*/*"}}),
    );
    assert!(
        msg.contains("event.headers"),
        "must report where it got to: {msg}"
    );
    assert!(msg.contains("hsot"), "must name the missing segment: {msg}");
    assert!(
        msg.contains("host") && msg.contains("accept"),
        "must list siblings: {msg}"
    );
}

#[test]
fn indexing_a_scalar_errors_clearly() {
    let msg = interp_err(json!("{{event.query_id.sub}}"), json!({"query_id": 5}));
    assert!(
        msg.contains("number"),
        "must say what the value actually is: {msg}"
    );
    assert!(msg.contains("sub"), "must name the bad segment: {msg}");
}

#[test]
fn out_of_range_array_index_errors() {
    let msg = interp_err(json!("{{event.list.5}}"), json!({"list": [1, 2]}));
    assert!(
        msg.contains("indices: 0..1"),
        "must state the valid range: {msg}"
    );
}

#[test]
fn reference_without_event_data_errors_instead_of_nulling() {
    let err = interpolate_actions(&[json!({"query_id": "{{event.query_id}}"})], None)
        .expect_err("a reference with no event payload must fail");
    assert!(err.to_string().contains("no structured data"), "{err}");
}

#[test]
fn malformed_paths_are_rejected() {
    for bad in [
        "{{event.}}",
        "{{event..x}}",
        "{{event.a..b}}",
        "{{event.a.}}",
    ] {
        let err = interpolate_value(&json!(bad), Some(&json!({"a": 1})))
            .expect_err(&format!("`{bad}` must be rejected"));
        assert!(err.to_string().contains("segment") || err.to_string().contains("empty"));
    }
}

#[test]
fn validate_catches_malformed_references_without_an_event() {
    let handler = EventHandlerType::static_response(vec![json!({"id": "{{event.}}"})]);
    assert!(
        handler.validate().is_err(),
        "parse-time validation must reject `{{{{event.}}}}`"
    );

    let ok = EventHandlerType::static_response(vec![json!({"id": "{{event.query_id}}"})]);
    assert!(
        ok.validate().is_ok(),
        "field existence is an event-time question, not a parse-time one"
    );

    // Non-static handlers have nothing to validate.
    assert!(EventHandlerType::script("python", "{{ not a template }}")
        .validate()
        .is_ok());
    assert!(EventHandlerType::llm("answer politely").validate().is_ok());
}

// ---------------------------------------------------------------------------
// Rule 3: pass-through — existing handlers must be unaffected
// ---------------------------------------------------------------------------

#[test]
fn actions_without_references_pass_through_byte_identical() {
    let actions = vec![
        json!({"type": "send_http_response", "status": 200, "body": "hello", "keep_alive": true}),
        json!({"type": "close_connection"}),
    ];
    let out = interpolate_actions(&actions, Some(&json!({"anything": 1}))).unwrap();
    assert_eq!(out, actions);
    assert_eq!(
        serde_json::to_string(&out).unwrap(),
        serde_json::to_string(&actions).unwrap()
    );
}

#[test]
fn actions_without_references_never_need_event_data() {
    let actions = vec![json!({"type": "close_connection"})];
    assert_eq!(interpolate_actions(&actions, None).unwrap(), actions);
}

#[test]
fn literal_braces_are_untouched() {
    // JSON body, regex quantifier, Rust format string, shell/JS interpolation, a
    // Handlebars/Vue template being *served* by the handler, and Handlebars block syntax.
    let actions = vec![json!({
        "type": "send_http_response",
        "body": "{\"ok\":true,\"nested\":{\"a\":[1,2]}}",
        "regex": "^a{2,3}$ and x{{2}}",
        "rust_fmt": "value = {} and literal {{braces}}",
        "shell": "${HOME}/x ${{VAR}}",
        "vue": "<p>{{ message }}</p>",
        "handlebars": "{{#if user}}hi {{user.name}}{{/if}}{{! comment }}{{> partial}}",
        "triple": "{{{raw}}}"
    })];

    let out = interpolate_actions(&actions, Some(&json!({"user": {"name": "x"}}))).unwrap();
    assert_eq!(
        out, actions,
        "only `event`-rooted references may be substituted; everything else is literal"
    );
}

#[test]
fn unclosed_reference_is_left_alone() {
    let actions = vec![json!({"body": "{{event.query_id"})];
    assert_eq!(
        interpolate_actions(&actions, Some(&json!({}))).unwrap(),
        actions
    );
    assert!(validate_event_references(&actions[0]).is_ok());
}

#[test]
fn contains_event_reference_detects_only_event_rooted_refs() {
    assert!(contains_event_reference(&json!({"a": "{{event.x}}"})));
    assert!(contains_event_reference(
        &json!(["x", {"b": ["{{ event }}"]}])
    ));
    assert!(contains_event_reference(&json!({"{{event.k}}": "v"})));
    assert!(!contains_event_reference(&json!({"a": "{{ message }}"})));
    assert!(!contains_event_reference(&json!({"a": "{{eventual.x}}"})));
    assert!(!contains_event_reference(
        &json!({"a": 1, "b": [true, null]})
    ));
}

#[test]
fn triple_brace_resolves_the_inner_reference_and_keeps_the_outer_braces() {
    // `{{{event.n}}}` is `{` + `{{event.n}}` + `}`; no Handlebars triple-stash semantics.
    assert_eq!(
        interp(json!("{{{event.n}}}"), json!({"n": 5})),
        json!("{5}")
    );
}

#[test]
fn non_ascii_text_around_a_reference_survives() {
    let out = interp(json!("héllo → {{event.who}} ✓"), json!({"who": "wörld"}));
    assert_eq!(out, json!("héllo → wörld ✓"));
}

// ---------------------------------------------------------------------------
// End-to-end: a DNS-shaped static handler echoing a client's query_id
// ---------------------------------------------------------------------------

/// Build the static handler a caller would register for DNS via MCP `start_server`.
fn dns_static_handler() -> EventHandlerConfig {
    let mut config = EventHandlerConfig::new();
    config.add_handler(EventHandler::new(
        EventPattern::specific("dns_query"),
        EventHandlerType::static_response(vec![json!({
            "type": "send_dns_a_response",
            "query_id": "{{event.query_id}}",
            "domain": "{{event.domain}}",
            "ip": "93.184.216.34",
            "ttl": 300
        })]),
    ));
    config
}

#[test]
fn dns_static_handler_echoes_the_clients_random_query_id() {
    let config = dns_static_handler();
    let handler = config
        .find_handler("dns_query")
        .expect("handler must match");
    let EventHandlerType::Static { actions } = handler else {
        panic!("expected a static handler");
    };
    assert!(
        handler.validate().is_ok(),
        "handler must pass parse-time validation"
    );

    // Two different clients, two different randomly-chosen transaction ids.
    for query_id in [0x1234u64, 0xBEEF, 0, 65535] {
        let event = json!({
            "query_id": query_id,
            "domain": "example.com",
            "query_type": "A",
            "source": "127.0.0.1:54321"
        });

        let rendered = interpolate_actions(actions, Some(&event)).expect("must render");
        assert_eq!(rendered.len(), 1);
        let action = &rendered[0];

        assert_eq!(action["type"], json!("send_dns_a_response"));
        assert_eq!(
            action["query_id"],
            json!(query_id),
            "the response must carry the client's own transaction id"
        );
        assert!(
            action["query_id"].is_number(),
            "query_id must be a number — a string would be rejected by the DNS action \
             executor and the client would time out"
        );
        assert_eq!(action["domain"], json!("example.com"));
        assert_eq!(action["ip"], json!("93.184.216.34"));
        assert_eq!(
            action["ttl"],
            json!(300),
            "untouched fields keep their value and type"
        );
    }
}

#[tokio::test]
async fn dns_static_handler_interpolates_through_the_event_handler_executor() {
    use netget::llm::event_handler_executor::{try_execute_event_handler, EventHandlerResult};
    use netget::state::app_state::AppState;
    use netget::state::server::ServerInstance;

    let state = AppState::new();
    let server_id = state
        .add_server(ServerInstance::new(
            netget::state::ServerId::new(0),
            5353,
            "DNS".to_string(),
            "static handler test".to_string(),
        ))
        .await;
    state
        .set_event_handler_config(server_id, Some(dns_static_handler()))
        .await;

    let event = json!({"query_id": 0x1234, "domain": "example.com", "query_type": "A"});

    let result = try_execute_event_handler(
        &state,
        server_id,
        None,
        "dns_query",
        "DNS query for example.com",
        Some(event),
        None, // no protocol: we assert on the rendered actions, not on the wire bytes
    )
    .await
    .expect("static handler must execute");

    let EventHandlerResult::Handled(execution) = result else {
        panic!("a matching static handler must not fall back to the LLM");
    };

    let action = &execution.raw_actions[0];
    assert_eq!(action["query_id"], json!(0x1234));
    assert!(
        action["query_id"].is_number(),
        "the executor must hand a numeric query_id to the action layer"
    );
    assert_eq!(action["domain"], json!("example.com"));
}

#[tokio::test]
async fn executor_fails_loudly_on_a_typod_field() {
    use netget::llm::event_handler_executor::try_execute_event_handler;
    use netget::state::app_state::AppState;
    use netget::state::server::ServerInstance;

    let state = AppState::new();
    let server_id = state
        .add_server(ServerInstance::new(
            netget::state::ServerId::new(0),
            5354,
            "DNS".to_string(),
            "typo test".to_string(),
        ))
        .await;

    let mut config = EventHandlerConfig::new();
    config.add_handler(EventHandler::new(
        EventPattern::specific("dns_query"),
        EventHandlerType::static_response(vec![json!({
            "type": "send_dns_a_response",
            "query_id": "{{event.queryid}}"
        })]),
    ));
    state
        .set_event_handler_config(server_id, Some(config))
        .await;

    let outcome = try_execute_event_handler(
        &state,
        server_id,
        None,
        "dns_query",
        "DNS query for example.com",
        Some(json!({"query_id": 1, "domain": "example.com"})),
        None,
    )
    .await;

    let Err(err) = outcome else {
        panic!("a typo'd reference must surface as an error, not a silent null");
    };

    let chain = format!("{err:#}");
    assert!(chain.contains("queryid"), "{chain}");
    assert!(
        chain.contains("query_id"),
        "must list the real field: {chain}"
    );
}

#[test]
fn expansion_has_a_shared_budget_across_repeated_references_and_actions() {
    let event = json!({"body": "x".repeat(1024 * 1024)});
    let action = json!({"type":"send_data", "value":"{{event.body}}"});
    let error = interpolate_actions(&vec![action; 10], Some(&event)).unwrap_err();
    assert!(error.to_string().contains("budget"));
    let embedded = json!(format!("prefix{}", "{{event.body}}".repeat(10)));
    assert!(interpolate_value(&embedded, Some(&event)).is_err());
}

#[test]
fn interpolation_rejects_deep_programmatic_json_without_recursive_inspection() {
    let mut value = json!("{{event}}");
    for _ in 0..500 {
        value = Value::Array(vec![value]);
    }
    assert!(validate_event_references(&value).is_err());
    assert!(interpolate_value(&value, Some(&json!(1))).is_err());
    netget::utils::json_budget::drop_iteratively(value);
}

#[test]
fn interpolation_refuses_colliding_resolved_keys() {
    assert!(interpolate_value(
        &json!({"{{event.a}}": 1, "{{event.b}}": 2}),
        Some(&json!({"a":"same","b":"same"}))
    )
    .is_err());
}

#[test]
fn script_entry_detection_ignores_language_comments_and_strings() {
    for (language, code) in [
        ("javascript", "// function handle() {}\nconsole.log('[]')"),
        (
            "javascript",
            "const message = `function handle() {}`; console.log('[]')",
        ),
        (
            "javascript",
            "const pattern = /function handle()/; console.log('[]')",
        ),
        ("javascript", "const handle = (input); console.log('[]')"),
        (
            "python",
            "text = '''def handle(event): pass'''\nprint('[]')",
        ),
        ("perl", "# sub handle { }\nprint '[]';"),
        ("perl", "my $text = q{sub handle { }}; print '[]';"),
    ] {
        let handler: EventHandlerType =
            serde_json::from_value(json!({"type":"script", "language":language, "code":code}))
                .unwrap();
        assert!(
            handler.validate().is_ok(),
            "must ignore literal/comment for {language}: {code}"
        );
    }
    for (language, code) in [
        ("python", "def\t handle \n (event): pass"),
        ("javascript", "async function\nhandle (event) {}"),
        ("javascript", "const handle = async (event) => [];"),
        ("perl", "sub\nhandle { return []; }"),
    ] {
        let handler: EventHandlerType =
            serde_json::from_value(json!({"type":"script", "language":language, "code":code}))
                .unwrap();
        assert!(
            handler.validate().is_err(),
            "must detect resident declaration for {language}: {code}"
        );
    }
}
