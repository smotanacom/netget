use netget::server::netconf::{
    wire::{self, Decoder, Framing},
    xml::{self, Document, Node},
};

#[test]
fn native_delimiter_and_octet_chunks_survive_fragmentation_and_transition() {
    let hello = b"<hello/>]]>]]>";
    let mut decoder = Decoder::new(Framing::Delimiter);
    for byte in &hello[..hello.len() - 1] {
        decoder.feed(&[*byte]).unwrap();
        assert!(decoder.next_message().unwrap().is_none());
    }
    decoder.feed(&hello[hello.len() - 1..]).unwrap();
    // An independent peer can send its negotiated frame in the same SSH data.
    decoder
        .feed(b"\n#3\n<a>\n#2\n\xce\xb1\n#4\n</a>\n##\n")
        .unwrap();
    assert_eq!(decoder.next_message().unwrap().unwrap(), b"<hello/>");
    decoder.set_framing(Framing::Chunked).unwrap();
    assert_eq!(
        decoder.next_message().unwrap().unwrap(),
        "<a>α</a>".as_bytes()
    );
    assert!(!decoder.is_partial());
    assert_eq!(
        wire::frame("<a>α</a>".as_bytes(), Framing::Chunked).unwrap(),
        "\n#9\n<a>α</a>\n##\n".as_bytes()
    );
    let raw = b"\n#1\nx\n##\n";
    for split in 0..raw.len() {
        let mut decoder = Decoder::new(Framing::Chunked);
        decoder.feed(&raw[..split]).unwrap();
        assert!(decoder.next_message().unwrap().is_none());
        decoder.feed(&raw[split..]).unwrap();
        assert_eq!(decoder.next_message().unwrap().unwrap(), b"x");
    }
}
#[test]
fn framing_native_grammar_and_declared_byte_chunk_queue_caps_are_exact() {
    for invalid in [
        b"\n#0\nx\n##\n".as_slice(),
        b"\n#01\nx\n##\n",
        b"\n#+1\nx\n##\n",
        b"\n#-1\nx\n##\n",
        b"\r\n#1\nx\n##\n",
        b"\n##\n",
        b"\n#4294967296\n",
        b"\n#4294967295\n",
        b"\n#1x\n",
        b"\n###\n",
    ] {
        let mut decoder = Decoder::new(Framing::Chunked);
        decoder.feed(invalid).unwrap();
        assert!(
            decoder.next_message().is_err(),
            "malformed/native-over-limit header must fail before body allocation"
        );
    }
    for framing in [Framing::Delimiter, Framing::Chunked] {
        let maximum = vec![b'x'; wire::MAX_MESSAGE_BYTES];
        let raw = wire::frame(&maximum, framing).unwrap();
        let mut decoder = Decoder::new(framing);
        decoder.feed(&raw).unwrap();
        assert_eq!(decoder.next_message().unwrap().unwrap(), maximum);
        assert!(wire::frame(&vec![b'x'; wire::MAX_MESSAGE_BYTES + 1], framing).is_err());
        let mut raw = vec![b'x'; wire::MAX_MESSAGE_BYTES + 1];
        if framing == Framing::Delimiter {
            raw.extend_from_slice(wire::DELIMITER);
        } else {
            raw = format!("\n#{}\n", raw.len()).into_bytes();
        }
        decoder.feed(&raw).unwrap();
        assert!(decoder.next_message().is_err());
    }
    for n in [wire::MAX_CHUNKS, wire::MAX_CHUNKS + 1] {
        let mut raw = b"\n#1\nx".repeat(n);
        raw.extend_from_slice(b"\n##\n");
        let mut decoder = Decoder::new(Framing::Chunked);
        decoder.feed(&raw).unwrap();
        if n == wire::MAX_CHUNKS {
            assert_eq!(decoder.next_message().unwrap().unwrap().len(), n);
        } else {
            assert!(decoder.next_message().is_err());
        }
    }
    let mut decoder = Decoder::new(Framing::Chunked);
    decoder.feed(&vec![b'x'; wire::MAX_BUFFER_BYTES]).unwrap();
    assert!(decoder.feed(b"x").is_err());
    let mut decoder = Decoder::new(Framing::Chunked);
    decoder.feed(b"\n#1\n").unwrap();
    assert!(decoder.next_message().unwrap().is_none());
    assert!(decoder.is_partial());
    assert!(decoder.set_framing(Framing::Delimiter).is_err());
}
#[test]
fn namespaces_mixed_text_attributes_and_qname_context_survive_a_typed_exchange() {
    let document = xml::parse(br#"<nc:rpc xmlns:nc="urn:ietf:params:xml:ns:netconf:base:1.0"><nc:data xmlns:p="urn:demo&amp;v" xmlns:xsi="http://www.w3.org/2001/XMLSchema-instance"><p:item xsi:type="p:Type" owner="a&#10;b">before<![CDATA[<&>]]>after<p:leaf>text</p:leaf>tail</p:item></nc:data></nc:rpc>"#).unwrap();
    let data = xml::children(&document, 0).unwrap()[0];
    let fragment = xml::inner(&document, data).unwrap();
    assert!(fragment
        .context
        .iter()
        .any(|b| b.prefix == "p" && b.uri == "urn:demo&v"));
    let encoded =
        xml::embedded(&fragment, "data", "urn:ietf:params:xml:ns:netconf:base:1.0").unwrap();
    let native = xml::parse(encoded.as_bytes()).unwrap();
    let child = xml::children(&native, 0).unwrap()[0];
    let (name, namespace, attributes) = xml::element(&native, child).unwrap();
    assert_eq!((name, namespace), ("item", "urn:demo&v"));
    assert!(attributes.iter().any(|a| a.name == "xsi:type"
        && a.namespace == "http://www.w3.org/2001/XMLSchema-instance"
        && a.value == "p:Type"));
    assert!(attributes
        .iter()
        .any(|a| a.name == "owner" && a.namespace.is_empty() && a.value == "a\nb"));
    assert!(native
        .nodes
        .iter()
        .any(|n| matches!(n,Node::Text{text} if text=="before<&>after")));
    assert!(native
        .nodes
        .iter()
        .any(|n| matches!(n,Node::Text{text} if text=="tail")));
    assert!(xml::parse(
        br#"<r xmlns:xml="http://www.w3.org/XML/1998/namespac&#101;" xml:lang="en"/>"#
    )
    .is_ok());
}
#[test]
fn xml_refuses_entity_dtd_namespace_duplicate_and_lexical_bypasses() {
    for raw in [
        b"<!DOCTYPE r [<!ENTITY s SYSTEM 'file:///etc/passwd'>]><r>&s;</r>".as_slice(),
        b"<r>&unknown;</r>",
        b"<p:r/>",
        b"<r xmlns:p=''/>",
        b"<r xmlns:xml='urn:bad'/>",
        b"<r xmlns:p='urn:one' xmlns:q='urn:one' p:x='a' q:x='b'/>",
        b"<r x='a' x='b'/>",
        b"<r/><s/>",
        b"<r><s></r>",
        b"<:r/>",
        b"<r>&#0;</r>",
        b"<?xml version='1.0' encoding='UTF-16'?><r/>",
        b"<r/><?xml version='1.0'?>",
        b"<r><!--bad--comment--></r>",
    ] {
        assert!(
            xml::parse(raw).is_err(),
            "malformed or excluded XML must be refused"
        );
    }
    let doc = xml::parse(b"<r attr='a\r\nb&#10;c'/>").unwrap();
    assert_eq!(xml::element(&doc, 0).unwrap().2[0].value, "a b\nc");
}
#[test]
fn xml_depth_node_text_name_attribute_namespace_caps_are_exact() {
    for depth in [xml::MAX_DEPTH, xml::MAX_DEPTH + 1] {
        let mut raw = b"<r>".repeat(depth);
        raw.extend_from_slice(&b"</r>".repeat(depth));
        assert_eq!(xml::parse(&raw).is_ok(), depth == xml::MAX_DEPTH);
    }
    for nodes in [xml::MAX_NODES, xml::MAX_NODES + 2] {
        let raw = format!("<r>{}</r>", "<c/>".repeat((nodes - 2) / 2));
        let parsed = xml::parse(raw.as_bytes());
        assert_eq!(parsed.is_ok(), nodes == xml::MAX_NODES);
        if let Ok(parsed) = parsed {
            assert_eq!(parsed.nodes.len(), nodes);
        }
    }
    for size in [xml::MAX_TEXT_BYTES, xml::MAX_TEXT_BYTES + 1] {
        assert_eq!(
            xml::parse(format!("<r>{}</r>", "x".repeat(size)).as_bytes()).is_ok(),
            size == xml::MAX_TEXT_BYTES
        );
    }
    for size in [xml::MAX_NAME_BYTES, xml::MAX_NAME_BYTES + 1] {
        assert_eq!(
            xml::parse(format!("<{} />", "x".repeat(size)).as_bytes()).is_ok(),
            size == xml::MAX_NAME_BYTES
        );
    }
    for size in [xml::MAX_ATTRIBUTE_BYTES, xml::MAX_ATTRIBUTE_BYTES + 1] {
        assert_eq!(
            xml::parse(format!("<r a='{}'/>", "x".repeat(size)).as_bytes()).is_ok(),
            size == xml::MAX_ATTRIBUTE_BYTES
        );
    }
    for count in [xml::MAX_ATTRIBUTES, xml::MAX_ATTRIBUTES + 1] {
        let attrs: String = (0..count).map(|n| format!(" a{n}='v'")).collect();
        assert_eq!(
            xml::parse(format!("<r{attrs}/>").as_bytes()).is_ok(),
            count == xml::MAX_ATTRIBUTES
        );
    }
    for count in [xml::MAX_NAMESPACES, xml::MAX_NAMESPACES + 1] {
        let attrs: String = (0..count)
            .map(|n| format!(" xmlns:p{n}='urn:{n}'"))
            .collect();
        assert_eq!(
            xml::parse(format!("<r{attrs}/>").as_bytes()).is_ok(),
            count == xml::MAX_NAMESPACES
        );
    }
}
#[test]
fn typed_xml_cannot_inject_markup_change_attribute_namespace_or_bypass_wire_caps() {
    let doc = Document {
        context: vec![],
        nodes: vec![Node::Text {
            text: "<&>\r".into(),
        }],
    };
    let encoded = xml::embedded(&doc, "data", "urn:demo").unwrap();
    let parsed = xml::parse(encoded.as_bytes()).unwrap();
    assert_eq!(xml::text(&parsed, 0).unwrap(), "<&>\r");
    let bad = Document {
        context: vec![],
        nodes: vec![
            Node::Start {
                name: "r".into(),
                namespace: "".into(),
                bindings: vec![],
                attributes: vec![xml::Attribute {
                    name: "attr".into(),
                    namespace: "urn:hidden".into(),
                    value: "value".into(),
                }],
            },
            Node::End {
                name: "r".into(),
                namespace: "".into(),
            },
        ],
    };
    assert!(xml::embedded(&bad, "data", "urn:demo").is_err());
    let oversized = Document {
        context: vec![],
        nodes: (0..16)
            .map(|_| Node::Text {
                text: "&".repeat(xml::MAX_TEXT_BYTES),
            })
            .collect(),
    };
    assert!(xml::embedded(&oversized, "data", "urn:demo").is_err());
}
