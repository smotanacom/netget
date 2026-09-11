//! The VRRP/CARP codec against literal specification bytes — in both directions.
//!
//! # Why literals and not a round trip
//!
//! Encoding with our encoder and decoding with our decoder proves only that the two agree
//! with each other. The root `CLAUDE.md` names that as circular evidence, and it is exactly
//! the mistake that held `rss` at Experimental while its test round-tripped one crate through
//! itself. There is no third-party VRRP or CARP codec in this tree and no peer that can be
//! run here — the raw transport needs root — so the external reference is **the
//! specification's field table, written out by hand as octets, with every checksum computed
//! by hand**.
//!
//! ## Provenance, stated plainly
//!
//! Every byte string below was assembled from RFC 3768 §5.1 (VRRPv2), RFC 5798 §5.1
//! (VRRPv3) and OpenBSD's `sys/netinet/ip_carp.h` (`struct carp_header`). Each checksum in
//! the comments is worked through as a one's-complement sum of the 16-bit words, so an
//! encoder that gets the offsets, the byte order, the interval scaling or the pseudo-header
//! wrong disagrees with this file rather than with itself. They are **not** taken from a
//! named packet capture and this file does not claim they are. If you can get a capture,
//! replacing these is a strict improvement — say where they came from when you do.
//!
//! ## The hash vectors changed job, and were kept
//!
//! SHA-1 is the `sha1` crate's; `codec::sha1` is a thin adapter over it, and the RFC 2104
//! HMAC construction on top is ours only because no HMAC crate is reachable from this
//! feature. So the FIPS 180 / RFC 3174 and RFC 2202 vectors here no longer assert that SHA-1
//! is *correct* — that is the crate's problem. They assert that this code **drives it
//! correctly**: an adapter that hashed the wrong buffer, dropped an `update`, truncated the
//! digest or mis-ordered the HMAC key padding would pass every other test in this file and
//! fail those. The 0..=130 padding sweep is kept for the same reason, with its oracle changed
//! from "the crate" (now tautological) to "the crate's streaming API", which is a genuinely
//! different code path through it.
//!
//! ## The three things implementations get wrong
//!
//! 1. **Seconds versus centiseconds.** One second is `0x01` in VRRPv2 and `0x0064` in
//!    VRRPv3. Pinned at the exact offsets, and asserted to *differ* from the naive encoding.
//! 2. **The VRRPv3 checksum covers an IP pseudo-header** (RFC 5798 §5.2.8) and the VRRPv2 one
//!    does not (RFC 3768 §5.3.8). The same body addressed elsewhere is different octets.
//! 3. **CARP is not VRRP, and the first octet does not tell them apart** — both are `0x21`.
//!    There is a test below that decodes a real CARP advertisement as a VRRPv2 one and shows
//!    what you get: a router apparently resigning with seven virtual addresses.
//!
//! Run with:
//!   ./cargo-isolated.sh test --no-default-features --features vrrp \
//!       --test server -- vrrp::codec --test-threads=100

use std::net::Ipv4Addr;

use netget::server::vrrp::codec::{
    self, Advertisement, CarpAdvertisement, PseudoHeader, Variant, VrrpAdvertisement,
    CARP_AUTH_LEN_WORDS, CARP_HEADER_LEN, CARP_VERSION, IP_PROTOCOL_VRRP, VRRP_MULTICAST_IPV4,
    VRRP_PRIORITY_ADDRESS_OWNER, VRRP_PRIORITY_RESIGN, VRRP_VERSION_2, VRRP_VERSION_3,
};

// ---------------------------------------------------------------------------
// Literal packets
// ---------------------------------------------------------------------------

/// A complete VRRPv2 advertisement (RFC 3768 §5.1).
///
/// ```text
///  0      21     version 2, type 1 (advertisement) — packed high nibble / low nibble
///  1      01     Virtual Rtr ID 1
///  2      64     Priority 100 (RFC 3768 §5.3.4's default for a non-owner)
///  3      01     Count IP Addrs 1
///  4      00     Auth Type 0 — RFC 3768 defines no method
///  5      01     Adver Int 1 SECOND (one octet of whole seconds)
///  6.. 8  b9 52  Checksum over the whole 20-octet message
///  8..12  c0a80101   IP Address (1) = 192.168.1.1
/// 12..20  00 x 8     Authentication Data (1) and (2), zeroed but PRESENT and checksummed
/// ```
///
/// Checksum, by hand, over the message with the checksum field zeroed:
/// `0x2101 + 0x6401 + 0x0001 + 0x0000 + 0xc0a8 + 0x0101 + 0 + 0 + 0 + 0 = 0x1_46AC`,
/// folded `0x46AC + 1 = 0x46AD`, complemented `0xB952`.
const VRRP_V2_ADVERTISEMENT: [u8; 20] = [
    0x21, 0x01, 0x64, 0x01, 0x00, 0x01, 0xb9, 0x52, 0xc0, 0xa8, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00,
];

/// The same group, resigning: priority 0 (RFC 3768 §6.4.3).
///
/// Checksum: `0x2101 + 0x0001 + 0x0001 + 0xc0a8 + 0x0101 = 0xE2AC`, complemented `0x1D53`.
const VRRP_V2_RESIGNATION: [u8; 20] = [
    0x21, 0x01, 0x00, 0x01, 0x00, 0x01, 0x1d, 0x53, 0xc0, 0xa8, 0x01, 0x01, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00,
];

/// A complete VRRPv3 advertisement (RFC 5798 §5.1), sent from 192.168.1.2 to 224.0.0.18.
///
/// ```text
///  0      31     version 3, type 1
///  1      01     Virtual Rtr ID 1
///  2      64     Priority 100
///  3      01     Count IPvX Addr 1
///  4      00     four RESERVED bits (v2's Auth Type is gone) + high nibble of the interval
///  5      64     low octet of the interval — 0x064 = 100 CENTISECONDS = 1 second
///  6.. 8  06 b6  Checksum over the pseudo-header AND the message
///  8..12  c0a80101   IPv4 Address (1) = 192.168.1.1
/// ```
///
/// No trailing authentication data — RFC 5798 removed it.
///
/// Checksum, by hand. The RFC 5798 §5.2.8 pseudo-header is
/// `c0 a8 01 02 | e0 00 00 12 | 00 | 70 | 00 0c` (source, destination, zero, protocol 112,
/// message length 12), summing to `0x1_A238`. The message with the checksum zeroed sums to
/// `0x1_570F`. Together `0x2_F947`, folded `0xF947 + 2 = 0xF949`, complemented `0x06B6`.
const VRRP_V3_ADVERTISEMENT: [u8; 12] = [
    0x31, 0x01, 0x64, 0x01, 0x00, 0x64, 0x06, 0xb6, 0xc0, 0xa8, 0x01, 0x01,
];

const V3_SOURCE: Ipv4Addr = Ipv4Addr::new(192, 168, 1, 2);

/// A complete CARP advertisement (OpenBSD `struct carp_header`), with no HMAC key configured.
///
/// ```text
///  0      21     carp_version 2, carp_type 1 — THE SAME FIRST OCTET AS VRRPv2
///  1      01     carp_vhid 1
///  2      00     carp_advskew 0 (lower wins — the inverse of VRRP priority)
///  3      07     carp_authlen: (8-octet counter + 20-octet HMAC) / 4
///  4      00     carp_demote 0
///  5      01     carp_advbase 1 second
///  6.. 8  de f6  carp_cksum over these 36 octets
///  8..16  00 x 8     carp_counter
/// 16..36  00 x 20    carp_md (SHA-1 HMAC)
/// ```
///
/// Checksum: only three non-zero words, `0x2101 + 0x0007 + 0x0001 = 0x2109`, complemented
/// `0xDEF6`.
const CARP_ADVERTISEMENT: [u8; 36] = [
    0x21, 0x01, 0x00, 0x07, 0x00, 0x01, 0xde, 0xf6, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00,
];

fn v2_advertisement() -> VrrpAdvertisement {
    VrrpAdvertisement {
        version: VRRP_VERSION_2,
        vrid: 1,
        priority: 100,
        advert_interval_seconds: 1.0,
        auth_type: 0,
        addresses: vec![Ipv4Addr::new(192, 168, 1, 1)],
        checksum: 0,
    }
}

fn v3_advertisement() -> VrrpAdvertisement {
    VrrpAdvertisement {
        version: VRRP_VERSION_3,
        ..v2_advertisement()
    }
}

fn v3_pseudo() -> PseudoHeader {
    PseudoHeader::new(V3_SOURCE, VRRP_MULTICAST_IPV4)
}

// ---------------------------------------------------------------------------
// VRRPv2
// ---------------------------------------------------------------------------

#[test]
fn a_vrrpv2_advertisement_encodes_to_the_specification_bytes() {
    let encoded = v2_advertisement().encode(None).expect("encode v2");
    assert_eq!(
        encoded, VRRP_V2_ADVERTISEMENT,
        "the VRRPv2 encoding must match RFC 3768 §5.1 octet for octet, including the two \
         zeroed authentication-data words the RFC keeps at the end"
    );
    assert_eq!(
        encoded.len(),
        20,
        "8 header + 4 address + 8 authentication data"
    );
}

#[test]
fn a_vrrpv2_advertisement_decodes_from_the_specification_bytes() {
    let decoded = VrrpAdvertisement::decode(&VRRP_V2_ADVERTISEMENT).expect("decode v2");
    assert_eq!(decoded.version, 2);
    assert_eq!(decoded.vrid, 1);
    assert_eq!(decoded.priority, 100);
    assert_eq!(decoded.auth_type, 0);
    assert_eq!(
        decoded.advert_interval_seconds, 1.0,
        "the v2 interval octet is whole seconds"
    );
    assert_eq!(decoded.addresses, vec![Ipv4Addr::new(192, 168, 1, 1)]);
    assert_eq!(decoded.checksum, 0xb952);
    assert!(!decoded.is_resignation());
    assert!(!decoded.is_address_owner());
}

#[test]
fn priority_zero_is_a_resignation_and_255_is_the_address_owner() {
    let resigning = VrrpAdvertisement::decode(&VRRP_V2_RESIGNATION).expect("decode");
    assert_eq!(resigning.priority, VRRP_PRIORITY_RESIGN);
    assert!(
        resigning.is_resignation(),
        "priority 0 exists so backups take over IMMEDIATELY instead of waiting out their \
         master-down interval (RFC 3768 §6.4.3); treating it as just a low priority loses \
         the whole point of the value"
    );
    assert_eq!(
        resigning.encode(None).expect("re-encode"),
        VRRP_V2_RESIGNATION,
        "a resignation is an ordinary advertisement with priority 0 — checksum included"
    );

    let owner = VrrpAdvertisement {
        priority: VRRP_PRIORITY_ADDRESS_OWNER,
        ..v2_advertisement()
    };
    assert!(owner.is_address_owner());
    assert!(!owner.is_resignation());
}

// ---------------------------------------------------------------------------
// VRRPv3
// ---------------------------------------------------------------------------

#[test]
fn a_vrrpv3_advertisement_encodes_to_the_specification_bytes() {
    let encoded = v3_advertisement()
        .encode(Some(&v3_pseudo()))
        .expect("encode v3");
    assert_eq!(
        encoded, VRRP_V3_ADVERTISEMENT,
        "the VRRPv3 encoding must match RFC 5798 §5.1 octet for octet"
    );
    assert_eq!(
        encoded.len(),
        12,
        "8 header + 4 address, and NO trailing authentication data — RFC 5798 removed it"
    );
}

#[test]
fn a_vrrpv3_advertisement_decodes_from_the_specification_bytes() {
    let decoded = VrrpAdvertisement::decode(&VRRP_V3_ADVERTISEMENT).expect("decode v3");
    assert_eq!(decoded.version, 3);
    assert_eq!(decoded.vrid, 1);
    assert_eq!(decoded.priority, 100);
    assert_eq!(
        decoded.advert_interval_seconds, 1.0,
        "0x0064 is 100 centiseconds, which is one second — a decoder that reports 100 here \
         has skipped the conversion"
    );
    assert_eq!(decoded.addresses, vec![Ipv4Addr::new(192, 168, 1, 1)]);
    assert_eq!(decoded.checksum, 0x06b6);
}

/// **The seconds-versus-centiseconds trap**, pinned at the exact wire offsets.
#[test]
fn the_advertisement_interval_is_seconds_in_v2_and_centiseconds_in_v3() {
    let v2 = v2_advertisement().encode(None).expect("v2");
    let v3 = v3_advertisement().encode(Some(&v3_pseudo())).expect("v3");

    assert_eq!(
        v2[5], 0x01,
        "VRRPv2 puts WHOLE SECONDS in octet 5 (RFC 3768 §5.3.7)"
    );
    assert_eq!(
        &v3[4..6],
        &[0x00, 0x64],
        "VRRPv3 puts CENTISECONDS in the low 12 bits of octets 4-5 (RFC 5798 §5.2.7), so one \
         second is 100"
    );
    // The negative half is what makes a failure legible: without it a regression reports
    // `1 != 100` and the reader has to work out which side is wrong.
    assert_ne!(
        &v3[4..6],
        &[0x00, 0x01],
        "writing the number of seconds straight into the v3 field produces an advertisement \
         claiming a 10-millisecond interval, and a real peer sizes its master-down interval \
         from it"
    );

    // The top four bits of octet 4 are reserved in v3 and must stay clear.
    assert_eq!(v3[4] & 0xf0, 0x00, "RFC 5798 §5.2.5: (rsvd) must be zero");
    // In v2 the same octet is the authentication type, a different field entirely.
    assert_eq!(v2[4], 0x00, "RFC 3768 §5.3.5: Auth Type 0");
}

#[test]
fn the_v3_interval_field_is_twelve_bits_and_refuses_what_it_cannot_hold() {
    // 40.95 s is 4095 centiseconds — every bit of the field set.
    let max = VrrpAdvertisement {
        advert_interval_seconds: 40.95,
        ..v3_advertisement()
    };
    let encoded = max.encode(Some(&v3_pseudo())).expect("40.95s fits");
    assert_eq!(&encoded[4..6], &[0x0f, 0xff]);

    // 40.96 s is 4096, which needs a thirteenth bit. It must be refused, not wrapped to 0.
    let over = VrrpAdvertisement {
        advert_interval_seconds: 40.96,
        ..v3_advertisement()
    };
    let error = format!(
        "{:#}",
        over.encode(Some(&v3_pseudo()))
            .expect_err("40.96s does not fit a 12-bit centisecond field")
    );
    assert!(
        error.contains("40.95") && error.contains("CENTISECONDS"),
        "the refusal must explain the unit and the ceiling, got: {error}"
    );
}

#[test]
fn vrrpv2_cannot_express_a_sub_second_interval_and_says_so() {
    let half = VrrpAdvertisement {
        advert_interval_seconds: 0.5,
        ..v2_advertisement()
    };
    let error = format!("{:#}", half.encode(None).expect_err("0.5s is not whole"));
    assert!(
        error.contains("WHOLE SECONDS") && error.contains("version 3"),
        "a refusal that does not point at the version which CAN express it trains people to \
         round the number instead, got: {error}"
    );
}

// ---------------------------------------------------------------------------
// Checksums
// ---------------------------------------------------------------------------

#[test]
fn the_checksum_is_the_rfc_1071_ones_complement_sum() {
    // The defining property a receiver relies on: summing a correct packet, checksum
    // included, yields zero.
    assert_eq!(codec::internet_checksum(&VRRP_V2_ADVERTISEMENT), 0);
    assert_eq!(codec::internet_checksum(&CARP_ADVERTISEMENT), 0);
    assert!(codec::checksum_is_valid(&VRRP_V2_ADVERTISEMENT, None));

    // Corrupt one octet and it must stop validating.
    let mut corrupt = VRRP_V2_ADVERTISEMENT;
    corrupt[2] ^= 0x01;
    assert!(
        !codec::checksum_is_valid(&corrupt, None),
        "a single flipped priority bit must fail the checksum"
    );

    // An odd-length buffer pads with a zero octet rather than dropping the last one.
    assert_ne!(
        codec::internet_checksum(&[0xff]),
        codec::internet_checksum(&[])
    );
}

/// **The VRRPv3 checksum depends on where the packet is going**, because RFC 5798 §5.2.8
/// folds an IP pseudo-header in. VRRPv2's does not.
#[test]
fn the_v3_checksum_covers_the_pseudo_header_and_the_v2_checksum_does_not() {
    let advertisement = v3_advertisement();

    let to_multicast = advertisement.encode(Some(&v3_pseudo())).expect("multicast");
    let to_unicast = advertisement
        .encode(Some(&PseudoHeader::new(
            V3_SOURCE,
            Ipv4Addr::new(192, 168, 1, 9),
        )))
        .expect("unicast");

    assert_eq!(
        to_multicast[..6],
        to_unicast[..6],
        "everything before the checksum is identical"
    );
    assert_ne!(
        to_multicast[6..8],
        to_unicast[6..8],
        "the destination address is part of the v3 checksum, so the same body addressed \
         elsewhere is different octets"
    );
    assert!(codec::checksum_is_valid(&to_multicast, Some(&v3_pseudo())));
    assert!(
        !codec::checksum_is_valid(&to_multicast, None),
        "validating a v3 packet without its pseudo-header must fail rather than pass by \
         accident — that is what a v2-only checksum routine would do to it"
    );

    // VRRPv2 ignores the pseudo-header entirely: the checksum is the same either way.
    let v2 = v2_advertisement();
    assert_eq!(
        v2.encode(None).expect("v2"),
        v2.encode(Some(&v3_pseudo()))
            .expect("v2 with a pseudo-header"),
        "RFC 3768 §5.3.8 sums the message alone; supplying a pseudo-header must change nothing"
    );

    // And a v3 encode with no pseudo-header refuses rather than silently producing a packet
    // every conformant peer discards.
    let error = format!(
        "{:#}",
        advertisement
            .encode(None)
            .expect_err("v3 needs the addresses")
    );
    assert!(
        error.contains("pseudo-header") && error.contains("5798"),
        "the refusal must name what is missing and why, got: {error}"
    );
}

#[test]
fn the_pseudo_header_is_the_twelve_octets_rfc_5798_describes() {
    let bytes = v3_pseudo().bytes(12).expect("pseudo-header");
    assert_eq!(
        bytes,
        [0xc0, 0xa8, 0x01, 0x02, 0xe0, 0x00, 0x00, 0x12, 0x00, 0x70, 0x00, 0x0c],
        "source | destination | zero | protocol 112 | VRRP message length"
    );
    assert_eq!(bytes[9], IP_PROTOCOL_VRRP);
    assert_eq!(VRRP_MULTICAST_IPV4, Ipv4Addr::new(224, 0, 0, 18));
}

// ---------------------------------------------------------------------------
// Malformed input
// ---------------------------------------------------------------------------

#[test]
fn truncated_and_malformed_messages_are_refused_rather_than_read_past() {
    assert!(VrrpAdvertisement::decode(&VRRP_V2_ADVERTISEMENT[..7]).is_err());

    // Declares one address but carries none.
    let short = [0x21, 0x01, 0x64, 0x01, 0x00, 0x01, 0x00, 0x00];
    let error = format!("{:#}", VrrpAdvertisement::decode(&short).unwrap_err());
    assert!(error.contains("declares 1 address"), "got: {error}");

    // Type 2 does not exist; VRRP has exactly one packet type.
    let bad_type = [0x22, 0x01, 0x64, 0x00, 0x00, 0x01, 0x00, 0x00];
    assert!(VrrpAdvertisement::decode(&bad_type).is_err());

    // Version 1 is not something this codec claims to speak.
    let bad_version = [0x11, 0x01, 0x64, 0x00, 0x00, 0x01, 0x00, 0x00];
    assert!(VrrpAdvertisement::decode(&bad_version).is_err());

    // VRID 0 cannot be put on the wire.
    let zero_vrid = VrrpAdvertisement {
        vrid: 0,
        ..v2_advertisement()
    };
    assert!(zero_vrid.encode(None).is_err());

    // A CARP header shorter than 36 octets is not a CARP header.
    assert!(CarpAdvertisement::decode(&CARP_ADVERTISEMENT[..35]).is_err());
}

// ---------------------------------------------------------------------------
// CARP
// ---------------------------------------------------------------------------

#[test]
fn a_carp_advertisement_encodes_to_the_openbsd_layout() {
    let advertisement = CarpAdvertisement {
        version: CARP_VERSION,
        vhid: 1,
        advskew: 0,
        auth_length_words: CARP_AUTH_LEN_WORDS,
        demote: 0,
        advbase: 1,
        counter: 0,
        hmac: [0u8; 20],
        checksum: 0,
    };
    let encoded = advertisement.encode().expect("encode carp");
    assert_eq!(encoded.len(), CARP_HEADER_LEN);
    assert_eq!(
        encoded, CARP_ADVERTISEMENT,
        "the CARP encoding must match struct carp_header octet for octet"
    );
}

#[test]
fn a_carp_advertisement_decodes_from_the_openbsd_layout() {
    let decoded = CarpAdvertisement::decode(&CARP_ADVERTISEMENT).expect("decode carp");
    assert_eq!(decoded.version, CARP_VERSION);
    assert_eq!(decoded.vhid, 1);
    assert_eq!(decoded.advskew, 0);
    assert_eq!(decoded.auth_length_words, CARP_AUTH_LEN_WORDS);
    assert_eq!(decoded.demote, 0);
    assert_eq!(decoded.advbase, 1);
    assert_eq!(decoded.counter, 0);
    assert_eq!(decoded.hmac, [0u8; 20]);
    assert_eq!(decoded.checksum, 0xdef6);
}

#[test]
fn the_carp_interval_is_advbase_plus_advskew_over_256() {
    let mut advertisement = CarpAdvertisement::decode(&CARP_ADVERTISEMENT).expect("decode");
    assert_eq!(advertisement.interval_seconds(), 1.0);

    advertisement.advskew = 128;
    assert_eq!(
        advertisement.interval_seconds(),
        1.5,
        "advskew is a 1/256-second fraction, and because the earliest advertiser wins, a \
         LOWER skew is the stronger claim — the opposite direction from VRRP priority"
    );

    // The counter really is 64 bits, big-endian, across octets 8..16.
    let with_counter = CarpAdvertisement {
        counter: 0x0102_0304_0506_0708,
        ..advertisement
    };
    let encoded = with_counter.encode().expect("encode");
    assert_eq!(&encoded[8..16], &[1, 2, 3, 4, 5, 6, 7, 8]);
    assert_eq!(
        CarpAdvertisement::decode(&encoded).expect("decode").counter,
        0x0102_0304_0506_0708
    );
}

/// **The first octet does not distinguish CARP from VRRPv2**, and this is what happens when
/// you let it.
///
/// A dispatcher that switches on `data[0]` reads the real CARP advertisement above as a
/// VRRPv2 advertisement — and not as an obviously broken one. CARP's `authlen` (7) lands on
/// VRRP's count-IP-addresses octet, and 8 + 7*4 is exactly 36, so the length check passes.
/// CARP's `advskew` (0) lands on VRRP's priority octet, so the phantom router appears to be
/// **resigning**, which is the one advertisement that provokes an immediate election.
///
/// That is why the server takes a `variant` startup parameter instead of sniffing.
#[test]
fn a_carp_advertisement_misdecodes_as_a_vrrpv2_resignation() {
    assert_eq!(
        CARP_ADVERTISEMENT[0], VRRP_V2_ADVERTISEMENT[0],
        "CARP_VERSION 2 / CARP_ADVERTISEMENT 1 packs to 0x21, exactly as VRRP version 2 / \
         type 1 does"
    );

    let misread = VrrpAdvertisement::decode(&CARP_ADVERTISEMENT)
        .expect("this is the problem: it does NOT fail");
    assert_eq!(misread.version, 2);
    assert_eq!(
        misread.addresses.len(),
        7,
        "CARP's authlen sits where VRRP's address count does"
    );
    assert!(
        misread.is_resignation(),
        "CARP's advskew sits where VRRP's priority does, so a healthy CARP host looks like a \
         VRRP master resigning"
    );

    // Dispatching on the configured variant gets it right in both directions.
    assert!(matches!(
        Advertisement::decode(Variant::Carp, &CARP_ADVERTISEMENT).expect("carp"),
        Advertisement::Carp(_)
    ));
    assert!(matches!(
        Advertisement::decode(Variant::Vrrp, &VRRP_V2_ADVERTISEMENT).expect("vrrp"),
        Advertisement::Vrrp(_)
    ));
    assert_eq!(Variant::from_name("CARP").expect("name"), Variant::Carp);
    assert!(Variant::from_name("hsrp").is_err());
}

// ---------------------------------------------------------------------------
// SHA-1 and HMAC-SHA1
// ---------------------------------------------------------------------------

/// HMAC-SHA1 (RFC 2104) re-derived here, on the `sha1` crate.
///
/// This is **not** an independent hash — `codec::hmac_sha1` now uses the same crate, so
/// comparing the two would be tautological about SHA-1. What it independently re-derives is
/// the RFC 2104 *construction*: the key padding, the `0x36`/`0x5c` pads, the inner/outer
/// ordering. Its own correctness is not assumed either — `hmac_sha1_matches_the_rfc_2202_vectors`
/// runs the published vectors through **both** it and `codec::hmac_sha1`, so the spec is the
/// oracle for both before this is used to pin CARP's input ordering below.
fn reference_hmac_sha1(key: &[u8], data: &[u8]) -> [u8; 20] {
    use sha1::{Digest, Sha1};
    let mut padded = [0u8; 64];
    if key.len() > 64 {
        let mut hashed = Sha1::new();
        hashed.update(key);
        padded[..20].copy_from_slice(&hashed.finalize());
    } else {
        padded[..key.len()].copy_from_slice(key);
    }
    let ipad: Vec<u8> = padded.iter().map(|b| b ^ 0x36).collect();
    let opad: Vec<u8> = padded.iter().map(|b| b ^ 0x5c).collect();

    let mut inner = Sha1::new();
    inner.update(&ipad);
    inner.update(data);
    let inner = inner.finalize();

    let mut outer = Sha1::new();
    outer.update(&opad);
    outer.update(inner);
    let mut out = [0u8; 20];
    out.copy_from_slice(&outer.finalize());
    out
}

/// The published SHA-1 vectors.
///
/// `codec::sha1` is a thin adapter over the `sha1` crate, so these do not assert that SHA-1 is
/// correct — that is the crate's problem and its own test suite's. They assert that **we drive
/// it correctly**: an adapter that hashed the wrong buffer, truncated or reordered the digest,
/// or returned a stale array would pass every other test in this file and fail these
/// immediately.
#[test]
fn sha1_matches_the_published_vectors() {
    // FIPS 180 / RFC 3174 §7.3.
    assert_eq!(
        hex::encode(codec::sha1(b"abc")),
        "a9993e364706816aba3e25717850c26c9cd0d89d"
    );
    assert_eq!(
        hex::encode(codec::sha1(b"")),
        "da39a3ee5e6b4b0d3255bfef95601890afd80709"
    );
    assert_eq!(
        hex::encode(codec::sha1(
            b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"
        )),
        "84983e441c3bd26ebaae4aa1f95129e5e54670f1"
    );
    // The million-'a' vector: the one that catches an adapter silently truncating its input.
    assert_eq!(
        hex::encode(codec::sha1(&vec![b'a'; 1_000_000])),
        "34aa973cd4c4daa4f61eeb2bdbad27316534016f"
    );
}

/// Every length from 0 to 130, against the crate's **streaming** API.
///
/// `codec::sha1` is a one-shot call into the `sha1` crate, so comparing it against
/// `Sha1::digest` would be tautological. The oracle here is the crate's **streaming** API,
/// fed one octet at a time: streaming and one-shot are different code paths through it, and
/// the adapter bugs that matter — passing a truncated slice, hashing the wrong buffer,
/// copying only part of the digest out — show up as a disagreement between them.
///
/// 0..=130 spans both padding cases (the length fits in the final block / it needs another
/// one) and several whole blocks, which is where an off-by-one in a length or an offset
/// surfaces.
#[test]
fn sha1_agrees_with_the_streaming_api_at_every_padding_boundary() {
    use sha1::{Digest, Sha1};

    let mut seen = std::collections::HashSet::new();
    for length in 0..=130usize {
        let input: Vec<u8> = (0..length).map(|i| (i * 7 + 3) as u8).collect();

        let mut streamed = Sha1::new();
        for byte in &input {
            streamed.update([*byte]);
        }
        let mut expected = [0u8; 20];
        expected.copy_from_slice(&streamed.finalize());

        assert_eq!(
            codec::sha1(&input),
            expected,
            "codec::sha1 disagrees with a byte-at-a-time hash of the same input at length \
             {length}; the wrapper is not feeding the hasher what it was given"
        );
        assert!(
            seen.insert(codec::sha1(&input)),
            "two different inputs hashed the same at length {length} — the wrapper is \
             returning something that does not depend on its whole argument"
        );
    }
}

/// The RFC 2202 HMAC vectors, run through **both** implementations.
///
/// `codec::hmac_sha1` is the one that ships; `reference_hmac_sha1` is the test-local
/// re-derivation used further down to pin CARP's input ordering. The RFC is the oracle for
/// both, so neither is trusted on the other's say-so.
///
/// The RFC 2104 construction is the part written by hand — no HMAC crate is reachable from
/// this feature — and these vectors are exactly what catch getting it wrong: swapping the
/// `0x36` and `0x5c` pads, forgetting to hash an over-long key, or concatenating inner and
/// outer the wrong way round each fail one of the four below.
#[test]
fn hmac_sha1_matches_the_rfc_2202_vectors() {
    // RFC 2202 §3, cases 1, 2, 3 and 6.
    let cases: [(&[u8], &[u8], &str); 4] = [
        (
            &[0x0b; 20],
            b"Hi There",
            "b617318655057264e28bc0b6fb378c8ef146be00",
        ),
        (
            b"Jefe",
            b"what do ya want for nothing?",
            "effcdf6ae5eb2fa2d27416d5f184df9c259a7c79",
        ),
        (
            &[0xaa; 20],
            &[0xdd; 50],
            "125d7342b9ac11cd91a39af48aa17b4f63f175d3",
        ),
        // Case 6: an 80-octet key, longer than the 64-octet block, which RFC 2104 §2 requires
        // to be hashed down first rather than truncated.
        (
            &[0xaa; 80],
            b"Test Using Larger Than Block-Size Key - Hash Key First",
            "aa4ae5e15272d00e95705637ce8a3b55ed402112",
        ),
    ];

    for (key, data, expected) in cases {
        assert_eq!(
            hex::encode(codec::hmac_sha1(key, data)),
            expected,
            "codec::hmac_sha1 fails an RFC 2202 vector"
        );
        assert_eq!(
            hex::encode(reference_hmac_sha1(key, data)),
            expected,
            "the test's own reference fails an RFC 2202 vector, so it cannot be trusted as \
             the oracle for the CARP input ordering below"
        );
    }
}

/// CARP's authentication field is HMAC-SHA1 over
/// `version || type || vhid || addresses || counter`, with version and type as **separate**
/// octets rather than the packed `0x21` of the header.
///
/// The construction is derived from reading OpenBSD's `sys/netinet/ip_carp.c`; there is no
/// CARP RFC and no live `carp` interface has ever accepted a packet from this code. What is
/// pinned here is that our `carp_hmac` really is HMAC-SHA1 over exactly those bytes — the
/// *input ordering*, which is the only part this repository authors.
///
/// **Not** "computed with an independent hash", which is what this comment used to claim.
/// `reference_hmac_sha1` runs on the same `sha1` crate `codec::hmac_sha1` does, so comparing
/// the two says nothing whatever about SHA-1; the helper's own doc comment says so and this one
/// contradicted it. What makes the comparison worth anything is one step earlier:
/// `hmac_sha1_matches_the_rfc_2202_vectors` puts the published RFC 2202 vectors through **both**
/// functions, so the specification — not either implementation — is the oracle before either is
/// used here.
#[test]
fn the_carp_hmac_is_hmac_sha1_over_the_openbsd_input() {
    let addresses = [Ipv4Addr::new(192, 168, 1, 1), Ipv4Addr::new(10, 0, 0, 1)];
    let counter = 0x1122_3344_5566_7788u64;

    let mut expected_input = vec![CARP_VERSION, 1u8, 7u8];
    for address in &addresses {
        expected_input.extend_from_slice(&address.octets());
    }
    expected_input.extend_from_slice(&counter.to_be_bytes());

    // OpenBSD's ifconfig copies the passphrase into a 20-octet field without hashing it.
    let mut key = [0u8; 20];
    key[..b"lab-secret".len()].copy_from_slice(b"lab-secret");

    assert_eq!(
        codec::carp_hmac(b"lab-secret", 7, &addresses, counter),
        reference_hmac_sha1(&key, &expected_input),
        "carp_hmac must be HMAC-SHA1 over version || type || vhid || addresses || counter, \
         keyed with the passphrase zero-padded to 20 octets"
    );

    // A different passphrase, vhid, address set or counter must all change the result —
    // otherwise the field would authenticate nothing.
    let base = codec::carp_hmac(b"lab-secret", 7, &addresses, counter);
    assert_ne!(base, codec::carp_hmac(b"other", 7, &addresses, counter));
    assert_ne!(
        base,
        codec::carp_hmac(b"lab-secret", 8, &addresses, counter)
    );
    assert_ne!(
        base,
        codec::carp_hmac(b"lab-secret", 7, &addresses[..1], counter)
    );
    assert_ne!(
        base,
        codec::carp_hmac(b"lab-secret", 7, &addresses, counter + 1)
    );

    // A passphrase longer than 20 octets is truncated, not hashed — matching CARP_KEY_LEN.
    assert_eq!(
        codec::carp_hmac(b"01234567890123456789EXTRA", 7, &addresses, counter),
        codec::carp_hmac(b"01234567890123456789", 7, &addresses, counter)
    );
}
