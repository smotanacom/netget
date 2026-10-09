//! RFC 6241 envelopes: what Rust decides without asking a handler, and what a handler's
//! XML text can and cannot do once it is placed in a reply.
use netget::server::netconf::{actions::reply_body, rpc, xml};
use serde_json::json;

fn caps(extra: &[&str]) -> Vec<String> {
    let mut v = vec![rpc::BASE_10.to_owned(), rpc::BASE_11.to_owned()];
    v.extend(extra.iter().map(|s| s.to_string()));
    v
}

fn refusal(message: &str, capabilities: &[String]) -> (String, String) {
    match rpc::parse_rpc(message.as_bytes(), capabilities) {
        Err(rpc::RpcRefusal::Reply {
            attributes,
            bindings,
            error,
        }) => {
            let rendered = rpc::reply(
                &attributes,
                &bindings,
                &rpc::ReplyBody::Errors(vec![error.clone()]),
            )
            .unwrap();
            (error.tag, String::from_utf8(rendered).unwrap())
        }
        Err(rpc::RpcRefusal::Fatal(e)) => panic!("fatal: {e}"),
        Ok(r) => panic!("accepted {}", r.operation),
    }
}

const NC: &str = "urn:ietf:params:xml:ns:netconf:base:1.0";

#[test]
fn a_reply_echoes_every_rpc_attribute_and_its_prefix_binding() {
    let message = format!(
        r#"<rpc xmlns="{NC}" xmlns:ex="urn:example" message-id="m&amp;1" ex:trace="t-9"><get/></rpc>"#
    );
    let incoming = rpc::parse_rpc(message.as_bytes(), &caps(&[])).ok().unwrap();
    assert_eq!(incoming.message_id, "m&1");
    let out = rpc::reply(
        &incoming.attributes,
        &incoming.bindings,
        &rpc::ReplyBody::Ok,
    )
    .unwrap();
    let doc = xml::parse(&out).unwrap();
    let (name, ns, attrs) = xml::element(&doc, 0).unwrap();
    assert_eq!((name, ns), ("rpc-reply", NC));
    assert!(attrs
        .iter()
        .any(|a| a.name == "message-id" && a.value == "m&1"));
    assert!(attrs
        .iter()
        .any(|a| a.name == "ex:trace" && a.namespace == "urn:example" && a.value == "t-9"));
}

#[test]
fn rust_refuses_what_the_capabilities_rule_out_before_any_handler_sees_it() {
    let base = caps(&[]);
    let writable = caps(&[rpc::WRITABLE_RUNNING]);
    let rpc = |op: &str| format!(r#"<rpc xmlns="{NC}" message-id="7">{op}</rpc>"#);
    for (op, capabilities, tag) in [
        ("<get-config><source><candidate/></source></get-config>", &writable, "invalid-value"),
        ("<get-config><source><startup/></source></get-config>", &writable, "invalid-value"),
        ("<edit-config><target><running/></target><config/></edit-config>", &base, "invalid-value"),
        ("<get-config><source><url>file:///etc/passwd</url></source></get-config>", &writable, "invalid-value"),
        ("<edit-config><target><running/></target><url>x</url></edit-config>", &writable, "operation-not-supported"),
        ("<commit/>", &writable, "operation-not-supported"),
        ("<validate><source><running/></source></validate>", &writable, "operation-not-supported"),
        ("<copy-config><target><running/></target><source><startup/></source></copy-config>", &writable, "operation-not-supported"),
        ("<delete-config><target><startup/></target></delete-config>", &writable, "operation-not-supported"),
        ("<get><filter type=\"xpath\" select=\"/a\"/></get>", &writable, "bad-attribute"),
        ("<edit-config><target><running/></target><test-option>test-only</test-option><config/></edit-config>", &writable, "invalid-value"),
        ("<edit-config><target><running/></target><error-option>rollback-on-error</error-option><config/></edit-config>", &writable, "invalid-value"),
        ("<edit-config><target><running/></target></edit-config>", &writable, "missing-element"),
        ("<get-config/>", &writable, "missing-element"),
        ("<get/><get/>", &writable, "malformed-message"),
        ("<lock><target><running/><candidate/></target></lock>", &writable, "missing-element"),
        ("<commit><confirmed/></commit>", &caps(&[rpc::CANDIDATE]), "operation-not-supported"),
    ] {
        let (got, rendered) = refusal(&rpc(op), capabilities);
        assert_eq!(got, tag, "{op}");
        assert!(rendered.contains("message-id=\"7\""), "{rendered}");
    }
    let missing = format!(r#"<rpc xmlns="{NC}"><get/></rpc>"#);
    let (tag, rendered) = refusal(&missing, &base);
    assert_eq!(tag, "missing-attribute");
    let doc = xml::parse(rendered.as_bytes()).unwrap();
    let bad = doc
        .nodes
        .iter()
        .position(|n| matches!(n, xml::Node::Start { name, .. } if name == "bad-attribute"))
        .expect("bad-attribute");
    assert_eq!(
        xml::element(&doc, bad).unwrap().1,
        NC,
        "RFC 6241 error-info elements are in the base namespace"
    );
    assert_eq!(xml::text(&doc, bad).unwrap(), "message-id");
    // With the capability, the same requests go through to the handler.
    for (op, extra) in [
        (
            "<get-config><source><candidate/></source></get-config>",
            rpc::CANDIDATE,
        ),
        ("<commit/>", rpc::CANDIDATE),
        (
            "<validate><source><running/></source></validate>",
            rpc::VALIDATE_11,
        ),
        (
            "<get><filter type=\"xpath\" select=\"/a\"/></get>",
            rpc::XPATH,
        ),
    ] {
        let incoming = rpc::parse_rpc(rpc(op).as_bytes(), &caps(&[extra]))
            .ok()
            .unwrap_or_else(|| panic!("{op} refused with {extra}"));
        assert!(!incoming.operation.is_empty());
    }
}

#[test]
fn messages_that_are_not_an_rpc_are_fatal_not_answered() {
    for raw in [
        "<hello xmlns=\"urn:ietf:params:xml:ns:netconf:base:1.0\"/>",
        "<rpc message-id=\"1\"><get/></rpc>",
        "not xml",
    ] {
        assert!(
            matches!(
                rpc::parse_rpc(raw.as_bytes(), &caps(&[])),
                Err(rpc::RpcRefusal::Fatal(_))
            ),
            "{raw}"
        );
    }
}

#[test]
fn hello_negotiation_picks_the_highest_shared_base_and_refuses_none() {
    let both = caps(&[]);
    assert_eq!(
        rpc::negotiate(&both, &[rpc::BASE_10.into(), rpc::BASE_11.into()]),
        Some(netget::server::netconf::wire::Framing::Chunked)
    );
    assert_eq!(
        rpc::negotiate(&both, &[rpc::BASE_10.into()]),
        Some(netget::server::netconf::wire::Framing::Delimiter)
    );
    assert_eq!(rpc::negotiate(&both, &["urn:other:1".into()]), None);
    let hello = rpc::hello(&both, Some(7)).unwrap();
    let parsed = rpc::parse_hello(&hello).unwrap();
    assert_eq!(parsed.session_id, Some(7));
    assert_eq!(parsed.capabilities, both);
    assert!(rpc::parse_hello(
        format!("<hello xmlns=\"{NC}\"><session-id>1</session-id></hello>").as_bytes()
    )
    .is_err());
    assert!(rpc::parse_hello(format!("<hello xmlns=\"{NC}\"><capabilities><capability>has space</capability></capabilities></hello>").as_bytes()).is_err());
    assert!(rpc::hello(&["no-colon".into()], None).is_err());
}

#[test]
fn handler_xml_is_parsed_not_spliced() {
    for bad in [
        "</data></rpc-reply><rpc-reply message-id=\"9\"><ok/>",
        "<!DOCTYPE x [<!ENTITY e SYSTEM 'file:///etc/passwd'>]><a>&e;</a>",
        "<a>&undeclared;</a>",
        "<p:a/>",
        "<?xml version=\"1.0\"?><a/>",
        "<a>",
    ] {
        assert!(
            reply_body(&json!({"type":"netconf_rpc_reply","data_xml":bad})).is_err(),
            "{bad} was accepted"
        );
    }
    // A no-namespace element stays in no namespace inside <data>, whose default is NC.
    let body = reply_body(
        &json!({"type":"netconf_rpc_reply","data_xml":"<plain>1</plain><q xmlns=\"urn:q\">2</q>"}),
    )
    .unwrap();
    let out = rpc::reply(
        &[xml::Attribute {
            name: "message-id".into(),
            namespace: String::new(),
            value: "1".into(),
        }],
        &[],
        &body,
    )
    .unwrap();
    let doc = xml::parse(&out).unwrap();
    let data = xml::children(&doc, 0).unwrap()[0];
    let kids = xml::children(&doc, data).unwrap();
    assert_eq!(xml::element(&doc, kids[0]).unwrap().1, "");
    assert_eq!(xml::element(&doc, kids[1]).unwrap().1, "urn:q");
}

#[test]
fn reply_actions_need_exactly_one_shape_and_real_error_tags() {
    for bad in [
        json!({"type":"netconf_rpc_reply"}),
        json!({"type":"netconf_rpc_reply","ok":true,"data_xml":""}),
        json!({"type":"netconf_rpc_reply","ok":false}),
        json!({"type":"netconf_rpc_reply","errors":[]}),
        json!({"type":"netconf_rpc_reply","errors":[{"error_tag":"made-up"}]}),
        json!({"type":"netconf_rpc_reply","errors":[{"error_tag":"in-use","error_type":"session"}]}),
        json!({"type":"netconf_rpc_reply","errors":[{"error_tag":"in-use","error_severity":"fatal"}]}),
        json!({"type":"netconf_rpc_reply","errors":[{"error_tag":"in-use","surprise":1}]}),
    ] {
        assert!(reply_body(&bad).is_err(), "{bad} was accepted");
    }
    assert!(
        matches!(reply_body(&json!({"type":"netconf_rpc_reply","data_xml":""})).unwrap(), rpc::ReplyBody::Data(d) if d.nodes.is_empty())
    );
}

#[test]
fn client_side_reply_parsing_reads_every_part_and_requires_content() {
    let raw = format!(
        r#"<rpc-reply xmlns="{NC}" message-id="3"><rpc-error><error-type>application</error-type><error-tag>data-missing</error-tag><error-severity>error</error-severity><error-path>/a</error-path><error-message xml:lang="en">gone</error-message><error-info><bad-element>a</bad-element></error-info></rpc-error></rpc-reply>"#
    );
    let reply = rpc::parse_reply(raw.as_bytes()).unwrap();
    assert_eq!(reply.message_id.as_deref(), Some("3"));
    let e = &reply.body["errors"][0];
    assert_eq!(e["error_tag"], "data-missing");
    assert_eq!(e["error_path"], "/a");
    assert_eq!(e["error_message"], "gone");
    assert!(e["error_info_xml"]
        .as_str()
        .unwrap()
        .contains("bad-element"));
    let data = rpc::parse_reply(format!(r#"<rpc-reply xmlns="{NC}" message-id="4"><data><x xmlns="urn:x">1</x></data></rpc-reply>"#).as_bytes()).unwrap();
    assert_eq!(data.body["data_xml"], "<x xmlns=\"urn:x\">1</x>");
    assert!(
        rpc::parse_reply(format!(r#"<rpc-reply xmlns="{NC}" message-id="5"/>"#).as_bytes())
            .is_err()
    );
    assert!(rpc::parse_reply(
        format!(r#"<rpc xmlns="{NC}" message-id="5"><ok/></rpc>"#).as_bytes()
    )
    .is_err());
}

#[test]
fn client_requests_carry_rust_message_ids_and_respect_server_capabilities() {
    let writable = caps(&[rpc::WRITABLE_RUNNING]);
    let (op, message) = rpc::build_rpc(41, &json!({"operation":"edit-config","target":"running","default_operation":"replace","config_xml":"<a xmlns=\"urn:a\">x &amp; y</a>"}), &writable).unwrap();
    assert_eq!(op, "edit-config");
    let doc = xml::parse(&message).unwrap();
    assert!(xml::element(&doc, 0)
        .unwrap()
        .2
        .iter()
        .any(|a| a.name == "message-id" && a.value == "41"));
    let text = String::from_utf8(message).unwrap();
    assert!(
        text.contains("<default-operation>replace</default-operation>")
            && text.contains("x &amp; y"),
        "{text}"
    );
    for (action, capabilities) in [
        (
            json!({"operation":"edit-config","target":"running","config_xml":"<a xmlns=\"urn:a\"/>"}),
            caps(&[]),
        ),
        (
            json!({"operation":"edit-config","target":"running","config_xml":""}),
            writable.clone(),
        ),
        (
            json!({"operation":"get-config","source":"startup"}),
            writable.clone(),
        ),
        (
            json!({"operation":"edit-config","target":"running","error_option":"rollback-on-error","config_xml":"<a xmlns=\"urn:a\"/>"}),
            writable.clone(),
        ),
        (
            json!({"operation":"kill-session","session_id":0}),
            writable.clone(),
        ),
        (
            json!({"operation":"custom","input_xml":"<a xmlns=\"urn:a\"/><b xmlns=\"urn:b\"/>"}),
            writable.clone(),
        ),
        (
            json!({"operation":"custom","input_xml":"<plain/>"}),
            writable.clone(),
        ),
    ] {
        assert!(
            rpc::build_rpc(1, &action, &capabilities).is_err(),
            "{action} was built"
        );
    }
    let (_, custom) = rpc::build_rpc(2, &json!({"operation":"custom","input_xml":"<reboot xmlns=\"urn:sys\"><delay>5</delay></reboot>"}), &writable).unwrap();
    let doc = xml::parse(&custom).unwrap();
    let op = xml::children(&doc, 0).unwrap()[0];
    assert_eq!(xml::element(&doc, op).unwrap().0, "reboot");
}
