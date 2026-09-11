//! The BPDU codec against literal specification bytes — in both directions.
//!
//! # Why literals and not a round trip
//!
//! Encoding with our encoder and decoding with our decoder proves only that the two agree
//! with each other; the root `CLAUDE.md` names that as circular evidence, and it is exactly
//! the mistake that let `rss` sit at Experimental while its test round-tripped one crate
//! through itself. There is no third-party STP codec in this tree to check against, and no
//! STP peer that can be run here, so the external reference is **the specification's field
//! table, written out by hand as octets**.
//!
//! ## Provenance of the literals, stated plainly
//!
//! Every byte string below was assembled by hand from IEEE 802.1D-2004 §9.3.1 (configuration
//! BPDU), §9.3.2 (topology change notification) and 802.1w §9.3.3 (RST BPDU), using the
//! default parameter values the standard recommends in Table 17-1. They are **not** extracted
//! from a named packet capture, and this file does not claim they are. What makes them useful
//! evidence is that they were derived from the field layout independently of the encoder: the
//! offsets, the byte order, the 1/256-second timer scaling and the 4/12-bit priority split are
//! written out here as constants, so an encoder that gets any of them wrong disagrees with
//! this file rather than with itself.
//!
//! ## The two things most often got wrong
//!
//! * **Timers are in units of 1/256 second.** Max age 20 s is `0x1400`, hello time 2 s is
//!   `0x0200`, forward delay 15 s is `0x0F00`. An implementation that writes the number of
//!   seconds straight into the field produces `0x0014` / `0x0002` / `0x000F`, i.e. a BPDU
//!   claiming a 78-millisecond max age, and a real bridge acts on it. Asserted at the exact
//!   offsets, and asserted to be *different* from the naive encoding.
//! * **The 16-bit priority field is two fields.** High 4 bits priority, low 12 bits system ID
//!   extension (the VLAN). `0x8001` is priority 32768 on VLAN 1, never "priority 32769", and
//!   32769 is not expressible at all.
//!
//! Run with:
//!   ./cargo-isolated.sh test --no-default-features --features stp --test server -- stp::codec

use netget::server::stp::codec::{
    self, Bpdu, BpduFlags, BridgeId, ConfigBpdu, PortId, PortRole, BPDU_TYPE_CONFIG, BPDU_TYPE_RST,
    BPDU_TYPE_TCN, VERSION_RSTP, VERSION_STP,
};

const BRIDGE_GROUP_ADDRESS: [u8; 6] = [0x01, 0x80, 0xc2, 0x00, 0x00, 0x00];

// ---------------------------------------------------------------------------
// Literal frames
// ---------------------------------------------------------------------------

/// An 802.1D configuration BPDU in its complete 802.3 + LLC frame, padded to the 60-octet
/// Ethernet minimum.
///
/// ```text
/// 00..06  01 80 c2 00 00 00        destination: the Bridge Group Address
/// 06..12  00 1c 0e 87 78 4f        source: the transmitting port's MAC
/// 12..14  00 26                    length = 38 = 3 (LLC) + 35 (BPDU); padding excluded
/// 14..17  42 42 03                 LLC: DSAP 0x42, SSAP 0x42, UI control 0x03
/// 17..19  00 00                    protocol identifier
/// 19      00                       protocol version 0 (802.1D)
/// 20      00                       BPDU type 0x00 (configuration)
/// 21      00                       flags: nothing set
/// 22..30  80 00 00 1c 0e 87 78 00  root id: priority 32768, system ID ext 0, MAC ..78:00
/// 30..34  00 00 00 00              root path cost 0 (this bridge believes it is the root)
/// 34..42  80 00 00 1c 0e 87 78 00  bridge id: the same bridge
/// 42..44  80 04                    port id: priority 128, port number 4
/// 44..46  00 00                    message age  0 s      (0 * 256)
/// 46..48  14 00                    max age      20 s     (20 * 256 = 5120 = 0x1400)
/// 48..50  02 00                    hello time   2 s      (2 * 256 = 512 = 0x0200)
/// 50..52  0f 00                    forward delay 15 s    (15 * 256 = 3840 = 0x0f00)
/// 52..60  00 * 8                   Ethernet padding to the 60-octet minimum
/// ```
const CONFIG_BPDU_FRAME: [u8; 60] = [
    0x01, 0x80, 0xc2, 0x00, 0x00, 0x00, // destination
    0x00, 0x1c, 0x0e, 0x87, 0x78, 0x4f, // source
    0x00, 0x26, // length
    0x42, 0x42, 0x03, // LLC
    0x00, 0x00, // protocol identifier
    0x00, // protocol version
    0x00, // BPDU type
    0x00, // flags
    0x80, 0x00, 0x00, 0x1c, 0x0e, 0x87, 0x78, 0x00, // root identifier
    0x00, 0x00, 0x00, 0x00, // root path cost
    0x80, 0x00, 0x00, 0x1c, 0x0e, 0x87, 0x78, 0x00, // bridge identifier
    0x80, 0x04, // port identifier
    0x00, 0x00, // message age
    0x14, 0x00, // max age
    0x02, 0x00, // hello time
    0x0f, 0x00, // forward delay
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // padding
];

/// An 802.1w RST BPDU: version 2, type 0x02, one extra "version 1 length" octet, and a flags
/// octet of `0x3c` — the designated/learning/forwarding combination a converged RSTP port
/// sends. The identifiers carry system ID extension 1, i.e. VLAN 1.
///
/// ```text
/// 12..14  00 27                    length = 39 = 3 (LLC) + 36 (RST BPDU)
/// 19      02                       protocol version 2 (802.1w)
/// 20      02                       BPDU type 0x02 (RST)
/// 21      3c                       flags 0b0011_1100:
///                                    bit0 topology change     0
///                                    bit1 proposal            0
///                                    bit2-3 port role        11 = designated
///                                    bit4 learning            1
///                                    bit5 forwarding          1
///                                    bit6 agreement           0
///                                    bit7 topology change ack 0
/// 22..24  80 01                    priority 32768 (0x8) | system ID extension 1 (0x001)
/// 52      00                       version 1 length, always zero
/// ```
const RST_BPDU_FRAME: [u8; 60] = [
    0x01, 0x80, 0xc2, 0x00, 0x00, 0x00, // destination
    0x00, 0x19, 0xe8, 0x5a, 0x08, 0x00, // source
    0x00, 0x27, // length
    0x42, 0x42, 0x03, // LLC
    0x00, 0x00, // protocol identifier
    0x02, // protocol version
    0x02, // BPDU type
    0x3c, // flags
    0x80, 0x01, 0x00, 0x19, 0xe8, 0x5a, 0x08, 0x00, // root identifier
    0x00, 0x00, 0x00, 0x00, // root path cost
    0x80, 0x01, 0x00, 0x19, 0xe8, 0x5a, 0x08, 0x00, // bridge identifier
    0x80, 0x04, // port identifier
    0x00, 0x00, // message age
    0x14, 0x00, // max age
    0x02, 0x00, // hello time
    0x0f, 0x00, // forward delay
    0x00, // version 1 length
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, // padding
];

/// A Topology Change Notification BPDU. Four octets of content: protocol identifier, version
/// 0 and type 0x80. It carries nothing else — the message *is* "something changed".
const TCN_BPDU_FRAME: [u8; 60] = [
    0x01, 0x80, 0xc2, 0x00, 0x00, 0x00, // destination
    0x00, 0x1c, 0x0e, 0x87, 0x78, 0x4f, // source
    0x00, 0x07, // length = 7 = 3 (LLC) + 4 (TCN BPDU)
    0x42, 0x42, 0x03, // LLC
    0x00, 0x00, // protocol identifier
    0x00, // protocol version
    0x80, // BPDU type 0x80 (topology change notification)
    // padding to the 60-octet minimum
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
    0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
];

/// Offsets into the frames above, so an assertion names the field it is checking.
mod offset {
    pub const LENGTH: usize = 12;
    pub const LLC: usize = 14;
    pub const BPDU: usize = 17;
    pub const VERSION: usize = 19;
    pub const TYPE: usize = 20;
    pub const FLAGS: usize = 21;
    pub const ROOT_ID: usize = 22;
    pub const ROOT_PATH_COST: usize = 30;
    pub const BRIDGE_ID: usize = 34;
    pub const PORT_ID: usize = 42;
    pub const MESSAGE_AGE: usize = 44;
    pub const MAX_AGE: usize = 46;
    pub const HELLO_TIME: usize = 48;
    pub const FORWARD_DELAY: usize = 50;
}

fn be16(frame: &[u8], at: usize) -> u16 {
    u16::from_be_bytes([frame[at], frame[at + 1]])
}

// ---------------------------------------------------------------------------
// Encoding
// ---------------------------------------------------------------------------

fn textbook_config_bpdu() -> ConfigBpdu {
    ConfigBpdu {
        version: VERSION_STP,
        bpdu_type: BPDU_TYPE_CONFIG,
        flags: BpduFlags::default(),
        root: BridgeId::new(32768, 0, [0x00, 0x1c, 0x0e, 0x87, 0x78, 0x00]).unwrap(),
        root_path_cost: 0,
        bridge: BridgeId::new(32768, 0, [0x00, 0x1c, 0x0e, 0x87, 0x78, 0x00]).unwrap(),
        port: PortId::new(128, 4).unwrap(),
        message_age_seconds: 0.0,
        max_age_seconds: 20.0,
        hello_time_seconds: 2.0,
        forward_delay_seconds: 15.0,
    }
}

#[test]
fn config_bpdu_frame_is_byte_for_byte_the_spec_layout() {
    let body = textbook_config_bpdu().encode().expect("encode config BPDU");
    assert_eq!(
        body.len(),
        codec::CONFIG_BPDU_LEN,
        "an 802.1D configuration BPDU is exactly 35 octets"
    );

    let frame = codec::encode_frame(
        BRIDGE_GROUP_ADDRESS,
        [0x00, 0x1c, 0x0e, 0x87, 0x78, 0x4f],
        &body,
    )
    .expect("a 35-octet BPDU fits the 802.3 length field");

    assert_eq!(
        frame,
        CONFIG_BPDU_FRAME.to_vec(),
        "the encoded frame must match the hand-derived 802.1D §9.3.1 layout octet for octet.\n\
         got      {}\n\
         expected {}",
        hex::encode(&frame),
        hex::encode(CONFIG_BPDU_FRAME)
    );
}

#[test]
fn rst_bpdu_frame_is_byte_for_byte_the_spec_layout() {
    let mac = [0x00, 0x19, 0xe8, 0x5a, 0x08, 0x00];
    let bpdu = ConfigBpdu {
        version: VERSION_RSTP,
        bpdu_type: BPDU_TYPE_RST,
        flags: BpduFlags {
            topology_change: false,
            proposal: false,
            port_role: PortRole::Designated,
            learning: true,
            forwarding: true,
            agreement: false,
            topology_change_ack: false,
        },
        // System ID extension 1 = VLAN 1, which is what makes the leading octets 0x8001.
        root: BridgeId::new(32768, 1, mac).unwrap(),
        root_path_cost: 0,
        bridge: BridgeId::new(32768, 1, mac).unwrap(),
        port: PortId::new(128, 4).unwrap(),
        message_age_seconds: 0.0,
        max_age_seconds: 20.0,
        hello_time_seconds: 2.0,
        forward_delay_seconds: 15.0,
    };

    let body = bpdu.encode().expect("encode RST BPDU");
    assert_eq!(
        body.len(),
        codec::RST_BPDU_LEN,
        "an RST BPDU is 36 octets: the 35-octet configuration body plus the version 1 length"
    );
    assert_eq!(
        body[35], 0x00,
        "802.1w §9.3.3: the version 1 length octet is always zero"
    );

    let frame = codec::encode_frame(BRIDGE_GROUP_ADDRESS, mac, &body)
        .expect("a 36-octet RST BPDU fits the 802.3 length field");
    assert_eq!(
        frame,
        RST_BPDU_FRAME.to_vec(),
        "the encoded RST frame must match the hand-derived 802.1w §9.3.3 layout octet for \
         octet.\ngot      {}\nexpected {}",
        hex::encode(&frame),
        hex::encode(RST_BPDU_FRAME)
    );
}

#[test]
fn tcn_bpdu_frame_is_byte_for_byte_the_spec_layout() {
    let frame = codec::encode_frame(
        BRIDGE_GROUP_ADDRESS,
        [0x00, 0x1c, 0x0e, 0x87, 0x78, 0x4f],
        &codec::encode_tcn_bpdu(),
    )
    .expect("a 4-octet TCN BPDU fits the 802.3 length field");
    assert_eq!(
        frame,
        TCN_BPDU_FRAME.to_vec(),
        "the encoded TCN frame must match the hand-derived 802.1D §9.3.2 layout octet for \
         octet.\ngot      {}\nexpected {}",
        hex::encode(&frame),
        hex::encode(TCN_BPDU_FRAME)
    );
    assert_eq!(codec::encode_tcn_bpdu().len(), codec::TCN_BPDU_LEN);
}

// ---------------------------------------------------------------------------
// The 1/256-second timer encoding
// ---------------------------------------------------------------------------

#[test]
fn timers_are_encoded_in_units_of_one_256th_of_a_second() {
    let body = textbook_config_bpdu().encode().unwrap();
    let frame = codec::encode_frame(BRIDGE_GROUP_ADDRESS, [0; 6], &body)
        .expect("a 35-octet BPDU fits the 802.3 length field");

    // The four timer fields, at their spec offsets, with the values every 802.1D capture
    // shows for the recommended defaults.
    assert_eq!(be16(&frame, offset::MESSAGE_AGE), 0x0000, "message age 0 s");
    assert_eq!(
        be16(&frame, offset::MAX_AGE),
        0x1400,
        "max age 20 s must encode as 20 * 256 = 5120 = 0x1400"
    );
    assert_eq!(
        be16(&frame, offset::HELLO_TIME),
        0x0200,
        "hello time 2 s must encode as 2 * 256 = 512 = 0x0200"
    );
    assert_eq!(
        be16(&frame, offset::FORWARD_DELAY),
        0x0f00,
        "forward delay 15 s must encode as 15 * 256 = 3840 = 0x0f00"
    );

    // The failure this guards against is writing seconds straight into the field. Assert the
    // naive encoding explicitly, so the test says what went wrong rather than just "5120 !=
    // 20".
    assert_ne!(
        be16(&frame, offset::MAX_AGE),
        20,
        "max age is being written as a raw number of seconds; the field is 1/256 s units, so \
         this BPDU claims a 78 ms max age"
    );
    assert_ne!(be16(&frame, offset::HELLO_TIME), 2);
    assert_ne!(be16(&frame, offset::FORWARD_DELAY), 15);
}

#[test]
fn the_timer_unit_has_1_256th_second_resolution_in_both_directions() {
    assert_eq!(codec::seconds_to_ticks(0.0).unwrap(), 0);
    assert_eq!(codec::seconds_to_ticks(1.0).unwrap(), 256);
    assert_eq!(
        codec::seconds_to_ticks(0.5).unwrap(),
        128,
        "half a second is 128 ticks — the field is not whole seconds"
    );
    assert_eq!(codec::seconds_to_ticks(20.0).unwrap(), 5120);
    assert_eq!(codec::ticks_to_seconds(5120), 20.0);
    assert_eq!(codec::ticks_to_seconds(128), 0.5);

    // 16 bits of 1/256 s tops out just under 256 seconds.
    assert!(codec::seconds_to_ticks(255.0).is_ok());
    assert!(
        codec::seconds_to_ticks(256.0).is_err(),
        "256 s overflows the 16-bit field and must be refused, not silently wrapped"
    );
    assert!(codec::seconds_to_ticks(-1.0).is_err());
}

// ---------------------------------------------------------------------------
// The priority / system-ID-extension packing
// ---------------------------------------------------------------------------

#[test]
fn bridge_priority_and_system_id_extension_share_one_16_bit_field() {
    // Priority alone, VLAN 0: the classic 0x8000.
    assert_eq!(
        BridgeId::new(32768, 0, [0; 6]).unwrap().encode()[0..2],
        [0x80, 0x00]
    );
    // Priority 32768 on VLAN 1: 0x8001. This is the value people misread as "32769".
    assert_eq!(
        BridgeId::new(32768, 1, [0; 6]).unwrap().encode()[0..2],
        [0x80, 0x01]
    );
    // Priority 0 claims the root of the whole spanning tree.
    assert_eq!(
        BridgeId::new(0, 0, [0; 6]).unwrap().encode()[0..2],
        [0x00, 0x00]
    );
    // The maximum, on the highest VLAN: 0xf000 | 0x0fff.
    assert_eq!(
        BridgeId::new(61440, 4095, [0; 6]).unwrap().encode()[0..2],
        [0xff, 0xff]
    );
    // 4096 is one step.
    assert_eq!(
        BridgeId::new(4096, 100, [0; 6]).unwrap().encode()[0..2],
        [0x10, 0x64]
    );
}

#[test]
fn a_priority_that_is_not_a_multiple_of_4096_is_refused() {
    // 32769 is what you get from misreading 0x8001 as a single number. There is nowhere to
    // put the extra 1: those bits are the VLAN.
    let err = BridgeId::new(32769, 0, [0; 6]).expect_err(
        "a priority that is not a multiple of 4096 cannot be encoded and must be refused",
    );
    let message = format!("{err:#}");
    assert!(
        message.contains("4096") && message.contains("system ID extension"),
        "the refusal must explain that the low bits are the system ID extension, got: {message}"
    );

    assert!(BridgeId::new(61441, 0, [0; 6]).is_err());
    assert!(
        BridgeId::new(32768, 4096, [0; 6]).is_err(),
        "the system ID extension is 12 bits"
    );
}

#[test]
fn decoding_splits_the_priority_field_the_same_way() {
    let id = BridgeId::decode(&[0x80, 0x01, 0x00, 0x19, 0xe8, 0x5a, 0x08, 0x00]).unwrap();
    assert_eq!(id.priority, 32768);
    assert_eq!(id.system_id_extension, 1);
    assert_eq!(id.mac_string(), "00:19:e8:5a:08:00");
}

#[test]
fn port_priority_and_port_number_share_one_16_bit_field() {
    assert_eq!(PortId::new(128, 4).unwrap().encode(), [0x80, 0x04]);
    assert_eq!(PortId::new(0, 1).unwrap().encode(), [0x00, 0x01]);
    // 0xf000 | 0x0fff.
    assert_eq!(PortId::new(240, 4095).unwrap().encode(), [0xff, 0xff]);

    let decoded = PortId::decode(&[0x80, 0x04]).unwrap();
    assert_eq!(decoded.priority, 128);
    assert_eq!(decoded.number, 4);

    assert!(
        PortId::new(129, 1).is_err(),
        "port priority moves in steps of 16; the low 12 bits are the port number"
    );
    assert!(PortId::new(128, 4096).is_err());
}

// ---------------------------------------------------------------------------
// Decoding
// ---------------------------------------------------------------------------

#[test]
fn a_literal_config_bpdu_decodes_to_the_documented_fields() {
    let frame = codec::decode_frame(&CONFIG_BPDU_FRAME).expect("decode 802.3 frame");
    assert_eq!(frame.destination, BRIDGE_GROUP_ADDRESS);
    assert_eq!(frame.source, [0x00, 0x1c, 0x0e, 0x87, 0x78, 0x4f]);
    assert_eq!(
        frame.payload.len(),
        codec::CONFIG_BPDU_LEN,
        "the 802.3 length field must be used to trim the Ethernet padding off the BPDU"
    );

    let Bpdu::Config(bpdu) = Bpdu::decode(&frame.payload).expect("decode BPDU") else {
        panic!("type 0x00 must decode as a configuration BPDU");
    };

    assert_eq!(bpdu.version, VERSION_STP);
    assert_eq!(bpdu.bpdu_type, BPDU_TYPE_CONFIG);
    assert!(!bpdu.is_rstp());

    assert_eq!(bpdu.root.priority, 32768);
    assert_eq!(bpdu.root.system_id_extension, 0);
    assert_eq!(bpdu.root.mac_string(), "00:1c:0e:87:78:00");
    assert_eq!(bpdu.root_path_cost, 0);

    assert_eq!(bpdu.bridge.priority, 32768);
    assert_eq!(bpdu.bridge.mac_string(), "00:1c:0e:87:78:00");

    assert_eq!(bpdu.port.priority, 128);
    assert_eq!(bpdu.port.number, 4);

    assert_eq!(bpdu.message_age_seconds, 0.0);
    assert_eq!(bpdu.max_age_seconds, 20.0, "0x1400 ticks is 20 seconds");
    assert_eq!(bpdu.hello_time_seconds, 2.0, "0x0200 ticks is 2 seconds");
    assert_eq!(
        bpdu.forward_delay_seconds, 15.0,
        "0x0f00 ticks is 15 seconds"
    );

    assert_eq!(bpdu.flags, BpduFlags::default());
    assert_eq!(bpdu.flags.port_role, PortRole::Unknown);
}

#[test]
fn a_literal_rst_bpdu_decodes_its_flags_into_named_booleans() {
    let frame = codec::decode_frame(&RST_BPDU_FRAME).expect("decode 802.3 frame");
    assert_eq!(
        frame.payload.len(),
        codec::RST_BPDU_LEN,
        "an RST BPDU is one octet longer than a configuration BPDU"
    );

    let Bpdu::Config(bpdu) = Bpdu::decode(&frame.payload).expect("decode BPDU") else {
        panic!("type 0x02 must decode as an RST BPDU");
    };

    assert_eq!(bpdu.version, VERSION_RSTP);
    assert_eq!(bpdu.bpdu_type, BPDU_TYPE_RST);
    assert!(bpdu.is_rstp());

    // 0x3c, decomposed.
    assert!(!bpdu.flags.topology_change);
    assert!(!bpdu.flags.proposal);
    assert_eq!(bpdu.flags.port_role, PortRole::Designated);
    assert!(bpdu.flags.learning);
    assert!(bpdu.flags.forwarding);
    assert!(!bpdu.flags.agreement);
    assert!(!bpdu.flags.topology_change_ack);

    assert_eq!(bpdu.root.priority, 32768);
    assert_eq!(
        bpdu.root.system_id_extension, 1,
        "0x8001 is priority 32768 on VLAN 1"
    );
    assert_eq!(bpdu.root.mac_string(), "00:19:e8:5a:08:00");
}

#[test]
fn a_literal_tcn_bpdu_decodes_as_a_topology_change_notification() {
    let frame = codec::decode_frame(&TCN_BPDU_FRAME).expect("decode 802.3 frame");
    assert_eq!(frame.payload.len(), codec::TCN_BPDU_LEN);
    assert_eq!(frame.payload, vec![0x00, 0x00, 0x00, BPDU_TYPE_TCN]);
    assert_eq!(
        Bpdu::decode(&frame.payload).expect("decode BPDU"),
        Bpdu::TopologyChangeNotification
    );
}

#[test]
fn every_flag_bit_maps_to_the_position_802_1w_gives_it() {
    let cases: [(u8, BpduFlags); 8] = [
        (
            0x01,
            BpduFlags {
                topology_change: true,
                ..Default::default()
            },
        ),
        (
            0x02,
            BpduFlags {
                proposal: true,
                ..Default::default()
            },
        ),
        (
            0x04,
            BpduFlags {
                port_role: PortRole::Alternate,
                ..Default::default()
            },
        ),
        (
            0x08,
            BpduFlags {
                port_role: PortRole::Root,
                ..Default::default()
            },
        ),
        (
            0x0c,
            BpduFlags {
                port_role: PortRole::Designated,
                ..Default::default()
            },
        ),
        (
            0x10,
            BpduFlags {
                learning: true,
                ..Default::default()
            },
        ),
        (
            0x20,
            BpduFlags {
                forwarding: true,
                ..Default::default()
            },
        ),
        (
            0x80,
            BpduFlags {
                topology_change_ack: true,
                ..Default::default()
            },
        ),
    ];

    for (byte, flags) in cases {
        assert_eq!(
            flags.to_byte(),
            byte,
            "{flags:?} must encode as 0x{byte:02x}"
        );
        assert_eq!(
            BpduFlags::from_byte(byte),
            flags,
            "0x{byte:02x} must decode to {flags:?}"
        );
    }

    // 0x40 is agreement, checked separately because it shares its shape with nothing else.
    assert_eq!(
        BpduFlags {
            agreement: true,
            ..Default::default()
        }
        .to_byte(),
        0x40
    );

    // The full designated/learning/forwarding byte from the RST literal above.
    assert_eq!(
        BpduFlags::from_byte(0x3c),
        BpduFlags {
            topology_change: false,
            proposal: false,
            port_role: PortRole::Designated,
            learning: true,
            forwarding: true,
            agreement: false,
            topology_change_ack: false,
        }
    );
}

// ---------------------------------------------------------------------------
// Framing
// ---------------------------------------------------------------------------

#[test]
fn the_802_3_length_field_counts_llc_plus_bpdu_and_excludes_padding() {
    assert_eq!(
        be16(&CONFIG_BPDU_FRAME, offset::LENGTH),
        38,
        "3 (LLC) + 35 (configuration BPDU)"
    );
    assert_eq!(
        be16(&RST_BPDU_FRAME, offset::LENGTH),
        39,
        "3 (LLC) + 36 (RST BPDU)"
    );
    assert_eq!(
        be16(&TCN_BPDU_FRAME, offset::LENGTH),
        7,
        "3 (LLC) + 4 (TCN BPDU)"
    );

    // Every frame is padded to the Ethernet minimum, and the length field does not count it.
    for frame in [&CONFIG_BPDU_FRAME, &RST_BPDU_FRAME, &TCN_BPDU_FRAME] {
        assert_eq!(frame.len(), codec::MIN_ETHERNET_FRAME_LEN);
        assert!(
            (be16(frame, offset::LENGTH) as usize) < frame.len() - codec::ETHERNET_HEADER_LEN,
            "padding must not be counted in the length field"
        );
    }
}

#[test]
fn the_llc_header_is_the_stp_one_and_nothing_else_is_accepted() {
    assert_eq!(
        &CONFIG_BPDU_FRAME[offset::LLC..offset::BPDU],
        &[0x42, 0x42, 0x03],
        "STP's LLC header: DSAP 0x42, SSAP 0x42, unnumbered information control 0x03"
    );

    // A SNAP frame (LLC 0xaa/0xaa/0x03) is not a BPDU — that is CDP's encapsulation.
    let mut snap = CONFIG_BPDU_FRAME;
    snap[offset::LLC] = 0xaa;
    snap[offset::LLC + 1] = 0xaa;
    let err = codec::decode_frame(&snap).expect_err("a SNAP frame is not a BPDU");
    assert!(format!("{err:#}").contains("LLC"));

    // An Ethernet II frame has an EtherType where 802.3 has a length. BPDUs are never
    // carried that way, and reading 0x0800 as a length would produce nonsense.
    let mut ethernet_ii = CONFIG_BPDU_FRAME;
    ethernet_ii[offset::LENGTH] = 0x08;
    ethernet_ii[offset::LENGTH + 1] = 0x00;
    let err = codec::decode_frame(&ethernet_ii)
        .expect_err("an EtherType in the length field is not an 802.3 frame");
    assert!(format!("{err:#}").contains("EtherType"));
}

#[test]
fn a_truncated_frame_is_refused_rather_than_read_past() {
    assert!(codec::decode_frame(&[]).is_err());
    assert!(codec::decode_frame(&CONFIG_BPDU_FRAME[..16]).is_err());

    // Claiming 38 octets of client data while carrying 10 must not read past the buffer: the
    // decoder trusts whichever is smaller.
    let short = &CONFIG_BPDU_FRAME[..27];
    let decoded = codec::decode_frame(short).expect("a short frame still decodes its header");
    assert_eq!(decoded.payload.len(), 27 - 17);
    assert!(
        Bpdu::decode(&decoded.payload).is_err(),
        "a 10-octet body is not a configuration BPDU"
    );
}

#[test]
fn an_unknown_bpdu_type_is_reported_rather_than_guessed() {
    let mut body = textbook_config_bpdu().encode().unwrap();
    body[3] = 0x7f;
    let err = Bpdu::decode(&body).expect_err("0x7f is not a BPDU type");
    assert!(format!("{err:#}").contains("0x7f"));

    // A non-zero protocol identifier is not spanning tree at all.
    let mut body = textbook_config_bpdu().encode().unwrap();
    body[0] = 0x01;
    assert!(Bpdu::decode(&body).is_err());
}

#[test]
fn the_version_and_type_octets_sit_where_the_spec_puts_them() {
    assert_eq!(CONFIG_BPDU_FRAME[offset::VERSION], VERSION_STP);
    assert_eq!(CONFIG_BPDU_FRAME[offset::TYPE], BPDU_TYPE_CONFIG);
    assert_eq!(CONFIG_BPDU_FRAME[offset::FLAGS], 0x00);

    assert_eq!(RST_BPDU_FRAME[offset::VERSION], VERSION_RSTP);
    assert_eq!(RST_BPDU_FRAME[offset::TYPE], BPDU_TYPE_RST);
    assert_eq!(RST_BPDU_FRAME[offset::FLAGS], 0x3c);

    assert_eq!(CONFIG_BPDU_FRAME[offset::ROOT_ID], 0x80);
    assert_eq!(
        u32::from_be_bytes([
            CONFIG_BPDU_FRAME[offset::ROOT_PATH_COST],
            CONFIG_BPDU_FRAME[offset::ROOT_PATH_COST + 1],
            CONFIG_BPDU_FRAME[offset::ROOT_PATH_COST + 2],
            CONFIG_BPDU_FRAME[offset::ROOT_PATH_COST + 3],
        ]),
        0
    );
    assert_eq!(CONFIG_BPDU_FRAME[offset::BRIDGE_ID], 0x80);
    assert_eq!(be16(&CONFIG_BPDU_FRAME, offset::PORT_ID), 0x8004);
}

#[test]
fn a_root_path_cost_survives_the_four_octet_field() {
    let mut bpdu = textbook_config_bpdu();
    // 200_000 is the 802.1D-2004 recommended cost of a 10 Mb/s link, and does not fit 16 bits.
    bpdu.root_path_cost = 200_000;
    let body = bpdu.encode().unwrap();
    assert_eq!(&body[13..17], &200_000u32.to_be_bytes());

    let Bpdu::Config(decoded) = Bpdu::decode(&body).unwrap() else {
        panic!("configuration BPDU");
    };
    assert_eq!(decoded.root_path_cost, 200_000);
}

#[test]
fn mac_addresses_round_trip_through_their_text_form() {
    assert_eq!(
        codec::parse_mac("01:80:c2:00:00:00").unwrap(),
        BRIDGE_GROUP_ADDRESS
    );
    assert_eq!(
        codec::parse_mac("01-80-C2-00-00-00").unwrap(),
        BRIDGE_GROUP_ADDRESS
    );
    assert_eq!(
        codec::format_mac(&BRIDGE_GROUP_ADDRESS),
        "01:80:c2:00:00:00"
    );

    assert!(codec::parse_mac("01:80:c2:00:00").is_err());
    assert!(codec::parse_mac("zz:80:c2:00:00:00").is_err());
}

/// The 802.3 length field is bounded in **both** directions, at the same value.
///
/// `decode_frame` has always rejected a declared length above 1500, on the grounds that IEEE
/// 802.3 reserves 0x0600 and above for EtherType. `encode_frame` narrowed with a bare `as u16`
/// and did not, which is the asymmetry this pins: a frame whose length field lands in the
/// EtherType range is not an oversized 802.3 frame, it is an Ethernet II frame of some other
/// protocol, and the BPDU behind it is never parsed by anybody.
///
/// No BPDU this codec produces can reach that size — they are 4, 35 or 36 octets — so the guard
/// exists for a future caller of what is a `pub fn` taking an arbitrary slice.
#[test]
fn the_8023_length_field_is_bounded_in_both_directions() {
    let oversized = vec![0u8; 2000];
    let client_len = codec::LLC_HEADER_LEN + oversized.len();
    assert!(
        client_len >= 0x0600,
        "this test is only meaningful if the length field would land in the EtherType range"
    );

    let err = codec::encode_frame(BRIDGE_GROUP_ADDRESS, [0; 6], &oversized)
        .expect_err("a frame whose length field is an EtherType must be refused")
        .to_string();
    assert!(
        err.contains(&codec::MAX_8023_LENGTH.to_string()) && err.contains("EtherType"),
        "the error must say why {} is the bound, got: {err}",
        codec::MAX_8023_LENGTH
    );

    // The largest body that is still unambiguously a length is accepted, so the bound sits
    // exactly on the encapsulation boundary rather than somewhere convenient.
    let largest = vec![0u8; codec::MAX_8023_LENGTH - codec::LLC_HEADER_LEN];
    let frame = codec::encode_frame(BRIDGE_GROUP_ADDRESS, [0; 6], &largest)
        .expect("a client-data length of exactly 1500 is still a length");
    assert_eq!(
        be16(&frame, 12) as usize,
        codec::MAX_8023_LENGTH,
        "the length field must carry the client-data length, padding excluded"
    );

    // And the decode side refuses the same value, which is where the constant came from.
    let mut ethernet_ii = vec![0u8; 64];
    ethernet_ii[12..14].copy_from_slice(&0x0800u16.to_be_bytes()); // IPv4 EtherType
    let err = codec::decode_frame(&ethernet_ii)
        .expect_err("an EtherType in the length field is not an 802.3 frame")
        .to_string();
    assert!(
        err.contains("EtherType"),
        "the decode-side refusal must name the same reason, got: {err}"
    );
}
