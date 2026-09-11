//! What `SamlClient::parse_saml_response` reports, and what it refuses to report.
//!
//! These are the assertions that keep a *reader* from behaving like a *verifier*. NetGet
//! checks no XML signature — there is no key here to check one against — so the only defence
//! against a forged `<samlp:Response>` is that the status it reports is the status the
//! document's top-level `<StatusCode>` actually carries. Three ways that failed:
//!
//! * `status_code.contains("Success")` matched a *nested* second-level status code, so
//!   `<StatusCode Value="…:Requester"><StatusCode Value="…:Success"/></StatusCode>` — a
//!   refusal — was reported to the model as `success: true`.
//! * `<StatusCode>` was only recognised as an `Event::Empty`, so the outer element of that
//!   same nesting (a `Start`) was skipped and the *inner* one became "the" status.
//! * Element names were matched as raw qualified bytes (`b"saml:NameID"`), so an assertion
//!   from any IDP using a different prefix — `saml2:`, which Shibboleth and ADFS emit — was
//!   parsed as having no subject and no attributes, silently.
//!
//! Run with:
//!   ./cargo-isolated.sh test --no-default-features --features saml \
//!       --test client -- --test-threads=100 saml::response_parsing

#[cfg(all(test, feature = "saml"))]
mod saml_response_parsing {
    use netget::client::saml::SamlClient;

    const SUCCESS: &str = "urn:oasis:names:tc:SAML:2.0:status:Success";
    const REQUESTER: &str = "urn:oasis:names:tc:SAML:2.0:status:Requester";

    /// A genuine success is reported as one, with subject and attributes.
    #[test]
    fn plain_success_is_success() {
        let xml = format!(
            r#"<samlp:Response xmlns:samlp="urn:oasis:names:tc:SAML:2.0:protocol"
                              xmlns:saml="urn:oasis:names:tc:SAML:2.0:assertion">
                 <samlp:Status><samlp:StatusCode Value="{SUCCESS}"/></samlp:Status>
                 <saml:Assertion>
                   <saml:Subject><saml:NameID>alice@example.com</saml:NameID></saml:Subject>
                   <saml:AttributeStatement>
                     <saml:Attribute Name="email">
                       <saml:AttributeValue>alice@example.com</saml:AttributeValue>
                     </saml:Attribute>
                   </saml:AttributeStatement>
                 </saml:Assertion>
               </samlp:Response>"#
        );

        let (success, status, assertion, attrs) = SamlClient::parse_saml_response(&xml).unwrap();
        assert!(success, "a top-level Success must parse as success");
        assert_eq!(status, SUCCESS);
        assert_eq!(
            assertion.as_ref().and_then(|a| a["subject"].as_str()),
            Some("alice@example.com")
        );
        assert_eq!(
            attrs.as_ref().and_then(|a| a["email"].as_str()),
            Some("alice@example.com")
        );
    }

    /// **The forgery this parser used to accept.**
    ///
    /// SAML nests `<StatusCode>` inside `<StatusCode>` to carry second-level detail. The
    /// top-level code is the verdict; the inner one is commentary and is fully attacker
    /// controlled. `contains("Success")` on a last-wins scan read the inner one, so a
    /// refusal arrived at the model as a completed sign-in.
    #[test]
    fn nested_success_inside_a_failure_is_still_a_failure() {
        let xml = format!(
            r#"<samlp:Response xmlns:samlp="urn:oasis:names:tc:SAML:2.0:protocol">
                 <samlp:Status>
                   <samlp:StatusCode Value="{REQUESTER}">
                     <samlp:StatusCode Value="{SUCCESS}"/>
                   </samlp:StatusCode>
                 </samlp:Status>
               </samlp:Response>"#
        );

        let (success, status, assertion, _) = SamlClient::parse_saml_response(&xml).unwrap();
        assert!(
            !success,
            "a second-level StatusCode must not decide the outcome"
        );
        assert_eq!(status, REQUESTER, "the top-level code is the verdict");
        assert!(
            assertion.is_none(),
            "no assertion data may be produced for a failure"
        );
    }

    /// Equality, not substring. `contains("Success")` matched anything with the word in it.
    #[test]
    fn a_status_uri_that_merely_contains_success_is_not_success() {
        for value in [
            "urn:example:NotSuccessful",
            "urn:oasis:names:tc:SAML:2.0:status:Success.evil",
            "prefixurn:oasis:names:tc:SAML:2.0:status:Success",
        ] {
            let xml = format!(
                r#"<samlp:Response xmlns:samlp="urn:oasis:names:tc:SAML:2.0:protocol">
                     <samlp:Status><samlp:StatusCode Value="{value}"/></samlp:Status>
                   </samlp:Response>"#
            );
            let (success, status, _, _) = SamlClient::parse_saml_response(&xml).unwrap();
            assert!(!success, "{value} must not parse as a successful sign-in");
            assert_eq!(status, value);
        }
    }

    /// A `<StatusCode>` outside `<Status>` is not the response's status. Without the
    /// containment check, any element of that name anywhere in the document could set it.
    #[test]
    fn a_status_code_outside_status_is_ignored() {
        let xml = format!(
            r#"<samlp:Response xmlns:samlp="urn:oasis:names:tc:SAML:2.0:protocol"
                              xmlns:saml="urn:oasis:names:tc:SAML:2.0:assertion">
                 <samlp:Status><samlp:StatusCode Value="{REQUESTER}"/></samlp:Status>
                 <saml:Advice><samlp:StatusCode Value="{SUCCESS}"/></saml:Advice>
               </samlp:Response>"#
        );

        let (success, status, _, _) = SamlClient::parse_saml_response(&xml).unwrap();
        assert!(!success);
        assert_eq!(status, REQUESTER);
    }

    /// SAML fixes the namespace URIs, not the prefixes. Shibboleth and ADFS emit `saml2:`;
    /// matching qualified bytes made those responses parse as empty.
    #[test]
    fn a_different_namespace_prefix_parses_identically() {
        let xml = format!(
            r#"<saml2p:Response xmlns:saml2p="urn:oasis:names:tc:SAML:2.0:protocol"
                               xmlns:saml2="urn:oasis:names:tc:SAML:2.0:assertion">
                 <saml2p:Status><saml2p:StatusCode Value="{SUCCESS}"/></saml2p:Status>
                 <saml2:Assertion>
                   <saml2:Subject><saml2:NameID>bob@example.com</saml2:NameID></saml2:Subject>
                   <saml2:AttributeStatement>
                     <saml2:Attribute Name="role">
                       <saml2:AttributeValue>admin</saml2:AttributeValue>
                     </saml2:Attribute>
                   </saml2:AttributeStatement>
                 </saml2:Assertion>
               </saml2p:Response>"#
        );

        let (success, status, assertion, attrs) = SamlClient::parse_saml_response(&xml).unwrap();
        assert!(success);
        assert_eq!(status, SUCCESS);
        assert_eq!(
            assertion.as_ref().and_then(|a| a["subject"].as_str()),
            Some("bob@example.com"),
            "a saml2:-prefixed NameID must be found"
        );
        assert_eq!(
            attrs.as_ref().and_then(|a| a["role"].as_str()),
            Some("admin")
        );
    }

    /// The first `<NameID>` is the subject. A later one — in `<Advice>`, or in a second
    /// assertion appended after the real one — must not replace it.
    #[test]
    fn a_later_nameid_cannot_replace_the_subject() {
        let xml = format!(
            r#"<samlp:Response xmlns:samlp="urn:oasis:names:tc:SAML:2.0:protocol"
                              xmlns:saml="urn:oasis:names:tc:SAML:2.0:assertion">
                 <samlp:Status><samlp:StatusCode Value="{SUCCESS}"/></samlp:Status>
                 <saml:Assertion>
                   <saml:Subject><saml:NameID>alice@example.com</saml:NameID></saml:Subject>
                 </saml:Assertion>
                 <saml:Assertion>
                   <saml:Subject><saml:NameID>attacker@evil.example</saml:NameID></saml:Subject>
                 </saml:Assertion>
               </samlp:Response>"#
        );

        let (_, _, assertion, _) = SamlClient::parse_saml_response(&xml).unwrap();
        assert_eq!(
            assertion.as_ref().and_then(|a| a["subject"].as_str()),
            Some("alice@example.com")
        );
    }

    /// **Billion laughs, and why it cannot work here.**
    ///
    /// `quick_xml` never processes a DTD: a `<!DOCTYPE>` with an internal subset arrives as
    /// an opaque `Event::DocType` that nothing acts on, and `unescape()` resolves only the
    /// five predefined entities plus numeric character references. So `&lol9;` is not
    /// expanded — it is an unrecognised symbol, the text is dropped, and the parse finishes
    /// in microseconds. This test exists so that stays true: swapping in a parser that *does*
    /// honour entity declarations would hang or exhaust memory here rather than in
    /// production.
    #[test]
    fn entity_expansion_does_not_happen() {
        let mut dtd = String::from("<!DOCTYPE r [<!ENTITY lol0 \"lol\">");
        for i in 1..=9 {
            dtd.push_str(&format!(
                "<!ENTITY lol{i} \"&lol{};&lol{};&lol{};&lol{};&lol{};&lol{};&lol{};&lol{};&lol{};&lol{};\">",
                i - 1, i - 1, i - 1, i - 1, i - 1, i - 1, i - 1, i - 1, i - 1, i - 1
            ));
        }
        dtd.push(']');
        dtd.push('>');

        let xml = format!(
            r#"{dtd}<samlp:Response xmlns:samlp="urn:oasis:names:tc:SAML:2.0:protocol"
                                   xmlns:saml="urn:oasis:names:tc:SAML:2.0:assertion">
                 <samlp:Status><samlp:StatusCode Value="{REQUESTER}"/></samlp:Status>
                 <saml:Assertion><saml:AttributeStatement>
                   <saml:Attribute Name="boom">
                     <saml:AttributeValue>&lol9;</saml:AttributeValue>
                   </saml:Attribute>
                 </saml:AttributeStatement></saml:Assertion>
               </samlp:Response>"#
        );

        let started = std::time::Instant::now();
        let (success, status, _, attrs) = SamlClient::parse_saml_response(&xml).unwrap();
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "parsing must not expand entities; it took {:?}",
            started.elapsed()
        );
        assert!(!success);
        assert_eq!(status, REQUESTER);
        // The unresolvable entity yields no attribute value rather than a billion `lol`s.
        let expanded = attrs
            .as_ref()
            .and_then(|a| a["boom"].as_str())
            .unwrap_or_default();
        assert!(
            expanded.len() < 1024,
            "entity was expanded to {} bytes",
            expanded.len()
        );
    }

    /// Nesting past the bound is refused rather than walked. `quick_xml` does not recurse,
    /// so this is not stack-overflow protection — it bounds pathological work.
    #[test]
    fn deep_nesting_is_refused() {
        let depth = SamlClient::MAX_XML_DEPTH + 10;
        let mut xml = String::new();
        for _ in 0..depth {
            xml.push_str("<a>");
        }
        for _ in 0..depth {
            xml.push_str("</a>");
        }
        let err = SamlClient::parse_saml_response(&xml)
            .expect_err("a document nested past the bound must be refused");
        assert!(
            err.to_string().contains("nests deeper"),
            "unexpected error: {err}"
        );
    }

    /// A document with no `<Status>` at all is not a success, and does not panic.
    #[test]
    fn a_response_with_no_status_is_not_a_success() {
        for xml in [
            "",
            "<samlp:Response xmlns:samlp=\"urn:oasis:names:tc:SAML:2.0:protocol\"/>",
            "not xml at all",
        ] {
            let (success, status, assertion, _) = SamlClient::parse_saml_response(xml).unwrap();
            assert!(!success, "{xml:?} must not parse as a successful sign-in");
            assert_eq!(status, "urn:oasis:names:tc:SAML:2.0:status:Unknown");
            assert!(assertion.is_none());
        }
    }

    /// An operator-supplied `entity_id` or `acs_url` goes into the AuthnRequest as an XML
    /// attribute and an element. Unescaped, a `"` closed the attribute early.
    #[test]
    fn xml_metacharacters_are_escaped() {
        assert_eq!(
            SamlClient::escape_xml(r#"a"b<c>d&e'f"#),
            "a&quot;b&lt;c&gt;d&amp;e&apos;f"
        );
    }

    /// Prefix stripping is on the *last* colon, and a name with no prefix is itself.
    #[test]
    fn local_name_strips_the_prefix() {
        assert_eq!(SamlClient::local_name(b"saml2:NameID"), b"NameID");
        assert_eq!(SamlClient::local_name(b"NameID"), b"NameID");
        assert_eq!(SamlClient::local_name(b"a:b:C"), b"C");
    }
}
