use netget::{
    client::tacacs::actions::TacacsClientProtocol,
    llm::actions::{
        client_trait::Client,
        executor::{ActionFailure, ExecutionResult},
        protocol_trait::{ActionResult, Server},
    },
    server::tacacs::{actions::TacacsProtocol, chosen_reply, codec::*},
};
use serde_json::{json, Value};
fn hex(s: &str) -> Vec<u8> {
    assert_eq!(s.len() % 2, 0);
    (0..s.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&s[i..i + 2], 16).unwrap())
        .collect()
}
#[test]
fn real_python_requests_and_go_replies_are_exact_dual_golden_packets() {
    let fixture: Value = serde_json::from_str(include_str!("peer_wire.json")).unwrap();
    let mut count = 0;
    for (_, exchange) in fixture.as_object().unwrap() {
        for direction in ["request", "reply"] {
            for record in exchange[direction].as_array().unwrap() {
                let wire = hex(record["wire"].as_str().unwrap());
                let plain = hex(record["plain"].as_str().unwrap());
                let header = Header::parse(&wire[..12]).unwrap();
                header.validate().unwrap();
                assert_eq!(obfuscate(header, &wire[12..], b"test-secret"), plain);
                assert_eq!(packet(header, &plain, b"test-secret").unwrap(), wire);
                let typed = match (header.kind, direction) {
                    (1, "request") if header.sequence == 1 => {
                        let start = parse_start(&plain).unwrap();
                        let a = Authentication {
                            username: start.username,
                            password: start.password.unwrap_or_default(),
                            method: start.method,
                            privilege_level: start.privilege_level,
                            port: start.port,
                            remote_address: start.remote_address,
                        };
                        assert_eq!(a.username, "alice");
                        authentication_body(&a).unwrap().1
                    }
                    (1, "request") => {
                        let c = parse_continue(&plain).unwrap();
                        assert_eq!(c.user_message, "correct");
                        continue_body(&c.user_message, c.abort).unwrap()
                    }
                    (1, _) => auth_reply_body(&parse_auth_reply(&plain).unwrap()).unwrap(),
                    (2, "request") => {
                        let (r, kind) = parse_request(&plain, false).unwrap();
                        assert!(kind.is_none());
                        assert_eq!(r.arguments.len(), 3);
                        request_body(&r, None).unwrap()
                    }
                    (2, _) => author_reply_body(&parse_author_reply(&plain).unwrap()).unwrap(),
                    (3, "request") => {
                        let (r, kind) = parse_request(&plain, true).unwrap();
                        assert_eq!(kind, Some(AccountKind::Start));
                        request_body(&r, kind).unwrap()
                    }
                    (3, _) => account_reply_body(&parse_account_reply(&plain).unwrap()).unwrap(),
                    _ => unreachable!(),
                };
                assert_eq!(typed, plain);
                count += 1;
            }
        }
    }
    assert_eq!(count, 10);
}
#[test]
fn argument_first_separator_order_and_body_bounds_are_literal() {
    assert_eq!(
        Argument::parse("cmd-arg=one=two*three").unwrap().value,
        "one=two*three"
    );
    assert!(!Argument::parse("audit*enabled").unwrap().mandatory);
    for value in ["=empty", "*empty", "missing", "line=bad\n"] {
        assert!(Argument::parse(value).is_err());
    }
    let mut request:Request=serde_json::from_value(json!({"username":"alice","arguments":[{"name":"cmd-arg","value":"one"},{"name":"cmd-arg","value":"two"}]})).unwrap();
    let plain = request_body(&request, None).unwrap();
    let (decoded, _) = parse_request(&plain, false).unwrap();
    assert_eq!(decoded.arguments[0].value, "one");
    assert_eq!(decoded.arguments[1].value, "two");
    request.arguments = vec![
        Argument {
            name: "a".into(),
            value: "b".into(),
            mandatory: true
        };
        32
    ];
    assert!(request_body(&request, None).is_ok());
    request.arguments.push(request.arguments[0].clone());
    assert!(request_body(&request, None).is_err());
    let mut header = crate::helpers::tacacs::header(1, 0xc0);
    header.flags = 0x80;
    assert!(header.validate().is_ok());
    header.flags |= 1;
    assert!(header.validate().is_err());
    header.flags = 0;
    header.sequence = 255;
    assert!(header.reply().is_err());
    header.length = MAX_BODY_BYTES + 1;
    assert!(header.bytes().is_err());
    let mut bad = plain.clone();
    bad.push(0);
    assert!(parse_request(&bad, false).is_err());
    assert!(parse_request(&plain[..plain.len() - 1], false).is_err());
}
#[test]
fn all_accounting_kinds_unknown_flag_bits_and_abort_data_are_handled() {
    let request: Request = serde_json::from_value(json!({"username":"alice"})).unwrap();
    for kind in [
        AccountKind::Start,
        AccountKind::Stop,
        AccountKind::Watchdog,
        AccountKind::Update,
    ] {
        let mut bytes = request_body(&request, Some(kind)).unwrap();
        bytes[0] |= 0x81;
        assert_eq!(parse_request(&bytes, true).unwrap().1, Some(kind));
    }
    for flags in [0, 6, 12, 14] {
        let mut bytes = request_body(&request, Some(AccountKind::Start)).unwrap();
        bytes[0] = flags;
        assert!(parse_request(&bytes, true).is_err());
    }
    let body = continue_body("reason", true).unwrap();
    assert_eq!(&body[..5], &[0, 0, 0, 6, 1]);
    assert!(parse_continue(&body).unwrap().abort);
}
fn nested() -> Value {
    let mut value = Value::Null;
    for _ in 0..10000 {
        value = Value::Array(vec![value]);
    }
    value
}
#[test]
fn constructed_json_and_nested_results_reject_without_recursive_drop() {
    assert!(TacacsProtocol.execute_action(nested()).is_err());
    assert!(TacacsClientProtocol.execute_action(nested()).is_err());
    let mut result = ExecutionResult::new();
    result.raw_actions.push(nested());
    let mut deep = ActionResult::Custom {
        name: "respond_tacacs_authentication".into(),
        data: nested(),
    };
    for _ in 0..10000 {
        deep = ActionResult::Multiple(vec![deep]);
    }
    result.protocol_results.push(deep);
    assert!(chosen_reply(result, "respond_tacacs_authentication").is_err());
    let mut result = ExecutionResult::new();
    result.add_protocol_result(ActionResult::Custom {
        name: "respond_tacacs_authentication".into(),
        data: json!({"status":"pass"}),
    });
    result.failures.push(ActionFailure {
        index: 0,
        action: "bad".into(),
        error: "failed".into(),
    });
    assert!(chosen_reply(result, "respond_tacacs_authentication").is_err());
}
#[test]
fn action_shapes_terminal_statuses_and_shared_privacy_classifier_match() {
    use netget::llm::actions::protocol_trait::Protocol;
    for value in [
        json!({"type":"respond_tacacs_authentication","reply":{"status":"get_pass"}}),
        json!({"type":"record_tacacs_accounting","reply":{"status":"follow"}}),
        json!({"type":"respond_tacacs_authorization","reply":{"status":"follow"}}),
        json!({"type":"respond_tacacs_authentication","reply":{"status":"pass"},"unchecked":true}),
    ] {
        assert!(TacacsProtocol.execute_action(value).is_err());
    }
    assert!(netget::utils::redact::actions_have_credentials(
        &TacacsClientProtocol.get_async_actions(&crate::helpers::tacacs::state())
    ));
    for alias in ["tacacs", "tacacs+", "ETH>IP>TCP>TACACS"] {
        assert!(netget::protocol::server_registry::registry()
            .resolve(alias)
            .is_ok());
        assert!(netget::protocol::client_registry::CLIENT_REGISTRY
            .resolve(alias)
            .is_ok());
    }
}
