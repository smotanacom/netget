//! The pcap oracle's own test.
//!
//! A validator that always passes is worse than no validator: it converts "nobody
//! checked" into "something checked and was happy", which is how the BLE HID report
//! descriptors stayed green. So this file does the thing the oracle does to
//! protocols — it puts known-bad bytes in and asserts a rejection, and known-good
//! bytes in and asserts silence.
//!
//! Three failure *mechanisms* are covered, because they are genuinely different:
//!
//! 1. **Malformed** — the dissector runs off the end of the buffer and raises
//!    Expert Info at Error severity (truncated DNS, truncated LLDP).
//! 2. **Protocol-level complaint** — the dissector parses the bytes and objects to
//!    what they say (a CDP checksum that does not match).
//! 3. **Silent fallback** — the dissector never engages at all and tshark reports
//!    generic `data`, with *no* expert info whatsoever. An 802.3 frame carrying an
//!    EtherType where the length belongs is exactly this, and it is the shape of the
//!    CDP defect Programme 2 found by hand. Only the `frame.protocols` check sees it.

#[path = "helpers/pcap_oracle.rs"]
mod pcap_oracle;

use pcap_oracle::PcapOracle;

/// Hex literal → bytes. Spaces are ignored so the literals below can be grouped
/// by field, which is the only way a hand-written packet stays reviewable.
fn h(s: &str) -> Vec<u8> {
    let clean: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    assert!(clean.len().is_multiple_of(2), "odd-length hex literal");
    (0..clean.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&clean[i..i + 2], 16).expect("hex"))
        .collect()
}

/// A standards-correct DNS response: id 0x1234, QR|RD|RA, one question
/// (`example.com` IN A) and one answer (A 93.184.216.34, TTL 300).
fn dns_query() -> Vec<u8> {
    h("1234 0100 0001 0000 0000 0000
       07 6578616d706c65 03 636f6d 00 0001 0001")
}

fn dns_reply() -> Vec<u8> {
    h("1234 8180 0001 0001 0000 0000
       07 6578616d706c65 03 636f6d 00 0001 0001
       c00c 0001 0001 0000012c 0004 5db8d822")
}

#[test]
fn tshark_is_present() {
    // Not an assertion about the oracle — an assertion about the machine. If this
    // fails, every other test in this file is meaningless, and it should say so
    // once, loudly, rather than 20 times obscurely.
    pcap_oracle::require_tshark();
}

#[test]
fn a_well_formed_dns_exchange_passes() {
    PcapOracle::udp("dns")
        .to_server(&dns_query())
        .from_server(&dns_reply())
        .assert_clean();
}

#[test]
fn a_truncated_dns_reply_is_rejected() {
    // The reply cut off inside the question section: the header still claims one
    // question and one answer, so the dissector reads past the end.
    let truncated = dns_reply()[..20].to_vec();
    let report = PcapOracle::udp("dns")
        .from_server(&truncated)
        .check()
        .expect("tshark must run");
    assert!(
        !report.is_clean(),
        "a DNS reply truncated mid-question must be rejected; tshark said: {report:#?}"
    );
    assert!(
        report
            .failures
            .iter()
            .any(|f| f.contains("Malformed") || f.contains("Error")),
        "the failure must name the malformation, got: {:?}",
        report.failures
    );
}

#[test]
fn a_dns_reply_with_a_lying_answer_count_is_rejected() {
    // ANCOUNT says 4; one answer follows. This is the class the oracle exists for:
    // the bytes are individually plausible and only a decoder that has read the RFC
    // notices the header contradicts the body.
    let mut bad = dns_reply();
    bad[7] = 4;
    let report = PcapOracle::udp("dns")
        .from_server(&bad)
        .check()
        .expect("tshark must run");
    assert!(
        !report.is_clean(),
        "a DNS reply whose ANCOUNT exceeds its answer section must be rejected; \
         tshark said: {report:#?}"
    );
}

#[test]
fn a_well_formed_http_exchange_passes() {
    PcapOracle::tcp("http")
        .to_server(b"GET /hello HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .from_server(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 5\r\n\r\nhello",
        )
        .assert_clean();
}

#[test]
fn an_http_response_shorter_than_its_content_length_is_rejected() {
    // 500 declared, 5 delivered. The http dissector never completes the message, so
    // the server's packet is reported as plain `tcp` — no expert info at all. This
    // is caught only by the "did the dissector engage" half of the oracle.
    let report = PcapOracle::tcp("http")
        .to_server(b"GET /hello HTTP/1.1\r\nHost: localhost\r\n\r\n")
        .from_server(
            b"HTTP/1.1 200 OK\r\nContent-Type: text/plain\r\nContent-Length: 500\r\n\r\nhello",
        )
        .check()
        .expect("tshark must run");
    assert!(
        !report.is_clean(),
        "a response body shorter than its declared Content-Length must be rejected; \
         tshark said: {report:#?}"
    );
    assert!(
        report.failures.iter().any(|f| f.contains("server→client")),
        "the failure must name the direction that was wrong, got: {:?}",
        report.failures
    );
}

// ---------------------------------------------------------------------------
// Link layer
// ---------------------------------------------------------------------------

fn lldp_tlv(t: u8, v: &[u8]) -> Vec<u8> {
    let head = ((u16::from(t) << 9) | v.len() as u16).to_be_bytes();
    let mut out = head.to_vec();
    out.extend_from_slice(v);
    out
}

fn lldp_frame() -> Vec<u8> {
    let mut body = Vec::new();
    body.extend(lldp_tlv(1, &h("04 020000000001"))); // chassis id, MAC subtype
    body.extend(lldp_tlv(2, b"\x05eth0")); // port id, interface name subtype
    body.extend(lldp_tlv(3, &h("0078"))); // TTL 120
    body.extend(lldp_tlv(0, &[])); // End of LLDPDU
    let mut f = h("0180c200000e 020000000001 88cc");
    f.extend(body);
    f
}

#[test]
fn a_well_formed_lldp_frame_passes() {
    PcapOracle::ethernet("lldp")
        .frame(&lldp_frame())
        .assert_clean();
}

#[test]
fn a_truncated_lldp_frame_is_rejected() {
    let f = lldp_frame();
    let report = PcapOracle::ethernet("lldp")
        .frame(&f[..20])
        .check()
        .expect("tshark must run");
    assert!(
        !report.is_clean(),
        "an LLDPDU cut off inside a TLV must be rejected; tshark said: {report:#?}"
    );
}

/// The 16-bit one's-complement checksum CDP shares with IP. Computed here rather
/// than pasted as a literal so the frame below stays correct when it is edited —
/// a hardcoded checksum is the `Example drifts from const` defect in miniature.
fn ones_complement(bytes: &[u8]) -> u16 {
    let mut sum: u32 = 0;
    let mut i = 0;
    while i + 1 < bytes.len() {
        sum += u32::from(u16::from_be_bytes([bytes[i], bytes[i + 1]]));
        i += 2;
    }
    if i < bytes.len() {
        sum += u32::from(u16::from_be_bytes([bytes[i], 0]));
    }
    while sum >> 16 != 0 {
        sum = (sum & 0xffff) + (sum >> 16);
    }
    !(sum as u16)
}

fn cdp_payload() -> Vec<u8> {
    let mut payload = h("02 b4 0000"); // version 2, TTL 180, checksum placeholder
    payload.extend(h("0001 000a")); // Device ID TLV, length 10
    payload.extend(b"netget");
    let ck = ones_complement(&payload);
    payload[2..4].copy_from_slice(&ck.to_be_bytes());
    payload
}

/// An 802.3 frame whose length field holds an EtherType instead of a length —
/// the exact shape Programme 2 found in CDP by reading bytes by hand. tshark
/// raises **no expert info**; it simply hands the payload to the generic `data`
/// dissector. Without the `frame.protocols` check the oracle would pass this.
#[test]
fn a_cdp_frame_with_an_ethertype_where_the_length_belongs_is_rejected() {
    let payload = cdp_payload();

    let mut llc = h("aaaa03 00000c 2000"); // SNAP, Cisco OUI, CDP PID
    llc.extend_from_slice(&payload);

    // Correct: an 802.3 length field.
    let mut good = h("01000ccccccc 020000000001");
    good.extend_from_slice(&(llc.len() as u16).to_be_bytes());
    good.extend_from_slice(&llc);

    // Wrong: 0x2000 — a value above 0x0600, so Ethernet II parsing kicks in.
    let mut bad = h("01000ccccccc 020000000001 2000");
    bad.extend_from_slice(&llc);

    let bad_report = PcapOracle::ethernet("cdp")
        .frame(&bad)
        .check()
        .expect("tshark must run");
    assert!(
        !bad_report.is_clean(),
        "an EtherType in the 802.3 length field must be rejected; tshark said: {bad_report:#?}"
    );
    assert!(
        bad_report.failures.iter().all(|f| !f.contains("Expert")),
        "this defect raises no expert info — the rejection must come from the \
         dissector-fallback check, got: {:?}",
        bad_report.failures
    );

    // And the same payload framed correctly must be accepted, or the test above
    // proves only that the oracle rejects everything.
    let good_report = PcapOracle::ethernet("cdp")
        .frame(&good)
        .check()
        .expect("tshark must run");
    assert!(
        good_report.is_clean(),
        "the same CDP payload with a correct 802.3 length must pass: {good_report:#?}"
    );
}

#[test]
fn a_cdp_frame_with_a_wrong_checksum_is_rejected() {
    // The middle mechanism: the dissector engages, parses, and objects.
    let mut payload = h("02 b4 dead"); // checksum that is not the real one
    payload.extend(h("0001 000a"));
    payload.extend(b"netget");
    let mut llc = h("aaaa03 00000c 2000");
    llc.extend_from_slice(&payload);
    let mut f = h("01000ccccccc 020000000001");
    f.extend_from_slice(&(llc.len() as u16).to_be_bytes());
    f.extend_from_slice(&llc);

    let report = PcapOracle::ethernet("cdp")
        .frame(&f)
        .check()
        .expect("tshark must run");
    assert!(
        !report.is_clean(),
        "a wrong CDP checksum must be rejected; tshark said: {report:#?}"
    );
    assert!(
        report.failures.iter().any(|f| f.contains("checksum")),
        "the failure must name the checksum, got: {:?}",
        report.failures
    );
}

// ---------------------------------------------------------------------------
// The oracle's own moving parts
// ---------------------------------------------------------------------------

#[test]
fn the_synthetic_tcp_framing_raises_nothing_by_itself() {
    // If the fabricated handshake, sequence numbers, checksums or FIN exchange
    // produced Warn-severity expert info of their own, every adopting suite would
    // fail for reasons that have nothing to do with its protocol. This asserts the
    // scaffolding is silent, using a protocol with no dissector so that *only* the
    // scaffolding is under test.
    let report = PcapOracle::tcp("tcp")
        .to_server(b"hello")
        .from_server(b"world")
        .check()
        .expect("tshark must run");
    assert!(
        report.is_clean(),
        "the synthetic TCP framing must be silent on its own: {report:#?}"
    );
    assert!(
        report.packets.len() >= 5,
        "expected a handshake, two data segments and a teardown, got {} packets",
        report.packets.len()
    );
}

#[test]
fn a_direction_that_carried_nothing_is_not_required_to_dissect() {
    // A server-only capture is the common case (a banner, a broadcast, a
    // notification). Requiring the absent client direction to dissect would fail
    // every one of them.
    PcapOracle::tcp("imap")
        .from_server(b"* OK [CAPABILITY IMAP4rev1] netget ready\r\n")
        .assert_clean();
}

#[test]
fn an_allowed_expert_message_is_demoted() {
    let bad = dns_reply()[..20].to_vec();
    let report = PcapOracle::udp("dns")
        .from_server(&bad)
        .allow_expert_containing("Malformed Packet")
        .check()
        .expect("tshark must run");
    // The malformation is forgiven, but the dissector-fallback check is a separate
    // mechanism and is unaffected — which is the point of having two.
    assert!(
        !report.failures.iter().any(|f| f.contains("Expert Info")),
        "an allowed message must not appear as a failure: {:?}",
        report.failures
    );
}

#[test]
fn peer_input_is_context_forgives_the_request_and_still_judges_the_reply() {
    // The escape hatch for tunnelling protocols, where tshark recurses into a payload
    // the *test* invented. It must forgive the request and change nothing about the
    // reply, or it is a way to switch the oracle off one call site at a time.
    let clean = PcapOracle::udp("dns")
        .peer_input_is_context()
        .to_server(&dns_query()[..6]) // a truncated query: judged, this would fail
        .from_server(&dns_reply())
        .check()
        .expect("tshark must run");
    assert!(
        clean.is_clean(),
        "a malformed request must be forgiven when it is declared context: {clean:#?}"
    );

    let still_fails = PcapOracle::udp("dns")
        .peer_input_is_context()
        .to_server(&dns_query())
        .from_server(&dns_reply()[..20])
        .check()
        .expect("tshark must run");
    assert!(
        !still_fails.is_clean(),
        "the reply is what the oracle is for and must still be judged: {still_fails:#?}"
    );
}

#[test]
fn framing_a_protocol_the_wrong_way_is_refused_rather_than_answered() {
    // `PcapOracle::tcp("ntp")` would build a TCP stream, tshark would decline to apply
    // the NTP dissector to it, and the oracle would report a framing defect in a
    // server that has none. That is worse than no check, so it is an error rather
    // than a verdict.
    let err = PcapOracle::tcp("ntp")
        .from_server(&[0u8; 48])
        .check()
        .expect_err("framing NTP as TCP must be refused");
    assert!(
        err.contains("wireshark.rs"),
        "the refusal must point at the table that decides this, got: {err}"
    );
}

#[test]
fn the_hexdump_is_readable() {
    let d = pcap_oracle::hexdump(b"AB\x00\xffhello", 2);
    assert!(d.contains("41 42 00 ff"), "bytes must be in hex: {d}");
    assert!(d.contains("|AB..hello|"), "ascii column must render: {d}");
}
