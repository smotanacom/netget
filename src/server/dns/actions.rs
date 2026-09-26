//! DNS protocol actions implementation

use crate::llm::actions::{
    protocol_trait::{ActionResult, Protocol, Server},
    ActionDefinition, Parameter,
};
use crate::protocol::log_template::LogTemplate;
use crate::protocol::EventType;
use crate::state::app_state::AppState;
use anyhow::{Context, Result};
use hickory_proto::op::{Header, Message as DnsMessage, MessageType, OpCode, Query, ResponseCode};
use hickory_proto::rr::{rdata, Name, RData, Record, RecordType};
use serde_json::json;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::str::FromStr;
use std::sync::LazyLock;

/// Longest DNS `<character-string>`, in octets (RFC 1035 §3.3).
///
/// A character-string is one length octet followed by that many characters, so 255 is what
/// the format can describe — not a policy this server chose. `send_dns_txt_response` emits
/// the TXT record as a single character-string, so this is its ceiling.
///
/// It is enforced in `execute_send_dns_txt_response` rather than left to hickory, and the
/// difference is not cosmetic: hickory's encoder does refuse, but it refuses inside
/// `message.to_vec()`, which fails the whole action with "Failed to serialize DNS message".
/// `src/server/dns/mod.rs` then takes its `Ok` branch with no `ActionResult::Output` in hand
/// and writes **nothing at all** — the peer waits out its own timeout, and the model is told
/// a serialisation error rather than the one number it needed. Stating the bound here turns a
/// fail-silent into an error the model can act on, which is the same reason
/// `coap::codec::MAX_PAYLOAD_LEN` is checked in `decode_payload` instead of at `send_to`.
pub const MAX_CHARACTER_STRING_LEN: usize = 255;

/// DNS protocol action handler
pub struct DnsProtocol;

impl Default for DnsProtocol {
    fn default() -> Self {
        Self::new()
    }
}

impl DnsProtocol {
    pub fn new() -> Self {
        Self
    }
}

// Implement Protocol trait (common functionality)
impl Protocol for DnsProtocol {
    fn get_async_actions(&self, _state: &AppState) -> Vec<ActionDefinition> {
        Vec::new() // DNS has no async actions
    }
    fn get_sync_actions(&self) -> Vec<ActionDefinition> {
        vec![
            send_dns_a_response_action(),
            send_dns_aaaa_response_action(),
            send_dns_cname_response_action(),
            send_dns_mx_response_action(),
            send_dns_txt_response_action(),
            send_dns_nxdomain_action(),
            send_dns_response_action(),
            ignore_query_action(),
        ]
    }
    fn protocol_name(&self) -> &'static str {
        "DNS"
    }
    fn get_event_types(&self) -> Vec<EventType> {
        get_dns_event_types()
    }
    fn stack_name(&self) -> &'static str {
        "ETH>IP>UDP>DNS"
    }
    fn keywords(&self) -> Vec<&'static str> {
        vec!["dns"]
    }
    fn metadata(&self) -> crate::protocol::metadata::ProtocolMetadataV2 {
        use crate::protocol::metadata::{
            DevelopmentState, PrivilegeRequirement, ProtocolMetadataV2,
        };

        ProtocolMetadataV2::builder()
            .connectionless()
            // Answers on failure: SERVFAIL echoing the query id, never silence.
            .answers_on_failure()
            // STABLE, set 16 September 2026 against the six conditions in the root CLAUDE.md,
            // each verified in that pass rather than inherited. Read `.e2e_testing` and
            // `.notes` before quoting it: the rating covers the surface this server
            // implements, which is plain UDP DNS with one record per answer.
            .state(DevelopmentState::Stable)
            .privilege_requirement(PrivilegeRequirement::PrivilegedPort(53))
            .implementation("hickory-proto for parsing and construction; UDP only, no TCP fallback")
            .llm_control("Response records (A, AAAA, MX, TXT, CNAME, NXDOMAIN)")
            .e2e_testing(
                "STABLE rests on all six conditions, each checked against source and by \
                 running it on 16 September 2026. (1) TWO independent resolvers from different \
                 projects, neither #[ignore]d and neither able to skip -- each FAILS naming its \
                 package when absent: dig_test.rs drives ISC BIND's dig and kdig_test.rs drives \
                 Knot DNS's kdig (CZ.NIC). Between them they decode EVERY record type this \
                 server can produce -- A, AAAA, CNAME, MX (preference 4660 = 0x1234, so a \
                 byte-swap reads 13330 rather than something plausible), the TXT \
                 character-string, and RCODE 3 for NXDOMAIN -- each through that resolver's own \
                 presentation writer, and each asserts no transaction-id mismatch. Verified by \
                 answering with a fixed id instead of the client's, at which point kdig \
                 discards the reply. dig_test.rs additionally runs ONE query as a person would \
                 actually invoke it, with EDNS offered and no flags, and it resolves: RFC 6891 \
                 §6.1.1 says a server that does not understand EDNS answers without an OPT \
                 record and the requestor treats it as non-EDNS. Without that query the whole \
                 rating would rest on a +noedns configuration nobody uses -- the \
                 mysql_native_password situation. (2) The pcap oracle reads both SERVFAIL paths \
                 in llm_failure_test.rs and a NOERROR answer with A rdata in bounds_test.rs, \
                 and hard-fails when tshark is missing. (3) fuzz/fuzz_targets/dns_message.rs: \
                 2,777,883 runs in 91s, clean; the corpus includes compression_pointer_loop, \
                 which is the depth bomb for a format whose recursion is the compression \
                 pointer. It fuzzes hickory-proto rather than NetGet code because NetGet has no \
                 DNS parser -- three registered protocols reach that decoder from an \
                 unauthenticated first datagram. (4) Every declared bound has a test in \
                 bounds_test.rs -- the 4096-byte receive buffer, query_id and MX preference as \
                 u16, the 12-octet floor on a raw message, the 255-octet character-string -- \
                 each verified by REMOVING the bound and recording what failed. (5) Both \
                 CLAUDE.md files were re-read against source in that pass; several claims were \
                 false and the corrections are in them. (6) No #[ignore] and no skip gate \
                 anywhere in tests/server/dns/. \
                 \
                 tests/server/dns/test.rs covers the same ground with hickory-client -- useful, \
                 but circular on its own, since hickory-client decodes with the same codec \
                 hickory-proto encoded with. It is not part of the evidence above. \
                 \
                 UNPROVEN, and this is the ceiling on what the two resolvers prove, because \
                 none of it is implemented: EDNS0 itself (an OPT record in a query is ignored \
                 and none is added to replies), TCP transport, DNSSEC, zone transfers, \
                 multi-record answers, and truncation with the TC bit -- an oversize response \
                 is sent rather than truncated.",
            )
            .notes(
                "Stable means the evidence for the implemented surface is complete, NOT that \
                 RFC 1035 is. Every action the model is offered is now decoded by two \
                 independent resolvers; what is missing is whole features, each listed in \
                 e2e_testing and in src/server/dns/CLAUDE.md. \
                 A query that produces no answer fails closed with SERVFAIL through one exit, \
                 `send_servfail`, tagged decision=fail_closed_llm_error / _llm_overload / \
                 _no_action; `ignore_query` is decision=model_silent and is the only path that \
                 writes nothing. The third of those did not exist before September 2026 and its \
                 absence was a fail-silent: a model answering with only common actions, or with \
                 a DNS action execute_action refused, produced no packet at all. \
                 Excellent scripting candidate; static handlers cannot echo the client's \
                 transaction ID, so use script mode for deterministic answers. \
                 Re-check rather than inherit: no CI job builds fuzz/ (it is its own \
                 workspace), and dig and kdig must be on PATH wherever the suite runs, since \
                 both tests hard-fail by design.",
            )
            .build()
    }
    fn description(&self) -> &'static str {
        "Domain name resolution server"
    }
    fn example_prompt(&self) -> &'static str {
        "DNS server on port 53 and resolve everything to 93.184.216.34"
    }
    fn group_name(&self) -> &'static str {
        "Core"
    }

    fn get_startup_examples(&self) -> crate::llm::actions::StartupExamples {
        use crate::llm::actions::StartupExamples;
        use serde_json::json;

        // A fixed-resolve DNS server: everything under example.com answers with a
        // fixed A record, everything else NXDOMAIN. Reads the event from stdin
        // and echoes the client's random transaction id (query_id) — the one
        // thing a static handler cannot do, which is why DNS needs script mode.
        let script = r#"import json, sys
data = json.load(sys.stdin)
event = data["event"]
if data["event_type_id"] == "dns_query":
    domain = event.get("domain", "").rstrip(".")
    if domain.endswith("example.com"):
        actions = [{"type": "send_dns_a_response", "query_id": event["query_id"],
                    "domain": event["domain"], "ip": "93.184.216.34", "ttl": 300}]
    else:
        actions = [{"type": "send_dns_nxdomain", "query_id": event["query_id"],
                    "domain": event["domain"],
                    "query_type": event.get("query_type", "A")}]
else:
    actions = []
print(json.dumps({"actions": actions}))"#;

        StartupExamples::new(
            // LLM mode: a different answer per subdomain — genuine reasoning.
            json!({
                "type": "open_server",
                "port": 53,
                "base_stack": "dns",
                "instruction": "Act as a DNS resolver that returns a different IP per query: map each queried hostname to a stable address in 10.0.0.0/8 derived from the name, but return the real public IP for well-known names such as www.example.com. Always echo the query's transaction id."
            }),
            // Script mode: deterministic fixed-resolve, no LLM call. The script
            // sees the event and can echo the client's random transaction id.
            json!({
                "type": "open_server",
                "port": 53,
                "base_stack": "dns",
                "event_handlers": [{
                    "event_pattern": "dns_query",
                    "handler": {
                        "type": "script",
                        "language": "python",
                        "code": script
                    }
                }]
            }),
            // Static mode: static handlers emit fixed action JSON with no access
            // to the event, so they cannot echo the client's random transaction
            // ID or the queried name - a static answer record would be silently
            // discarded by every real resolver. Dropping queries (a DNS
            // blackhole) is what static mode can do correctly here; use script
            // mode for deterministic answers.
            json!({
                "type": "open_server",
                "port": 53,
                "base_stack": "dns",
                "event_handlers": [{
                    "event_pattern": "dns_query",
                    "handler": {
                        "type": "static",
                        "actions": [{
                            "type": "ignore_query"
                        }]
                    }
                }]
            }),
        )
    }
}

// Implement Server trait (server-specific functionality)
impl Server for DnsProtocol {
    fn spawn(
        &self,
        ctx: crate::protocol::SpawnContext,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<std::net::SocketAddr>> + Send>,
    > {
        Box::pin(async move {
            use crate::server::dns::DnsServer;
            DnsServer::spawn_with_llm_actions(
                ctx.legacy_listen_addr(),
                ctx.llm_client,
                ctx.state,
                ctx.status_tx,
                ctx.server_id,
            )
            .await
        })
    }
    fn execute_action(&self, action: serde_json::Value) -> Result<ActionResult> {
        let action_type = action
            .get("type")
            .and_then(|v| v.as_str())
            .context("Missing 'type' field in action")?;

        match action_type {
            "send_dns_a_response" => self.execute_send_dns_a_response(action),
            "send_dns_aaaa_response" => self.execute_send_dns_aaaa_response(action),
            "send_dns_cname_response" => self.execute_send_dns_cname_response(action),
            "send_dns_mx_response" => self.execute_send_dns_mx_response(action),
            "send_dns_txt_response" => self.execute_send_dns_txt_response(action),
            "send_dns_nxdomain" => self.execute_send_dns_nxdomain(action),
            "send_dns_response" => self.execute_send_dns_response(action),
            "ignore_query" => Ok(ActionResult::NoAction),
            _ => Err(anyhow::anyhow!("Unknown DNS action: {}", action_type)),
        }
    }
}

/// Read the DNS transaction ID out of an action.
///
/// The transaction ID is how a client correlates a response with the query it
/// sent, and clients pick it at random. Silently truncating an out-of-range
/// value with `as u16` would produce a response the client discards without any
/// diagnostic, so an out-of-range value is a hard error instead.
fn parse_query_id(action: &serde_json::Value) -> Result<u16> {
    let raw = action
        .get("query_id")
        .and_then(|v| v.as_u64())
        .context("Missing 'query_id' parameter (echo the query_id from the dns_query event)")?;

    u16::try_from(raw).map_err(|_| {
        anyhow::anyhow!(
            "'query_id' must be a 16-bit DNS transaction ID (0-65535), got {}. \
             Echo the query_id from the dns_query event verbatim.",
            raw
        )
    })
}

/// Parse a DNS record type name (`"A"`, `"AAAA"`, `"MX"`, ...).
fn parse_record_type(name: &str) -> Result<RecordType> {
    RecordType::from_str(&name.to_ascii_uppercase())
        .with_context(|| format!("Unknown DNS record type: {name}"))
}

/// Build the shell of a DNS response: header plus the echoed question section.
///
/// RFC 1035 §4.1.2 requires a response to repeat the question it answers, and
/// real stub resolvers (glibc's, systemd-resolved, `dig`) discard responses
/// whose question section does not match the query they sent. The question is
/// reconstructed from the action's `domain` plus the record type implied by the
/// action, so answering a query with the matching action produces a response
/// those clients accept.
fn new_response(
    query_id: u16,
    domain: &str,
    query_type: RecordType,
    response_code: ResponseCode,
) -> Result<(DnsMessage, Name)> {
    let name =
        Name::from_str(domain).with_context(|| format!("Invalid domain name: '{domain}'"))?;

    let mut message = DnsMessage::new();
    let mut header = Header::new();
    header.set_id(query_id);
    header.set_message_type(MessageType::Response);
    header.set_op_code(OpCode::Query);
    header.set_authoritative(true);
    header.set_response_code(response_code);
    message.set_header(header);

    // Echo the question section (counts are recomputed by hickory at encode time).
    message.add_query(Query::query(name.clone(), query_type));

    Ok((message, name))
}

/// Build a SERVFAIL (RCODE 2) answer to `query`, for when the server cannot
/// produce a real answer at all — the LLM backend is down, overloaded, or
/// returned nothing usable.
///
/// RFC 1035 §4.1.1 gives exactly one code for "this server failed while
/// processing your query", and a resolver that receives it stops waiting and
/// moves to the next nameserver immediately. Writing nothing instead leaves the
/// resolver blocked for its own full timeout (5s in glibc, per server).
///
/// The transaction ID and the question section are copied from the query, for
/// the same reason `new_response` echoes them: a stub resolver discards any
/// response whose ID or question does not match what it sent, which would turn
/// this into silence again.
///
/// `AA` is deliberately *not* set: we are not answering authoritatively, we are
/// failing.
pub fn build_servfail(query: &DnsMessage) -> Result<Vec<u8>> {
    let mut message = DnsMessage::new();
    let mut header = Header::new();
    header.set_id(query.id());
    header.set_message_type(MessageType::Response);
    header.set_op_code(query.op_code());
    header.set_recursion_desired(query.recursion_desired());
    header.set_response_code(ResponseCode::ServFail);
    message.set_header(header);

    for question in query.queries() {
        message.add_query(question.clone());
    }

    message
        .to_vec()
        .context("Failed to serialize DNS SERVFAIL response")
}

/// Serialize a response message to DNS wire format.
fn finish_response(message: DnsMessage) -> Result<ActionResult> {
    let bytes = message
        .to_vec()
        .context("Failed to serialize DNS message")?;
    Ok(ActionResult::Output(bytes))
}

fn ttl_of(action: &serde_json::Value) -> u32 {
    action
        .get("ttl")
        .and_then(|v| v.as_u64())
        .unwrap_or(300)
        .min(u32::MAX as u64) as u32
}

fn required_str<'a>(action: &'a serde_json::Value, key: &str) -> Result<&'a str> {
    action
        .get(key)
        .and_then(|v| v.as_str())
        .with_context(|| format!("Missing '{key}' parameter"))
}

impl DnsProtocol {
    fn execute_send_dns_a_response(&self, action: serde_json::Value) -> Result<ActionResult> {
        let query_id = parse_query_id(&action)?;
        let domain = required_str(&action, "domain")?;
        let ip = required_str(&action, "ip")?;
        let ttl = ttl_of(&action);

        let ipv4 =
            Ipv4Addr::from_str(ip).with_context(|| format!("Invalid IPv4 address: '{ip}'"))?;

        let (mut message, name) =
            new_response(query_id, domain, RecordType::A, ResponseCode::NoError)?;

        let mut record = Record::with(name, RecordType::A, ttl);
        record.set_data(Some(RData::A(rdata::A(ipv4))));
        message.add_answer(record);

        finish_response(message)
    }

    fn execute_send_dns_aaaa_response(&self, action: serde_json::Value) -> Result<ActionResult> {
        let query_id = parse_query_id(&action)?;
        let domain = required_str(&action, "domain")?;
        let ip = required_str(&action, "ip")?;
        let ttl = ttl_of(&action);

        let ipv6 =
            Ipv6Addr::from_str(ip).with_context(|| format!("Invalid IPv6 address: '{ip}'"))?;

        let (mut message, name) =
            new_response(query_id, domain, RecordType::AAAA, ResponseCode::NoError)?;

        let mut record = Record::with(name, RecordType::AAAA, ttl);
        record.set_data(Some(RData::AAAA(rdata::AAAA(ipv6))));
        message.add_answer(record);

        finish_response(message)
    }

    fn execute_send_dns_cname_response(&self, action: serde_json::Value) -> Result<ActionResult> {
        let query_id = parse_query_id(&action)?;
        let domain = required_str(&action, "domain")?;
        let target = required_str(&action, "target")?;
        let ttl = ttl_of(&action);

        let target_name = Name::from_str(target)
            .with_context(|| format!("Invalid target domain name: '{target}'"))?;

        // A CNAME may be returned for any query type; the question echoes CNAME
        // unless the caller says which type was actually asked for.
        let query_type = match action.get("query_type").and_then(|v| v.as_str()) {
            Some(qt) => parse_record_type(qt)?,
            None => RecordType::CNAME,
        };

        let (mut message, name) =
            new_response(query_id, domain, query_type, ResponseCode::NoError)?;

        let mut record = Record::with(name, RecordType::CNAME, ttl);
        record.set_data(Some(RData::CNAME(rdata::CNAME(target_name))));
        message.add_answer(record);

        finish_response(message)
    }

    fn execute_send_dns_mx_response(&self, action: serde_json::Value) -> Result<ActionResult> {
        let query_id = parse_query_id(&action)?;
        let domain = required_str(&action, "domain")?;
        let exchange = required_str(&action, "exchange")?;
        let ttl = ttl_of(&action);

        let preference_raw = action
            .get("preference")
            .and_then(|v| v.as_u64())
            .unwrap_or(10);
        let preference = u16::try_from(preference_raw).map_err(|_| {
            anyhow::anyhow!("'preference' must be in the range 0-65535, got {preference_raw}")
        })?;

        let exchange_name = Name::from_str(exchange)
            .with_context(|| format!("Invalid exchange domain name: '{exchange}'"))?;

        let (mut message, name) =
            new_response(query_id, domain, RecordType::MX, ResponseCode::NoError)?;

        let mut record = Record::with(name, RecordType::MX, ttl);
        record.set_data(Some(RData::MX(rdata::MX::new(preference, exchange_name))));
        message.add_answer(record);

        finish_response(message)
    }

    fn execute_send_dns_txt_response(&self, action: serde_json::Value) -> Result<ActionResult> {
        let query_id = parse_query_id(&action)?;
        let domain = required_str(&action, "domain")?;
        let text = required_str(&action, "text")?;
        let ttl = ttl_of(&action);

        if text.len() > MAX_CHARACTER_STRING_LEN {
            anyhow::bail!(
                "'text' is {} octets, over the {}-octet limit. RFC 1035 §3.3 defines a DNS \
                 <character-string> as one length octet followed by that many characters, so \
                 {} is what a single length octet can describe and this server emits the TXT \
                 record as one string. Shorten it.",
                text.len(),
                MAX_CHARACTER_STRING_LEN,
                MAX_CHARACTER_STRING_LEN
            );
        }

        let (mut message, name) =
            new_response(query_id, domain, RecordType::TXT, ResponseCode::NoError)?;

        let mut record = Record::with(name, RecordType::TXT, ttl);
        record.set_data(Some(RData::TXT(rdata::TXT::new(vec![text.to_string()]))));
        message.add_answer(record);

        finish_response(message)
    }

    fn execute_send_dns_nxdomain(&self, action: serde_json::Value) -> Result<ActionResult> {
        let query_id = parse_query_id(&action)?;
        let domain = required_str(&action, "domain")?;

        // NXDOMAIN carries no answer, so the question section is the only way a
        // client can tell which of its outstanding queries the response is for.
        let query_type = match action.get("query_type").and_then(|v| v.as_str()) {
            Some(qt) => parse_record_type(qt)?,
            None => RecordType::A,
        };

        let (message, _name) = new_response(query_id, domain, query_type, ResponseCode::NXDomain)?;

        finish_response(message)
    }

    fn execute_send_dns_response(&self, action: serde_json::Value) -> Result<ActionResult> {
        let data = required_str(&action, "data")?;

        // Hex only. The previous behaviour fell back to sending the raw string
        // bytes when hex decoding failed, which silently put non-DNS bytes on
        // the wire and left the client timing out with no diagnostic.
        let bytes = hex::decode(data.trim()).map_err(|e| {
            anyhow::anyhow!(
                "'data' must be a hex-encoded DNS message ({e}). \
                 Prefer the structured actions (send_dns_a_response, send_dns_nxdomain, ...) \
                 which build the wire format for you."
            )
        })?;

        if bytes.len() < 12 {
            anyhow::bail!(
                "'data' decoded to {} bytes; a DNS message needs at least a 12-byte header",
                bytes.len()
            );
        }

        Ok(ActionResult::Output(bytes))
    }
}

fn send_dns_a_response_action() -> ActionDefinition {
    ActionDefinition {
        name: "send_dns_a_response".to_string(),
        description: "Answer a query whose query_type is A with an IPv4 address. Only for query_type A: a TXT query is answered with send_dns_txt_response, AAAA with send_dns_aaaa_response, MX with send_dns_mx_response. This is the correct DNS-specific action - do NOT use generic 'send_data' or 'show_message' actions for DNS responses. Always include the query_id from the request, the domain name, and the IPv4 address to return.".to_string(),
        parameters: vec![
            Parameter {
                name: "query_id".to_string(),
                type_hint: "number".to_string(),
                description: "DNS query ID from the request (MUST match the request)".to_string(),
                required: true,
            },
            Parameter {
                name: "domain".to_string(),
                type_hint: "string".to_string(),
                description: "Domain name being queried (e.g., 'example.com')".to_string(),
                required: true,
            },
            Parameter {
                name: "ip".to_string(),
                type_hint: "string".to_string(),
                description: "IPv4 address to return (e.g., '192.0.2.1' or '93.184.216.34')".to_string(),
                required: true,
            },
            Parameter {
                name: "ttl".to_string(),
                type_hint: "number".to_string(),
                description: "Time-to-live in seconds (how long clients should cache this response). Default: 300".to_string(),
                required: false,
            },
        ],
        example: json!({
            "type": "send_dns_a_response",
            "query_id": 12345,
            "domain": "example.com",
            "ip": "192.0.2.1",
            "ttl": 300
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("DNS A {domain} -> {ip}")
                .with_debug("DNS A response: {domain} -> {ip} (TTL={ttl})")
                .with_trace("DNS A response: {json_pretty(.)}"),
        ),
    }
}

fn send_dns_aaaa_response_action() -> ActionDefinition {
    ActionDefinition {
        name: "send_dns_aaaa_response".to_string(),
        description: "Send DNS AAAA record response (IPv6 address)".to_string(),
        parameters: vec![
            Parameter {
                name: "query_id".to_string(),
                type_hint: "number".to_string(),
                description: "DNS query ID from the request".to_string(),
                required: true,
            },
            Parameter {
                name: "domain".to_string(),
                type_hint: "string".to_string(),
                description: "Domain name being queried (e.g., 'example.com')".to_string(),
                required: true,
            },
            Parameter {
                name: "ip".to_string(),
                type_hint: "string".to_string(),
                description: "IPv6 address to return (e.g., '2001:db8::1')".to_string(),
                required: true,
            },
            Parameter {
                name: "ttl".to_string(),
                type_hint: "number".to_string(),
                description: "Time-to-live in seconds. Default: 300".to_string(),
                required: false,
            },
        ],
        example: json!({
            "type": "send_dns_aaaa_response",
            "query_id": 12345,
            "domain": "example.com",
            "ip": "2001:db8::1",
            "ttl": 300
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("DNS AAAA {domain} -> {ip}")
                .with_debug("DNS AAAA response: {domain} -> {ip} (TTL={ttl})"),
        ),
    }
}

fn send_dns_cname_response_action() -> ActionDefinition {
    ActionDefinition {
        name: "send_dns_cname_response".to_string(),
        description: "Send DNS CNAME record response (canonical name/alias)".to_string(),
        parameters: vec![
            Parameter {
                name: "query_id".to_string(),
                type_hint: "number".to_string(),
                description: "DNS query ID from the request".to_string(),
                required: true,
            },
            Parameter {
                name: "domain".to_string(),
                type_hint: "string".to_string(),
                description: "Domain name being queried (e.g., 'www.example.com')".to_string(),
                required: true,
            },
            Parameter {
                name: "target".to_string(),
                type_hint: "string".to_string(),
                description: "Target domain name (e.g., 'example.com')".to_string(),
                required: true,
            },
            Parameter {
                name: "query_type".to_string(),
                type_hint: "string".to_string(),
                description: "Record type the client actually asked for (A, AAAA, ...). Echoed back in the question section. Default: CNAME".to_string(),
                required: false,
            },
            Parameter {
                name: "ttl".to_string(),
                type_hint: "number".to_string(),
                description: "Time-to-live in seconds. Default: 300".to_string(),
                required: false,
            },
        ],
        example: json!({
            "type": "send_dns_cname_response",
            "query_id": 12345,
            "domain": "www.example.com",
            "target": "example.com",
            "ttl": 300
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("DNS CNAME {domain} -> {target}")
                .with_debug("DNS CNAME response: {domain} -> {target} (TTL={ttl})"),
        ),
    }
}

fn send_dns_mx_response_action() -> ActionDefinition {
    ActionDefinition {
        name: "send_dns_mx_response".to_string(),
        description: "Send DNS MX record response (mail exchange)".to_string(),
        parameters: vec![
            Parameter {
                name: "query_id".to_string(),
                type_hint: "number".to_string(),
                description: "DNS query ID from the request".to_string(),
                required: true,
            },
            Parameter {
                name: "domain".to_string(),
                type_hint: "string".to_string(),
                description: "Domain name being queried (e.g., 'example.com')".to_string(),
                required: true,
            },
            Parameter {
                name: "exchange".to_string(),
                type_hint: "string".to_string(),
                description: "Mail server domain (e.g., 'mail.example.com')".to_string(),
                required: true,
            },
            Parameter {
                name: "preference".to_string(),
                type_hint: "number".to_string(),
                description: "MX preference (priority, lower = higher priority). Default: 10"
                    .to_string(),
                required: false,
            },
            Parameter {
                name: "ttl".to_string(),
                type_hint: "number".to_string(),
                description: "Time-to-live in seconds. Default: 300".to_string(),
                required: false,
            },
        ],
        example: json!({
            "type": "send_dns_mx_response",
            "query_id": 12345,
            "domain": "example.com",
            "exchange": "mail.example.com",
            "preference": 10,
            "ttl": 300
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("DNS MX {domain} -> {exchange} (pref={preference})")
                .with_debug(
                    "DNS MX response: {domain} -> {exchange} (pref={preference}, TTL={ttl})",
                ),
        ),
    }
}

fn send_dns_txt_response_action() -> ActionDefinition {
    ActionDefinition {
        name: "send_dns_txt_response".to_string(),
        description: "Answer a query whose query_type is TXT with a text record - the only action for TXT queries. A TXT question asks for text, never for an address: when the instruction says to answer text queries with some text, this carries that text.".to_string(),
        parameters: vec![
            Parameter {
                name: "query_id".to_string(),
                type_hint: "number".to_string(),
                description: "DNS query ID from the request".to_string(),
                required: true,
            },
            Parameter {
                name: "domain".to_string(),
                type_hint: "string".to_string(),
                description: "Domain name being queried (e.g., 'example.com')".to_string(),
                required: true,
            },
            Parameter {
                name: "text".to_string(),
                type_hint: "string".to_string(),
                description: "Text data to return. At most 255 octets: it is emitted as a \
                     single DNS <character-string>, whose length is one octet (RFC 1035 \
                     §3.3). Longer text is refused rather than split."
                    .to_string(),
                required: true,
            },
            Parameter {
                name: "ttl".to_string(),
                type_hint: "number".to_string(),
                description: "Time-to-live in seconds. Default: 300".to_string(),
                required: false,
            },
        ],
        example: json!({
            "type": "send_dns_txt_response",
            "query_id": 12345,
            "domain": "example.com",
            "text": "v=spf1 include:_spf.example.com ~all",
            "ttl": 300
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("DNS TXT {domain}")
                .with_debug("DNS TXT response: {domain} -> \"{text}\" (TTL={ttl})"),
        ),
    }
}

fn send_dns_nxdomain_action() -> ActionDefinition {
    ActionDefinition {
        name: "send_dns_nxdomain".to_string(),
        description: "IMPORTANT: Use this action to respond when a DNS domain does not exist. This is the correct DNS-specific action for NXDOMAIN responses - do NOT use generic error actions. This tells the client that the requested domain name does not exist in the DNS system.".to_string(),
        parameters: vec![
            Parameter {
                name: "query_id".to_string(),
                type_hint: "number".to_string(),
                description: "DNS query ID from the request (MUST match the request)".to_string(),
                required: true,
            },
            Parameter {
                name: "domain".to_string(),
                type_hint: "string".to_string(),
                description: "Domain name being queried (the nonexistent domain)".to_string(),
                required: true,
            },
            Parameter {
                name: "query_type".to_string(),
                type_hint: "string".to_string(),
                description: "Record type from the request (A, AAAA, MX, TXT, ...). Echoed back in the question section so the client can match the response to its query. Default: A".to_string(),
                required: false,
            },
        ],
        example: json!({
            "type": "send_dns_nxdomain",
            "query_id": 12345,
            "domain": "nonexistent.example.com",
            "query_type": "A"
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("DNS NXDOMAIN {domain}")
                .with_debug("DNS NXDOMAIN response: {domain}"),
        ),
    }
}

fn send_dns_response_action() -> ActionDefinition {
    ActionDefinition {
        name: "send_dns_response".to_string(),
        description: "ESCAPE HATCH - only for record types that have no dedicated action (NS, SOA, PTR, SRV, CAA, ...). Prefer send_dns_a_response / send_dns_aaaa_response / send_dns_cname_response / send_dns_mx_response / send_dns_txt_response / send_dns_nxdomain, which build the wire format for you. This action requires you to hand-assemble the full DNS message, including echoing the query ID and the question section.".to_string(),
        parameters: vec![Parameter {
            name: "data".to_string(),
            type_hint: "string".to_string(),
            description: "Complete DNS response message in RFC 1035 wire format, hex-encoded (at least 12 bytes / 24 hex characters). Not base64, not plain text - invalid hex is rejected.".to_string(),
            required: true,
        }],
        // A complete RFC 1035 response: id=0x1234, flags 0x8180 (QR|RD|RA), one
        // question (example.com A IN) and one answer (compressed name pointer
        // 0xc00c, A IN, TTL 300, rdlength 4, 93.184.216.34). The previous example
        // was abbreviated with an ellipsis and omitted the 2-byte id, so it was
        // neither valid hex nor a DNS message.
        example: json!({
            "type": "send_dns_response",
            "data": "123481800001000100000000076578616d706c6503636f6d0000010001\
                     c00c000100010000012c00045db8d822"
        }),
        log_template: Some(
            LogTemplate::new()
                .with_info("DNS raw response {output_bytes}B")
                .with_debug("DNS send_dns_response: {output_bytes}B"),
        ),
    }
}

fn ignore_query_action() -> ActionDefinition {
    ActionDefinition {
        name: "ignore_query".to_string(),
        description: "Ignore this DNS query and don't send a response".to_string(),
        parameters: vec![],
        example: json!({
            "type": "ignore_query"
        }),
        log_template: Some(LogTemplate::new().with_debug("DNS ignore_query")),
    }
}

// ============================================================================
// DNS Event Type Constants
// ============================================================================

/// DNS query event - triggered when DNS client sends a query
pub static DNS_QUERY_EVENT: LazyLock<EventType> = LazyLock::new(|| {
    EventType::new(
        "dns_query",
        "DNS client sent a query for domain resolution",
        serde_json::json!({
            "type": "send_dns_a_response",
            "query_id": 12345,
            "domain": "example.com",
            "ip": "93.184.216.34",
            "ttl": 300
        }),
    )
    .with_parameters(vec![
        Parameter {
            name: "query_id".to_string(),
            type_hint: "number".to_string(),
            description: "DNS query ID from the request packet".to_string(),
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
            description: "DNS query type (A, AAAA, MX, TXT, CNAME, etc.). Decides the action: A -> send_dns_a_response, AAAA -> send_dns_aaaa_response, MX -> send_dns_mx_response, TXT -> send_dns_txt_response, CNAME -> send_dns_cname_response.".to_string(),
            required: true,
        },
    ])
    .with_actions(vec![
        send_dns_a_response_action(),
        send_dns_aaaa_response_action(),
        send_dns_cname_response_action(),
        send_dns_mx_response_action(),
        send_dns_txt_response_action(),
        send_dns_nxdomain_action(),
        send_dns_response_action(),
        ignore_query_action(),
    ])
    .with_alternative_example(serde_json::json!({
        "type": "send_dns_nxdomain",
        "query_id": 12345,
        "domain": "unknown.example.com"
    }))
    .with_alternative_example(serde_json::json!({
        "type": "send_dns_aaaa_response",
        "query_id": 12345,
        "domain": "example.com",
        "ip": "2606:2800:220:1:248:1893:25c8:1946",
        "ttl": 300
    }))
    .with_log_template(
        LogTemplate::new()
            .with_info("DNS {query_type} {domain} from {client_ip}")
            .with_debug("DNS query ID={query_id}, type={query_type}, domain={domain}")
            .with_trace("DNS query: {json_pretty(.)}"),
    )
});

/// Get DNS event types
pub fn get_dns_event_types() -> Vec<EventType> {
    vec![DNS_QUERY_EVENT.clone()]
}
