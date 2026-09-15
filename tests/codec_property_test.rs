//! Property-based round-trip tests for NetGet's hand-written codecs.
//!
//! Four properties, stated once per codec and checked against thousands of generated values
//! rather than the handful a table-driven test can hold:
//!
//! 1. **Round-trip** — `decode(encode(x)) == x` for every structurally valid `x`.
//! 2. **Bounded output** — `encode(x)` either stays inside the bound its own decoder enforces,
//!    or refuses. This is the `m3ua` shape: `MAX_MESSAGE_LEN` was checked on decode, against a
//!    hostile peer, and not on encode, against the model.
//! 3. **Decode never panics** — arbitrary bytes produce `Err`, not a panic and not a hang.
//!    (`cargo-fuzz` covers this far more thoroughly; a cheap proptest version is worth having
//!    because it runs in the ordinary suite.)
//! 4. **Idempotent normalisation** — where a codec normalises (case, padding, line endings,
//!    control characters), `f(f(x)) == f(x)`.
//!
//! # Generators must produce structurally valid values
//!
//! Otherwise property 1 is vacuous: a generator that mostly produces values the encoder
//! refuses proves that the encoder refuses things. Every generator below is written against
//! the codec's own stated contract — a VRRPv3 interval is a whole number of centiseconds, an
//! STP bridge priority is a multiple of 4096, a CAN FD payload is one of the sixteen encodable
//! lengths — and the values those contracts *exclude* get their own assertion that they are
//! refused.
//!
//! # A failing property is a finding, not a property to weaken
//!
//! Properties that do not hold today are kept here, stating what *should* hold, behind
//! `#[ignore = "FINDING: …"]`. Running `cargo test -- --ignored` shows every one. They are
//! listed in the module header of the section they belong to, with the minimal counterexample
//! proptest shrank to.

#![allow(clippy::uninlined_format_args)]

// ===========================================================================================
// Shared helpers
// ===========================================================================================

/// Generated cases per property.
///
/// Deliberately modest: this file is part of the ordinary suite, which runs at
/// `--test-threads=100`, and a fuzzing campaign belongs in `cargo-fuzz` rather than here.
#[allow(dead_code)]
const CASES: u32 = 256;

/// Cases for the cheaper "decode never panics" properties, which allocate nothing.
#[allow(dead_code)]
const PANIC_CASES: u32 = 512;

#[allow(unused_macros)]
macro_rules! codec_config {
    ($cases:expr) => {
        proptest::test_runner::Config {
            cases: $cases,
            // Never write `.proptest-regressions` into the source tree: this repository is
            // edited by several agents at once and a failing case belongs in the test output,
            // not in a file someone else has to notice.
            failure_persistence: None,
            ..proptest::test_runner::Config::default()
        }
    };
}

// ===========================================================================================
// `src/utils/bencode.rs` — the structural pre-check every bencode decoder in the tree runs
// ===========================================================================================
//
// One-directional by design: it validates, it does not decode. So property 1 is stated the
// other way round — an independently rendered, well-formed bencode value must be *accepted* —
// which is the same evidence with the encoder living in the test.

mod bencode_props {
    use netget::utils::bencode::{
        check_bencode_structure, check_bencode_structure_with_limit, BencodeStructureError,
    };
    use proptest::prelude::*;

    /// A bencode value, rendered by this test rather than by the crate under test.
    #[derive(Debug, Clone)]
    enum BVal {
        Int(i64),
        Bytes(Vec<u8>),
        List(Vec<BVal>),
        Dict(Vec<(Vec<u8>, BVal)>),
    }

    fn render(value: &BVal, out: &mut Vec<u8>) {
        match value {
            BVal::Int(n) => {
                out.push(b'i');
                out.extend_from_slice(n.to_string().as_bytes());
                out.push(b'e');
            }
            BVal::Bytes(b) => {
                out.extend_from_slice(b.len().to_string().as_bytes());
                out.push(b':');
                out.extend_from_slice(b);
            }
            BVal::List(items) => {
                out.push(b'l');
                for item in items {
                    render(item, out);
                }
                out.push(b'e');
            }
            BVal::Dict(entries) => {
                out.push(b'd');
                for (key, val) in entries {
                    render(&BVal::Bytes(key.clone()), out);
                    render(val, out);
                }
                out.push(b'e');
            }
        }
    }

    /// Container nesting depth, counted the way `check_bencode_structure` counts it.
    fn depth_of(value: &BVal) -> usize {
        match value {
            BVal::Int(_) | BVal::Bytes(_) => 0,
            BVal::List(items) => 1 + items.iter().map(depth_of).max().unwrap_or(0),
            BVal::Dict(entries) => 1 + entries.iter().map(|(_, v)| depth_of(v)).max().unwrap_or(0),
        }
    }

    fn arb_bval() -> impl Strategy<Value = BVal> {
        let leaf = prop_oneof![
            any::<i64>().prop_map(BVal::Int),
            proptest::collection::vec(any::<u8>(), 0..12).prop_map(BVal::Bytes),
        ];
        leaf.prop_recursive(5, 48, 3, |inner| {
            prop_oneof![
                proptest::collection::vec(inner.clone(), 0..3).prop_map(BVal::List),
                proptest::collection::vec(
                    (proptest::collection::vec(any::<u8>(), 0..6), inner),
                    0..3
                )
                .prop_map(BVal::Dict),
            ]
        })
    }

    /// `l` repeated `depth` times, then `e` repeated `depth` times.
    fn nest(depth: usize) -> Vec<u8> {
        let mut out = vec![b'l'; depth];
        out.extend(std::iter::repeat_n(b'e', depth));
        out
    }

    proptest! {
        #![proptest_config(codec_config!(crate::CASES))]

        /// Property 1 (inverted): every well-formed value is accepted.
        #[test]
        fn well_formed_bencode_is_accepted(value in arb_bval()) {
            let mut bytes = Vec::new();
            render(&value, &mut bytes);
            prop_assert!(
                check_bencode_structure_with_limit(&bytes, 64).is_ok(),
                "rejected a well-formed value: {:?}",
                String::from_utf8_lossy(&bytes)
            );
        }
    }

    proptest! {
        #![proptest_config(codec_config!(crate::PANIC_CASES))]

        /// Property 3: arbitrary bytes are an `Err`, never a panic and never a hang.
        #[test]
        fn arbitrary_bytes_never_panic(data in proptest::collection::vec(any::<u8>(), 0..512)) {
            let _ = check_bencode_structure(&data);
        }

        /// Property 3, biased: bytes drawn only from bencode's own alphabet, which is where a
        /// length-scanner or an unbalanced-container bug actually lives.
        #[test]
        fn bencode_alphabet_bytes_never_panic(
            data in proptest::collection::vec(
                prop::sample::select(vec![
                    b'd', b'l', b'e', b'i', b':', b'-', b'0', b'1', b'2', b'9', b'x',
                ]),
                0..256,
            )
        ) {
            let _ = check_bencode_structure(&data);
        }
    }

    proptest! {
        #![proptest_config(codec_config!(64))]

        /// The depth bound is exactly the bound, in both directions. The whole point of this
        /// module is that one byte buys one level, so an off-by-one here is a stack overflow.
        #[test]
        fn depth_limit_is_exact(limit in 1usize..24, extra in 0usize..8) {
            prop_assert!(check_bencode_structure_with_limit(&nest(limit), limit).is_ok());
            let over = limit + 1 + extra;
            prop_assert_eq!(
                check_bencode_structure_with_limit(&nest(over), limit),
                Err(BencodeStructureError::TooDeep { limit })
            );
        }

        /// A generated value is accepted at exactly its own depth and refused one below it.
        #[test]
        fn generated_value_depth_is_the_boundary(value in arb_bval()) {
            let mut bytes = Vec::new();
            render(&value, &mut bytes);
            let depth = depth_of(&value);
            prop_assert!(check_bencode_structure_with_limit(&bytes, depth.max(1)).is_ok());
            if depth > 0 {
                prop_assert!(check_bencode_structure_with_limit(&bytes, depth - 1).is_err());
            }
        }
    }
}

// ===========================================================================================
// Modbus — `src/server/modbus/codec.rs`
// ===========================================================================================
//
// FINDINGS (see the `#[ignore]`d tests at the end of this module):
//
//  * `encode_adu` applies no bound to the PDU it is given, while `try_parse_adu` refuses any
//    MBAP length outside `2..=254`. Exactly the m3ua shape.
//  * `encode_registers_response` writes `(values.len() * 2) as u8` and
//    `encode_bits_response` writes `byte_count as u8`, both unchecked. 128 registers produce
//    a byte count of 0 followed by 256 octets of data.
//
// Neither is reachable through `mod.rs` today, which bounds the model's value list to the
// quantity `parse_request` already validated (≤2000 bits, ≤125 registers). Both are unguarded
// in a `pub fn`.

#[cfg(feature = "modbus")]
mod modbus_props {
    use netget::server::modbus::codec::{
        encode_adu, encode_bits_response, encode_exception, encode_registers_response,
        encode_write_ack, parse_request, try_parse_adu, Adu, ModbusRequest, FC_READ_COILS,
        FC_READ_DISCRETE_INPUTS, FC_READ_HOLDING_REGISTERS, FC_READ_INPUT_REGISTERS,
        FC_WRITE_MULTIPLE_COILS, FC_WRITE_MULTIPLE_REGISTERS, FC_WRITE_SINGLE_COIL,
        FC_WRITE_SINGLE_REGISTER, MAX_PDU_LEN, MBAP_HEADER_LEN,
    };
    use proptest::prelude::*;

    /// A spec-legal request, generated at the boundaries `parse_request` enforces.
    ///
    /// `start + quantity` is kept inside the 16-bit address space, because `check_range`
    /// refuses anything else — a generator that ignored it would spend most of its cases
    /// proving that refusal rather than exercising the round trip.
    fn arb_request() -> impl Strategy<Value = ModbusRequest> {
        let bit_read = (any::<u16>(), 1u16..=2000u16).prop_filter_map(
            "start + quantity must stay in the 16-bit address space",
            |(start, quantity)| {
                (start as u32 + quantity as u32 <= 0x1_0000).then_some((start, quantity))
            },
        );
        let reg_read = (any::<u16>(), 1u16..=125u16).prop_filter_map(
            "start + quantity must stay in the 16-bit address space",
            |(start, quantity)| {
                (start as u32 + quantity as u32 <= 0x1_0000).then_some((start, quantity))
            },
        );
        let multi_coils = (any::<u16>(), 1usize..=1968usize).prop_filter_map(
            "start + quantity must stay in the 16-bit address space",
            |(start, n)| (start as u32 + n as u32 <= 0x1_0000).then_some((start, n)),
        );
        let multi_regs = (any::<u16>(), 1usize..=123usize).prop_filter_map(
            "start + quantity must stay in the 16-bit address space",
            |(start, n)| (start as u32 + n as u32 <= 0x1_0000).then_some((start, n)),
        );

        prop_oneof![
            bit_read
                .clone()
                .prop_map(|(start, quantity)| ModbusRequest::ReadCoils { start, quantity }),
            bit_read.prop_map(|(start, quantity)| ModbusRequest::ReadDiscreteInputs {
                start,
                quantity
            }),
            reg_read
                .clone()
                .prop_map(|(start, quantity)| ModbusRequest::ReadHoldingRegisters {
                    start,
                    quantity
                }),
            reg_read.prop_map(|(start, quantity)| ModbusRequest::ReadInputRegisters {
                start,
                quantity
            }),
            (any::<u16>(), any::<bool>())
                .prop_map(|(address, value)| ModbusRequest::WriteSingleCoil { address, value }),
            (any::<u16>(), any::<u16>())
                .prop_map(|(address, value)| ModbusRequest::WriteSingleRegister { address, value }),
            multi_coils.prop_flat_map(|(start, n)| {
                proptest::collection::vec(any::<bool>(), n..=n)
                    .prop_map(move |values| ModbusRequest::WriteMultipleCoils { start, values })
            }),
            multi_regs.prop_flat_map(|(start, n)| {
                proptest::collection::vec(any::<u16>(), n..=n)
                    .prop_map(move |values| ModbusRequest::WriteMultipleRegisters { start, values })
            }),
        ]
    }

    /// The request PDU bytes for a parsed request — the inverse `codec.rs` does not expose,
    /// written here from RFC 1.1b so `parse_request` is checked against an independent
    /// reading of the specification rather than against itself.
    fn encode_request_pdu(request: &ModbusRequest) -> Vec<u8> {
        let mut pdu = vec![request.function_code()];
        match request {
            ModbusRequest::ReadCoils { start, quantity }
            | ModbusRequest::ReadDiscreteInputs { start, quantity }
            | ModbusRequest::ReadHoldingRegisters { start, quantity }
            | ModbusRequest::ReadInputRegisters { start, quantity } => {
                pdu.extend_from_slice(&start.to_be_bytes());
                pdu.extend_from_slice(&quantity.to_be_bytes());
            }
            ModbusRequest::WriteSingleCoil { address, value } => {
                pdu.extend_from_slice(&address.to_be_bytes());
                pdu.extend_from_slice(&if *value { 0xFF00u16 } else { 0x0000u16 }.to_be_bytes());
            }
            ModbusRequest::WriteSingleRegister { address, value } => {
                pdu.extend_from_slice(&address.to_be_bytes());
                pdu.extend_from_slice(&value.to_be_bytes());
            }
            ModbusRequest::WriteMultipleCoils { start, values } => {
                pdu.extend_from_slice(&start.to_be_bytes());
                pdu.extend_from_slice(&(values.len() as u16).to_be_bytes());
                let byte_count = values.len().div_ceil(8);
                pdu.push(byte_count as u8);
                let mut packed = vec![0u8; byte_count];
                for (i, &on) in values.iter().enumerate() {
                    if on {
                        packed[i / 8] |= 1 << (i % 8);
                    }
                }
                pdu.extend_from_slice(&packed);
            }
            ModbusRequest::WriteMultipleRegisters { start, values } => {
                pdu.extend_from_slice(&start.to_be_bytes());
                pdu.extend_from_slice(&(values.len() as u16).to_be_bytes());
                pdu.push((values.len() * 2) as u8);
                for v in values {
                    pdu.extend_from_slice(&v.to_be_bytes());
                }
            }
        }
        pdu
    }

    proptest! {
        #![proptest_config(codec_config!(crate::CASES))]

        /// Property 1: the MBAP framing round-trips for every PDU the length field can carry.
        #[test]
        fn adu_round_trips(
            transaction_id in any::<u16>(),
            unit_id in any::<u8>(),
            pdu in proptest::collection::vec(any::<u8>(), 1..=MAX_PDU_LEN),
        ) {
            let bytes = encode_adu(transaction_id, unit_id, &pdu);
            let parsed = try_parse_adu(&bytes);
            prop_assert!(matches!(parsed, Ok(Some(_))), "{:?}", parsed);
            let Ok(Some((adu, consumed))) = parsed else { unreachable!() };
            prop_assert_eq!(consumed, bytes.len());
            prop_assert_eq!(adu, Adu { transaction_id, unit_id, pdu });
        }

        /// Property 2: every ADU this encoder produces from a legal PDU stays inside the
        /// MBAP maximum its own parser enforces.
        #[test]
        fn adu_output_is_bounded(
            transaction_id in any::<u16>(),
            unit_id in any::<u8>(),
            pdu in proptest::collection::vec(any::<u8>(), 0..=MAX_PDU_LEN),
        ) {
            let bytes = encode_adu(transaction_id, unit_id, &pdu);
            prop_assert!(bytes.len() <= MBAP_HEADER_LEN + MAX_PDU_LEN);
        }

        /// A partial ADU is `Ok(None)` — "read more" — and never an error or a panic.
        #[test]
        fn a_partial_adu_asks_for_more(
            transaction_id in any::<u16>(),
            unit_id in any::<u8>(),
            pdu in proptest::collection::vec(any::<u8>(), 1..=MAX_PDU_LEN),
            cut in 0usize..260,
        ) {
            let bytes = encode_adu(transaction_id, unit_id, &pdu);
            let cut = cut.min(bytes.len().saturating_sub(1));
            prop_assert_eq!(try_parse_adu(&bytes[..cut]), Ok(None));
        }

        /// Property 1 for the PDU layer: an independently encoded request parses back.
        #[test]
        fn request_round_trips(request in arb_request()) {
            let pdu = encode_request_pdu(&request);
            prop_assert_eq!(parse_request(&pdu), Ok(request));
        }

        /// Property 1 for the response layer: a write acknowledgement re-parses as the
        /// request it acknowledges, for FC 5 and 6 which echo the request verbatim.
        #[test]
        fn single_write_ack_echoes_the_request(
            address in any::<u16>(),
            on in any::<bool>(),
            value in any::<u16>(),
            coil in any::<bool>(),
        ) {
            let request = if coil {
                ModbusRequest::WriteSingleCoil { address, value: on }
            } else {
                ModbusRequest::WriteSingleRegister { address, value }
            };
            prop_assert_eq!(parse_request(&encode_write_ack(&request)), Ok(request));
        }

        /// Property 2: a read response never overflows the PDU the MBAP length field can
        /// carry, for every quantity `parse_request` accepts.
        #[test]
        fn read_responses_fit_a_legal_pdu(
            bits in proptest::collection::vec(any::<bool>(), 1..=2000),
            regs in proptest::collection::vec(any::<u16>(), 1..=125),
        ) {
            let bit_pdu = encode_bits_response(FC_READ_COILS, &bits);
            prop_assert!(bit_pdu.len() <= MAX_PDU_LEN, "bit PDU {} bytes", bit_pdu.len());
            prop_assert_eq!(bit_pdu[1] as usize, bits.len().div_ceil(8));

            let reg_pdu = encode_registers_response(FC_READ_HOLDING_REGISTERS, &regs);
            prop_assert!(reg_pdu.len() <= MAX_PDU_LEN, "register PDU {} bytes", reg_pdu.len());
            prop_assert_eq!(reg_pdu[1] as usize, regs.len() * 2);
        }

        /// Property 1: the bit packing is its own inverse — what `encode_bits_response`
        /// writes is what `parse_request` reads back out of a Write Multiple Coils body.
        #[test]
        fn bit_packing_round_trips(values in proptest::collection::vec(any::<bool>(), 1..=1968)) {
            let response = encode_bits_response(FC_READ_COILS, &values);
            let packed = &response[2..];
            let read_back: Vec<bool> = (0..values.len())
                .map(|i| packed[i / 8] & (1 << (i % 8)) != 0)
                .collect();
            prop_assert_eq!(read_back, values);
        }
    }

    proptest! {
        #![proptest_config(codec_config!(crate::PANIC_CASES))]

        /// Property 3.
        #[test]
        fn arbitrary_bytes_never_panic(data in proptest::collection::vec(any::<u8>(), 0..300)) {
            let _ = try_parse_adu(&data);
            let _ = parse_request(&data);
        }

        /// Property 3, biased: a well-formed MBAP header in front of arbitrary PDU bytes, so
        /// the framing check passes and the PDU parser is actually reached.
        #[test]
        fn framed_arbitrary_pdus_never_panic(
            transaction_id in any::<u16>(),
            unit_id in any::<u8>(),
            pdu in proptest::collection::vec(any::<u8>(), 0..=MAX_PDU_LEN),
        ) {
            let bytes = encode_adu(transaction_id, unit_id, &pdu);
            if let Ok(Some((adu, _))) = try_parse_adu(&bytes) {
                let _ = parse_request(&adu.pdu);
            }
        }

        /// Every function code outside the eight implemented ones is an illegal-function
        /// exception, never a panic, whatever follows it.
        #[test]
        fn unknown_function_codes_are_refused(
            fc in any::<u8>(),
            body in proptest::collection::vec(any::<u8>(), 0..32),
        ) {
            let known = [
                FC_READ_COILS, FC_READ_DISCRETE_INPUTS, FC_READ_HOLDING_REGISTERS,
                FC_READ_INPUT_REGISTERS, FC_WRITE_SINGLE_COIL, FC_WRITE_SINGLE_REGISTER,
                FC_WRITE_MULTIPLE_COILS, FC_WRITE_MULTIPLE_REGISTERS,
            ];
            prop_assume!(!known.contains(&fc));
            let mut pdu = vec![fc];
            pdu.extend_from_slice(&body);
            prop_assert_eq!(parse_request(&pdu), Err(1));
        }

        /// An exception PDU is two bytes and sets the high bit of the function code.
        #[test]
        fn exception_pdus_are_two_bytes(fc in any::<u8>(), code in any::<u8>()) {
            let pdu = encode_exception(fc, code);
            prop_assert_eq!(pdu.len(), 2);
            prop_assert_eq!(pdu[0], fc | 0x80);
            prop_assert_eq!(pdu[1], code);
        }
    }

    // -------------------------------------------------------------------------------------
    // FINDINGS
    // -------------------------------------------------------------------------------------

    /// FINDING: `encode_adu` bounds nothing, while `try_parse_adu` refuses any MBAP length
    /// outside `2..=254`. Minimal counterexample: a 254-byte PDU, which produces a length
    /// field of 255 that the codec's own parser rejects as `BadLength`. Beyond 65534 bytes
    /// the `(pdu.len() as u16) + 1` also overflows, which panics in every debug and test
    /// build (`Cargo.toml` has no `[profile.dev]`, so `overflow-checks` is on there).
    ///
    /// Not reachable through `mod.rs`, which bounds the model's answer to the quantity
    /// `parse_request` validated. Unguarded in a `pub fn`.
    #[test]
    #[ignore = "FINDING: encode_adu enforces no bound while try_parse_adu enforces 2..=254"]
    fn encode_adu_should_refuse_a_pdu_its_own_parser_would_reject() {
        let pdu = vec![0u8; MAX_PDU_LEN + 1];
        let bytes = encode_adu(0, 1, &pdu);
        assert!(
            try_parse_adu(&bytes).is_ok(),
            "encode_adu produced an ADU its own parser rejects"
        );
    }

    /// FINDING: `encode_registers_response` writes `(values.len() * 2) as u8`. At 128
    /// registers the byte count is 0 and 256 octets of data follow it — a silently corrupt
    /// frame rather than a refusal. `encode_bits_response` has the same shape at 2040 bits.
    #[test]
    #[ignore = "FINDING: encode_registers_response narrows the byte count with a bare `as u8`"]
    fn encode_registers_response_should_not_narrow_its_byte_count() {
        let values = vec![0u16; 128];
        let pdu = encode_registers_response(FC_READ_HOLDING_REGISTERS, &values);
        assert_eq!(
            pdu[1] as usize,
            values.len() * 2,
            "byte count wrapped: declared {} for {} octets of data",
            pdu[1],
            values.len() * 2
        );
    }
}

// ===========================================================================================
// CoAP — `src/server/coap/codec.rs`
// ===========================================================================================
//
// FINDINGS:
//
//  * `CoapMessage::encode` silently truncates a token longer than 8 bytes
//    (`self.token.len().min(8)`), while `decode` refuses `tkl > 8`. A model that supplies a
//    16-byte token gets a different token on the wire, and CoAP's whole request/response
//    matching is token equality.
//  * An option value longer than 65535 bytes has its length narrowed by `as u16`.

#[cfg(feature = "coap")]
mod coap_props {
    use netget::server::coap::codec::{
        code_to_string, parse_code_string, CoapMessage, MessageType, MAX_PAYLOAD_LEN,
    };
    use proptest::prelude::*;

    fn arb_message_type() -> impl Strategy<Value = MessageType> {
        prop_oneof![
            Just(MessageType::Confirmable),
            Just(MessageType::NonConfirmable),
            Just(MessageType::Acknowledgement),
            Just(MessageType::Reset),
        ]
    }

    /// Options as the wire requires them: ascending by number, repeats allowed.
    ///
    /// `encode` sorts before emitting, so an unsorted `options` vector is not a structurally
    /// valid message — it is one the encoder normalises. The sorted generator is what makes
    /// the round-trip property about the codec rather than about `sort_by_key`.
    fn arb_options() -> impl Strategy<Value = Vec<(u16, Vec<u8>)>> {
        proptest::collection::vec(
            (0u16..600, proptest::collection::vec(any::<u8>(), 0..40)),
            0..8,
        )
        .prop_map(|mut opts| {
            opts.sort_by_key(|(n, _)| *n);
            opts
        })
    }

    fn arb_message() -> impl Strategy<Value = CoapMessage> {
        (
            arb_message_type(),
            any::<u8>(),
            any::<u16>(),
            proptest::collection::vec(any::<u8>(), 0..=8),
            arb_options(),
            proptest::collection::vec(any::<u8>(), 0..64),
        )
            .prop_map(
                |(mtype, code, message_id, token, options, payload)| CoapMessage {
                    mtype,
                    code,
                    message_id,
                    token,
                    options,
                    payload,
                },
            )
    }

    proptest! {
        #![proptest_config(codec_config!(crate::CASES))]

        /// Property 1.
        #[test]
        fn message_round_trips(message in arb_message()) {
            let bytes = message.encode();
            let decoded = CoapMessage::decode(&bytes);
            prop_assert!(decoded.is_ok(), "{:?} from {:02x?}", decoded, bytes);
            prop_assert_eq!(decoded.unwrap(), message);
        }

        /// Property 1 for the delta/extension encoding specifically. Option numbers up to
        /// 65535 exercise all three nibble forms (0-12, 13 + one byte, 14 + two bytes),
        /// which is where every CoAP codec gets the `+13` / `+269` bias wrong.
        #[test]
        fn option_deltas_round_trip(
            numbers in proptest::collection::vec(any::<u16>(), 1..12),
        ) {
            let mut numbers = numbers;
            numbers.sort_unstable();
            let options: Vec<(u16, Vec<u8>)> =
                numbers.iter().map(|n| (*n, vec![0xAB; 3])).collect();
            let message = CoapMessage {
                mtype: MessageType::Confirmable,
                code: 1,
                message_id: 0x1234,
                token: vec![1, 2, 3, 4],
                options: options.clone(),
                payload: Vec::new(),
            };
            let decoded = CoapMessage::decode(&message.encode()).unwrap();
            prop_assert_eq!(decoded.options, options);
        }

        /// Property 2: a message built from values the protocol's own limits allow stays
        /// inside the datagram size the module documents.
        #[test]
        fn a_bounded_message_encodes_within_the_payload_ceiling(
            token in proptest::collection::vec(any::<u8>(), 0..=8),
            payload in proptest::collection::vec(any::<u8>(), 0..=MAX_PAYLOAD_LEN),
        ) {
            let message = CoapMessage {
                mtype: MessageType::Acknowledgement,
                code: 69,
                message_id: 7,
                token,
                options: vec![(12, vec![0])],
                payload,
            };
            prop_assert!(message.encode().len() <= 4 + 8 + 4 + MAX_PAYLOAD_LEN);
        }

        /// Property 4: the response-code text form normalises and round-trips.
        #[test]
        fn code_strings_round_trip(class in 0u8..8, detail in 0u8..32) {
            let code = (class << 5) | detail;
            let text = code_to_string(code);
            prop_assert_eq!(parse_code_string(&text), Some(code));
            prop_assert_eq!(code_to_string(code), text);
        }
    }

    proptest! {
        #![proptest_config(codec_config!(crate::PANIC_CASES))]

        /// Property 3.
        #[test]
        fn arbitrary_bytes_never_panic(data in proptest::collection::vec(any::<u8>(), 0..256)) {
            if let Ok(message) = CoapMessage::decode(&data) {
                // A message that decoded must also survive being described to the model.
                let _ = message.uri_path();
                let _ = message.uri_query();
                let _ = message.path_segments();
                let _ = message.encode();
            }
        }

        /// Property 3, biased: a valid header in front of arbitrary option bytes, so the
        /// version/token checks pass and the option walker is actually exercised.
        #[test]
        fn valid_header_with_arbitrary_options_never_panics(
            tkl in 0u8..=8,
            rest in proptest::collection::vec(any::<u8>(), 0..128),
        ) {
            let mut data = vec![(1u8 << 6) | tkl, 0x01, 0x00, 0x01];
            data.extend(std::iter::repeat_n(0u8, tkl as usize));
            data.extend_from_slice(&rest);
            let _ = CoapMessage::decode(&data);
        }
    }

    // -------------------------------------------------------------------------------------
    // FINDINGS
    // -------------------------------------------------------------------------------------

    /// FINDING: `encode` truncates an over-long token instead of refusing it. Minimal
    /// counterexample: a 9-byte token, which reaches the wire as its first 8 bytes.
    /// `decode` refuses `tkl > 8`, so the two directions disagree about what is legal —
    /// and silently, because truncating still produces a parseable message.
    #[test]
    #[ignore = "FINDING: CoapMessage::encode truncates a token longer than 8 bytes"]
    fn encode_should_refuse_an_over_long_token() {
        let message = CoapMessage {
            mtype: MessageType::Confirmable,
            code: 1,
            message_id: 1,
            token: vec![1, 2, 3, 4, 5, 6, 7, 8, 9],
            options: Vec::new(),
            payload: Vec::new(),
        };
        let decoded = CoapMessage::decode(&message.encode()).unwrap();
        assert_eq!(
            decoded.token, message.token,
            "token was silently truncated on encode"
        );
    }
}

// ===========================================================================================
// STOMP — `src/server/stomp/frame.rs`
// ===========================================================================================

#[cfg(feature = "stomp")]
mod stomp_props {
    use netget::server::stomp::frame::{
        escape_header, is_safe_unescaped_header, parse_frame, should_escape, unescape_header,
        ParseOutcome, StompFrame, MAX_FRAME_BYTES,
    };
    use proptest::prelude::*;

    /// A command whose headers are escaped — i.e. anything but the three 1.0/1.1-compatible
    /// commands, which carry their headers raw and therefore cannot represent a `:` at all.
    fn arb_escaping_command() -> impl Strategy<Value = String> {
        "[A-Z]{1,12}".prop_filter("CONNECT/STOMP/CONNECTED carry raw headers", |c| {
            should_escape(c)
        })
    }

    /// Header names and values are arbitrary text: escaping is exactly what makes `\r`, `\n`,
    /// `:` and `\` expressible, so restricting the generator would test nothing.
    ///
    /// `content-length` is excluded as a *name* only: `encode` adds one itself for a non-empty
    /// body, and a caller-supplied one that disagrees with the body is a different property.
    fn arb_headers() -> impl Strategy<Value = Vec<(String, String)>> {
        proptest::collection::vec(
            (
                ".{0,12}".prop_filter("encode appends its own content-length", |n: &String| {
                    n != "content-length"
                }),
                ".{0,20}",
            ),
            0..6,
        )
    }

    proptest! {
        #![proptest_config(codec_config!(crate::CASES))]

        /// Property 1: header escaping is exactly invertible, for any text at all.
        #[test]
        fn header_escaping_round_trips(s in ".{0,64}") {
            prop_assert_eq!(unescape_header(&escape_header(&s)), Ok(s));
        }

        /// Escaping never leaves a bare `:` or newline behind — that is the forgery the
        /// escape exists to prevent, and it is what makes the frame parser's `split_once(':')`
        /// unambiguous.
        #[test]
        fn escaping_neutralises_every_separator(s in ".{0,64}") {
            let escaped = escape_header(&s);
            let mut chars = escaped.chars().peekable();
            while let Some(c) = chars.next() {
                if c == '\\' {
                    chars.next();
                    continue;
                }
                prop_assert!(
                    !matches!(c, ':' | '\r' | '\n'),
                    "unescaped separator {:?} survived in {:?}",
                    c,
                    escaped
                );
            }
        }

        /// Property 1 for the whole frame.
        ///
        /// `encode` adds a `content-length` header for a non-empty body that had none, so the
        /// decoded header list is the caller's followed by at most that one. Asserting a
        /// prefix rather than equality is the honest statement of what the codec promises.
        #[test]
        fn frame_round_trips(
            command in arb_escaping_command(),
            headers in arb_headers(),
            body in proptest::collection::vec(any::<u8>(), 0..64),
        ) {
            let frame = StompFrame { command, headers, body };
            let bytes = frame.encode();
            let outcome = parse_frame(&bytes);
            prop_assert!(matches!(outcome, Ok(ParseOutcome::Frame { .. })), "{:?}", outcome);
            let Ok(ParseOutcome::Frame { frame: decoded, consumed }) = outcome else {
                unreachable!()
            };
            prop_assert_eq!(consumed, bytes.len());
            prop_assert_eq!(&decoded.command, &frame.command);
            prop_assert_eq!(&decoded.body, &frame.body);
            prop_assert_eq!(
                &decoded.headers[..frame.headers.len()],
                &frame.headers[..]
            );
            if frame.body.is_empty() {
                prop_assert_eq!(decoded.headers.len(), frame.headers.len());
            } else {
                prop_assert_eq!(decoded.headers.len(), frame.headers.len() + 1);
                let declared = frame.body.len().to_string();
                prop_assert_eq!(decoded.header("content-length"), Some(declared.as_str()));
            }
        }

        /// A body containing NUL bytes survives, because `encode` writes the `content-length`
        /// that makes the NUL terminator unambiguous. This is the one thing the auto-added
        /// header is for, so it gets its own property.
        #[test]
        fn a_body_full_of_nuls_round_trips(
            command in arb_escaping_command(),
            body in proptest::collection::vec(prop_oneof![Just(0u8), any::<u8>()], 1..48),
        ) {
            let frame = StompFrame { command, headers: Vec::new(), body };
            let Ok(ParseOutcome::Frame { frame: decoded, .. }) = parse_frame(&frame.encode())
            else {
                return Err(TestCaseError::fail("frame did not parse"));
            };
            prop_assert_eq!(decoded.body, frame.body);
        }

        /// Property 2: the encoder's output is the command, the headers, the body and a
        /// fixed overhead — never more. A frame built from bounded parts cannot approach
        /// `MAX_FRAME_BYTES`, which is what the parser refuses.
        #[test]
        fn encoded_size_tracks_its_input(
            command in arb_escaping_command(),
            headers in arb_headers(),
            body in proptest::collection::vec(any::<u8>(), 0..64),
        ) {
            let frame = StompFrame { command, headers, body };
            let bytes = frame.encode();
            prop_assert!(bytes.len() < MAX_FRAME_BYTES);
        }

        /// Any prefix of a frame is `Incomplete` — never a frame, never an error. A parser
        /// that accepted a prefix would desynchronise the stream, which STOMP cannot recover
        /// from.
        #[test]
        fn a_prefix_of_a_frame_is_incomplete(
            command in arb_escaping_command(),
            headers in arb_headers(),
            body in proptest::collection::vec(any::<u8>(), 0..32),
            cut in 0usize..400,
        ) {
            let frame = StompFrame { command, headers, body };
            let bytes = frame.encode();
            let cut = cut.min(bytes.len().saturating_sub(1));
            prop_assert_eq!(parse_frame(&bytes[..cut]), Ok(ParseOutcome::Incomplete));
        }

        /// The three escaping-exempt commands carry their headers raw, so `is_safe_unescaped_header`
        /// is the *only* thing standing between a header value and a forged header. It must
        /// agree with what the parser then reads back.
        #[test]
        fn exempt_commands_round_trip_safe_headers(
            command in prop::sample::select(vec!["CONNECT", "STOMP", "CONNECTED"]),
            headers in proptest::collection::vec((".{1,10}", ".{0,10}"), 0..4),
        ) {
            let headers: Vec<(String, String)> = headers
                .into_iter()
                .filter(|(k, v)| {
                    is_safe_unescaped_header(k)
                        && is_safe_unescaped_header(v)
                        && k != "content-length"
                })
                .collect();
            let frame = StompFrame {
                command: command.to_string(),
                headers,
                body: Vec::new(),
            };
            let Ok(ParseOutcome::Frame { frame: decoded, .. }) = parse_frame(&frame.encode())
            else {
                return Err(TestCaseError::fail("frame did not parse"));
            };
            prop_assert_eq!(decoded.headers, frame.headers);
        }
    }

    proptest! {
        #![proptest_config(codec_config!(crate::PANIC_CASES))]

        /// Property 3.
        #[test]
        fn arbitrary_bytes_never_panic(data in proptest::collection::vec(any::<u8>(), 0..512)) {
            let _ = parse_frame(&data);
        }

        /// Property 3, biased: a frame-shaped stream built from STOMP's own metacharacters,
        /// which is where the `content-length` arithmetic and the EOL drain live. The
        /// `content-length: 18446744073709551615` overflow that once panicked every test
        /// build is exactly this shape.
        #[test]
        fn stomp_metacharacter_soup_never_panics(
            data in proptest::collection::vec(
                prop::sample::select(vec![
                    b'\n', b'\r', b':', b'\\', b'\0', b'c', b'o', b'n', b't', b'e', b'-',
                    b'l', b'g', b'h', b'0', b'1', b'9', b'A',
                ]),
                0..200,
            )
        ) {
            let _ = parse_frame(&data);
        }

        /// A declared `content-length` larger than the frame ceiling is refused before any
        /// arithmetic is done on it — the "bound the declared size, not the remainder" rule.
        #[test]
        fn an_absurd_content_length_is_refused(len in (MAX_FRAME_BYTES as u64 + 1)..=u64::MAX) {
            let raw = format!("SEND\ncontent-length:{len}\n\n\0");
            prop_assert!(parse_frame(raw.as_bytes()).is_err());
        }
    }
}

// ===========================================================================================
// M3UA — `src/server/m3ua/codec.rs`
// ===========================================================================================
//
// The motivating case. `parse_header` refuses a declared length above `MAX_MESSAGE_LEN`;
// `Message::encode` applies no bound at all and writes `total as u32`. `Parameter::write_into`
// is worse: it writes `declared as u16`, so a parameter value of 65532 bytes or more has its
// length silently truncated and the whole body after it is misread.

#[cfg(feature = "m3ua")]
mod m3ua_props {
    use netget::server::m3ua::codec::{
        padding_for, parse_header, parse_parameters, peek_class_type, Message, Parameter,
        HEADER_LEN, MAX_MESSAGE_LEN,
    };
    use proptest::prelude::*;

    /// A parameter whose value fits its 16-bit length field, with room for the 4-octet TLV
    /// header — which is what "structurally valid" means here.
    fn arb_parameter() -> impl Strategy<Value = Parameter> {
        (any::<u16>(), proptest::collection::vec(any::<u8>(), 0..64))
            .prop_map(|(tag, value)| Parameter { tag, value })
    }

    fn arb_message() -> impl Strategy<Value = Message> {
        (
            any::<u8>(),
            any::<u8>(),
            proptest::collection::vec(arb_parameter(), 0..8),
        )
            .prop_map(|(class, msg_type, parameters)| Message {
                class,
                msg_type,
                parameters,
            })
    }

    proptest! {
        #![proptest_config(codec_config!(crate::CASES))]

        /// Property 1.
        #[test]
        fn message_round_trips(message in arb_message()) {
            let bytes = message.encode();
            let parsed = Message::parse(&bytes);
            prop_assert!(parsed.is_ok(), "{:?}", parsed);
            prop_assert_eq!(parsed.unwrap(), message);
        }

        /// The declared Message Length is the whole message, padding included — the thing the
        /// reader in `mod.rs` slices on.
        #[test]
        fn declared_length_is_the_encoded_length(message in arb_message()) {
            let bytes = message.encode();
            let header = parse_header(&bytes).unwrap();
            prop_assert_eq!(header.length as usize, bytes.len());
            prop_assert_eq!(peek_class_type(&bytes), Some((message.class, message.msg_type)));
        }

        /// Every parameter is padded to a 4-octet boundary and the padding is *not* in the
        /// declared length (RFC 4666 §3.2). Both halves of that sentence are load-bearing and
        /// both get asserted.
        #[test]
        fn parameters_are_padded_but_the_padding_is_not_declared(
            parameters in proptest::collection::vec(arb_parameter(), 1..6),
        ) {
            let mut body = Vec::new();
            for parameter in &parameters {
                parameter.write_into(&mut body);
                prop_assert_eq!(body.len() % 4, 0);
            }
            prop_assert_eq!(&parse_parameters(&body).unwrap(), &parameters);
            for parameter in &parameters {
                prop_assert_eq!(parameter.declared_len(), 4 + parameter.value.len());
                prop_assert_eq!(
                    parameter.wire_len(),
                    parameter.declared_len() + padding_for(parameter.declared_len())
                );
            }
        }

        /// Property 2, stated for the range the encoder actually handles correctly.
        ///
        /// See the FINDING below for what happens above it.
        #[test]
        fn a_bounded_message_encodes_within_the_ceiling(
            parameters in proptest::collection::vec(
                (any::<u16>(), proptest::collection::vec(any::<u8>(), 0..400))
                    .prop_map(|(tag, value)| Parameter { tag, value }),
                0..40,
            ),
        ) {
            let message = Message { class: 1, msg_type: 1, parameters };
            let bytes = message.encode();
            prop_assume!(bytes.len() <= MAX_MESSAGE_LEN);
            prop_assert!(Message::parse(&bytes).is_ok());
        }
    }

    proptest! {
        #![proptest_config(codec_config!(crate::PANIC_CASES))]

        /// Property 3.
        #[test]
        fn arbitrary_bytes_never_panic(data in proptest::collection::vec(any::<u8>(), 0..256)) {
            let _ = parse_header(&data);
            let _ = parse_parameters(&data);
            let _ = Message::parse(&data);
            let _ = peek_class_type(&data);
        }

        /// Property 3, biased: a valid common header in front of arbitrary TLV bytes, so the
        /// version and length checks pass and the parameter walker is reached.
        #[test]
        fn valid_header_with_arbitrary_parameters_never_panics(
            class in any::<u8>(),
            msg_type in any::<u8>(),
            body in proptest::collection::vec(any::<u8>(), 0..128),
        ) {
            let total = (HEADER_LEN + body.len()) as u32;
            let mut data = vec![1u8, 0, class, msg_type];
            data.extend_from_slice(&total.to_be_bytes());
            data.extend_from_slice(&body);
            let _ = Message::parse(&data);
        }
    }

    // -------------------------------------------------------------------------------------
    // FINDINGS
    // -------------------------------------------------------------------------------------

    /// FINDING (the motivating case). `Message::encode` applies no bound, so a message whose
    /// parameters exceed `MAX_MESSAGE_LEN` encodes happily and is then rejected by the
    /// codec's own `parse_header`. Minimal counterexample: 1024 parameters of 64 value bytes
    /// each (70 KiB), which is a routine `send_m3ua_data` answer with a large payload list.
    #[test]
    #[ignore = "FINDING: Message::encode enforces no MAX_MESSAGE_LEN; parse_header does"]
    fn encode_should_refuse_a_message_its_own_parser_would_reject() {
        let parameters = (0..1024)
            .map(|i| Parameter {
                tag: i as u16,
                value: vec![0u8; 64],
            })
            .collect();
        let message = Message {
            class: 1,
            msg_type: 1,
            parameters,
        };
        let bytes = message.encode();
        assert!(bytes.len() > MAX_MESSAGE_LEN);
        assert!(
            Message::parse(&bytes).is_ok(),
            "encode produced {} octets, past the {MAX_MESSAGE_LEN} its own parser enforces",
            bytes.len()
        );
    }

    /// FINDING, and the worse half. `Parameter::write_into` writes `(declared as u16)`. A
    /// value of 65532 bytes makes `declared` 65536, which narrows to 0 — below the 4-octet
    /// TLV header — so the parameter is not merely oversized, it is unparseable, and every
    /// parameter after it is lost. Silent in release, silent in debug (the cast does not
    /// overflow-check), and the only symptom is a peer that stops making sense.
    #[test]
    #[ignore = "FINDING: Parameter::write_into narrows its declared length with `as u16`"]
    fn parameter_length_should_not_narrow() {
        let parameter = Parameter {
            tag: 0x0210,
            value: vec![0u8; 65_532],
        };
        let mut body = Vec::new();
        parameter.write_into(&mut body);
        let declared = u16::from_be_bytes([body[2], body[3]]);
        assert_eq!(
            declared as usize,
            parameter.declared_len(),
            "declared length wrapped to {declared}"
        );
    }
}

// ===========================================================================================
// GTP — `src/server/gtp/codec.rs`
// ===========================================================================================
//
// FINDING: `encode_apn` truncates a label longer than 63 octets (`bytes.len().min(63)`)
// instead of refusing it, so `decode_apn(encode_apn(x)) != x`. `netbios_ns`'s
// `encode_name_field` is the same situation and bails; this one does not.

#[cfg(feature = "gtp")]
mod gtp_props {
    use netget::server::gtp::codec::{
        decode_apn, decode_end_user_address, decode_fteid, decode_paa, decode_tbcd, encode_apn,
        encode_end_user_address, encode_fteid, encode_paa, encode_tbcd, encode_v1_ies,
        parse_v1_ies, peek_version, GtpV1Ie,
    };
    use proptest::prelude::*;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    fn arb_ip() -> impl Strategy<Value = IpAddr> {
        prop_oneof![
            any::<[u8; 4]>().prop_map(|o| IpAddr::V4(Ipv4Addr::from(o))),
            any::<[u8; 16]>().prop_map(|o| IpAddr::V6(Ipv6Addr::from(o))),
        ]
    }

    proptest! {
        #![proptest_config(codec_config!(crate::CASES))]

        /// Property 1: the GTPv1 information-element TLV walker.
        #[test]
        fn v1_ies_round_trip(
            ies in proptest::collection::vec(
                // Type 128 and above is the variable-length form, which carries its own
                // 16-bit length. Below 128 the length is a fixed table lookup, so an
                // arbitrary value would not be a structurally valid IE.
                (128u8..=255, proptest::collection::vec(any::<u8>(), 0..48))
                    .prop_map(|(ie_type, value)| GtpV1Ie { ie_type, value }),
                0..8,
            ),
        ) {
            let body = encode_v1_ies(&ies);
            prop_assert_eq!(parse_v1_ies(&body).unwrap(), ies);
        }

        /// Property 1: TBCD, low nibble first, `0xF`-padded on an odd count.
        #[test]
        fn tbcd_round_trips(digits in "[0-9]{0,20}") {
            prop_assert_eq!(decode_tbcd(&encode_tbcd(&digits)), digits);
        }

        /// Property 4: TBCD *normalises* — it drops everything that is not a digit — and the
        /// normalisation is idempotent. `f(f(x)) == f(x)` is the whole statement here.
        #[test]
        fn tbcd_normalisation_is_idempotent(raw in ".{0,24}") {
            let once = decode_tbcd(&encode_tbcd(&raw));
            let twice = decode_tbcd(&encode_tbcd(&once));
            prop_assert_eq!(twice, once);
        }

        /// Property 1: APN labels, for labels the format can express.
        #[test]
        fn apn_round_trips(
            labels in proptest::collection::vec("[a-z0-9-]{1,63}", 1..5),
        ) {
            let apn = labels.join(".");
            prop_assert_eq!(decode_apn(&encode_apn(&apn).unwrap()), Some(apn));
        }

        /// Property 4: APN encoding normalises away empty labels, idempotently.
        #[test]
        fn apn_normalisation_is_idempotent(raw in "[a-z0-9.-]{0,40}") {
            let Some(once) = decode_apn(&encode_apn(&raw).unwrap()) else { return Ok(()); };
            prop_assert_eq!(decode_apn(&encode_apn(&once).unwrap()), Some(once));
        }

        /// Property 1: the GTPv2 F-TEID. `interface_type` is six bits on the wire and both
        /// directions mask it, so the generator respects the six bits.
        #[test]
        fn fteid_round_trips(interface_type in 0u8..64, teid in any::<u32>(), addr in arb_ip()) {
            let value = encode_fteid(interface_type, teid, addr);
            prop_assert_eq!(decode_fteid(&value), Some((interface_type, teid, Some(addr))));
        }

        /// Property 1: the GTPv2 PDN Address Allocation IE.
        #[test]
        fn paa_round_trips(addr in arb_ip()) {
            let (name, decoded) = decode_paa(&encode_paa(addr));
            prop_assert_eq!(decoded, Some(addr));
            prop_assert_eq!(name, if addr.is_ipv4() { "IPv4" } else { "IPv6" });
        }

        /// Property 1: the GTPv1 End User Address IE, including the "give me a dynamic one"
        /// form, which carries a type and no address.
        #[test]
        fn end_user_address_round_trips(addr in proptest::option::of(arb_ip())) {
            let (name, decoded) = decode_end_user_address(&encode_end_user_address(addr));
            prop_assert_eq!(decoded, addr);
            match addr {
                Some(IpAddr::V6(_)) => prop_assert_eq!(name, "IPv6"),
                _ => prop_assert_eq!(name, "IPv4"),
            }
        }
    }

    proptest! {
        #![proptest_config(codec_config!(crate::PANIC_CASES))]

        /// Property 3, across every decoder in the module that takes raw bytes.
        #[test]
        fn arbitrary_bytes_never_panic(data in proptest::collection::vec(any::<u8>(), 0..256)) {
            let _ = parse_v1_ies(&data);
            let _ = decode_tbcd(&data);
            let _ = decode_apn(&data);
            let _ = decode_fteid(&data);
            let _ = decode_paa(&data);
            let _ = decode_end_user_address(&data);
            let _ = peek_version(&data);
        }
    }

    // -------------------------------------------------------------------------------------
    // FINDINGS
    // -------------------------------------------------------------------------------------

    /// `encode_apn` used to silently truncate a label past 63 octets rather than refusing.
    /// Minimal counterexample: a single label of 64 `a`s, which reached the wire as 63 and
    /// named a different access point — a perfectly valid-looking one, which is what makes
    /// truncation worse than a refusal here. The DNS label length is a hard format limit, so
    /// nothing downstream could have recovered the intended name.
    #[test]
    fn encode_apn_refuses_an_over_long_label() {
        assert!(encode_apn(&"a".repeat(64)).is_err());
        // Refused wherever in the name it sits, not just first.
        assert!(encode_apn(&format!("internet.{}.epc", "a".repeat(64))).is_err());

        // The boundary: 63 is legal and round-trips.
        let legal = "a".repeat(63);
        assert_eq!(
            decode_apn(&encode_apn(&legal).unwrap()),
            Some(legal.clone())
        );
    }
}

// ===========================================================================================
// HSRP — `src/server/hsrp/codec.rs`
// ===========================================================================================

#[cfg(feature = "hsrp")]
mod hsrp_props {
    use netget::server::hsrp::codec::{
        decode, encode, HsrpMessage, HsrpState, HsrpVersion, Opcode, V1_LEN,
    };
    use proptest::prelude::*;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    fn arb_opcode() -> impl Strategy<Value = Opcode> {
        prop_oneof![
            Just(Opcode::Hello),
            Just(Opcode::Coup),
            Just(Opcode::Resign)
        ]
    }

    fn arb_state() -> impl Strategy<Value = HsrpState> {
        prop_oneof![
            Just(HsrpState::Initial),
            Just(HsrpState::Learn),
            Just(HsrpState::Listen),
            Just(HsrpState::Speak),
            Just(HsrpState::Standby),
            Just(HsrpState::Active),
        ]
    }

    /// The plaintext authentication field: at most eight bytes, no control characters
    /// (`encode_auth_field` refuses those), and never the empty string — an empty auth string
    /// and "no auth" are the same eight zero octets in v1, so `Some("")` is not a value the
    /// wire can distinguish.
    fn arb_auth() -> impl Strategy<Value = Option<String>> {
        proptest::option::of("[ -~]{1,8}")
    }

    /// HSRPv1: a flat 20-byte struct, every field one octet.
    fn arb_v1() -> impl Strategy<Value = HsrpMessage> {
        (
            arb_opcode(),
            arb_state(),
            any::<u8>(),
            any::<u8>(),
            any::<u8>(),
            any::<u8>(),
            arb_auth(),
            any::<[u8; 4]>(),
        )
            .prop_map(
                |(opcode, state, hello, hold, priority, group, auth_data, ip)| HsrpMessage {
                    version: HsrpVersion::V1,
                    opcode,
                    state,
                    hellotime_secs: hello as u32,
                    holdtime_secs: hold as u32,
                    priority: priority as u32,
                    group: group as u16,
                    auth_data,
                    virtual_ip: IpAddr::V4(Ipv4Addr::from(ip)),
                    // v1 has no identifier field, and `decode_v1` reports zeros.
                    identifier: [0u8; 6],
                    md5_auth: None,
                },
            )
    }

    /// HSRPv2: TLVs. The group is twelve bits and the timers are a 32-bit millisecond field,
    /// so the generator bounds both rather than spending its cases on the refusal path.
    fn arb_v2() -> impl Strategy<Value = HsrpMessage> {
        (
            arb_opcode(),
            arb_state(),
            0u32..4_294_967u32,
            0u32..4_294_967u32,
            any::<u32>(),
            0u16..=4095u16,
            arb_auth(),
            prop_oneof![
                any::<[u8; 4]>().prop_map(|o| IpAddr::V4(Ipv4Addr::from(o))),
                any::<[u8; 16]>().prop_map(|o| IpAddr::V6(Ipv6Addr::from(o))),
            ],
            any::<[u8; 6]>(),
        )
            .prop_map(
                |(
                    opcode,
                    state,
                    hellotime_secs,
                    holdtime_secs,
                    priority,
                    group,
                    auth_data,
                    virtual_ip,
                    identifier,
                )| HsrpMessage {
                    version: HsrpVersion::V2,
                    opcode,
                    state,
                    hellotime_secs,
                    holdtime_secs,
                    priority,
                    group,
                    auth_data,
                    virtual_ip,
                    identifier,
                    // Parse-direction only: `encode_v2` never writes an MD5 TLV.
                    md5_auth: None,
                },
            )
    }

    proptest! {
        #![proptest_config(codec_config!(crate::CASES))]

        /// Property 1 for HSRPv1.
        #[test]
        fn v1_round_trips(message in arb_v1()) {
            let bytes = encode(&message).unwrap();
            prop_assert_eq!(bytes.len(), V1_LEN);
            prop_assert_eq!(decode(&bytes).unwrap(), message);
        }

        /// Property 1 for HSRPv2.
        #[test]
        fn v2_round_trips(message in arb_v2()) {
            let bytes = encode(&message).unwrap();
            prop_assert_eq!(decode(&bytes).unwrap(), message);
        }

        /// The two versions are told apart by their first octet and never confused. This is
        /// the module's own stated "single most dangerous thing": state code 4 is Speak in v1
        /// and Standby in v2, so a message decoded as the wrong version mis-reports the
        /// election silently rather than failing.
        #[test]
        fn the_two_versions_never_decode_as_each_other(v1 in arb_v1(), v2 in arb_v2()) {
            prop_assert_eq!(decode(&encode(&v1).unwrap()).unwrap().version, HsrpVersion::V1);
            prop_assert_eq!(decode(&encode(&v2).unwrap()).unwrap().version, HsrpVersion::V2);
        }

        /// Reserved and out-of-range values are refused rather than truncated, which is the
        /// other half of a generator that only produces valid ones.
        #[test]
        fn v1_refuses_fields_that_do_not_fit_one_octet(over in 256u32..100_000) {
            let base = HsrpMessage {
                version: HsrpVersion::V1,
                opcode: Opcode::Hello,
                state: HsrpState::Active,
                hellotime_secs: 3,
                holdtime_secs: 10,
                priority: 100,
                group: 1,
                auth_data: Some("cisco".to_string()),
                virtual_ip: IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
                identifier: [0u8; 6],
                md5_auth: None,
            };
            // Bound each result first: `prop_assert!` stringifies its expression into a
            // format string, so a struct literal inside it is a compile error.
            let priority = encode(&HsrpMessage { priority: over, ..base.clone() });
            let hello = encode(&HsrpMessage { hellotime_secs: over, ..base.clone() });
            let hold = encode(&HsrpMessage { holdtime_secs: over, ..base.clone() });
            let group = encode(&HsrpMessage {
                group: over.min(u16::MAX as u32) as u16,
                ..base.clone()
            });
            let ipv6 = encode(&HsrpMessage {
                virtual_ip: IpAddr::V6(Ipv6Addr::LOCALHOST),
                ..base
            });
            prop_assert!(priority.is_err());
            prop_assert!(hello.is_err());
            prop_assert!(hold.is_err());
            prop_assert!(group.is_err());
            prop_assert!(ipv6.is_err());
        }

        /// A control character in the authentication string is refused, not stripped: the
        /// field is interpolated unquoted into this protocol's own log line.
        #[test]
        fn a_control_character_in_auth_is_refused(
            prefix in "[ -~]{0,3}",
            control in prop::sample::select(vec!['\n', '\r', '\t', '\0', '\u{7f}']),
        ) {
            let message = HsrpMessage {
                version: HsrpVersion::V1,
                opcode: Opcode::Hello,
                state: HsrpState::Active,
                hellotime_secs: 3,
                holdtime_secs: 10,
                priority: 100,
                group: 1,
                auth_data: Some(format!("{prefix}{control}")),
                virtual_ip: IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)),
                identifier: [0u8; 6],
                md5_auth: None,
            };
            prop_assert!(encode(&message).is_err());
        }
    }

    proptest! {
        #![proptest_config(codec_config!(crate::PANIC_CASES))]

        /// Property 3.
        #[test]
        fn arbitrary_bytes_never_panic(data in proptest::collection::vec(any::<u8>(), 0..128)) {
            let _ = decode(&data);
        }

        /// Property 3, biased: a v2 TLV chain built from plausible type/length octets, which
        /// is where a length that runs past the end of the datagram would bite.
        #[test]
        fn arbitrary_v2_tlv_chains_never_panic(
            data in proptest::collection::vec(
                prop::sample::select(vec![1u8, 2, 3, 4, 40, 28, 8, 0, 255]),
                0..80,
            )
        ) {
            prop_assume!(data.first() != Some(&0));
            let _ = decode(&data);
        }
    }
}

// ===========================================================================================
// VRRP — `src/server/vrrp/codec.rs`
// ===========================================================================================

#[cfg(feature = "vrrp")]
mod vrrp_props {
    use netget::server::vrrp::codec::{
        checksum_is_valid, internet_checksum, Advertisement, PseudoHeader, Variant,
        VrrpAdvertisement, VRRP_VERSION_2, VRRP_VERSION_3,
    };
    use proptest::prelude::*;
    use std::net::Ipv4Addr;

    fn arb_v4() -> impl Strategy<Value = Ipv4Addr> {
        any::<[u8; 4]>().prop_map(Ipv4Addr::from)
    }

    /// VRRPv2: the interval is whole seconds in one octet, so only 1..=255 is expressible.
    fn arb_v2() -> impl Strategy<Value = VrrpAdvertisement> {
        (
            1u8..=255,
            any::<u8>(),
            1u16..=255,
            any::<u8>(),
            proptest::collection::vec(arb_v4(), 0..8),
        )
            .prop_map(|(vrid, priority, seconds, auth_type, addresses)| {
                VrrpAdvertisement {
                    version: VRRP_VERSION_2,
                    vrid,
                    priority,
                    advert_interval_seconds: seconds as f64,
                    auth_type,
                    addresses,
                    checksum: 0,
                }
            })
    }

    /// VRRPv3: the interval is centiseconds in twelve bits. Generating from the wire value
    /// and dividing is what makes the round trip exact — an arbitrary `f64` of seconds has no
    /// representation and would be testing `round()`.
    fn arb_v3() -> impl Strategy<Value = VrrpAdvertisement> {
        (
            1u8..=255,
            any::<u8>(),
            1u16..=0x0fff,
            proptest::collection::vec(arb_v4(), 0..8),
        )
            .prop_map(
                |(vrid, priority, centiseconds, addresses)| VrrpAdvertisement {
                    version: VRRP_VERSION_3,
                    vrid,
                    priority,
                    advert_interval_seconds: centiseconds as f64 / 100.0,
                    // RFC 5798 §5.2.5: those four bits are reserved and must be zero.
                    auth_type: 0,
                    addresses,
                    checksum: 0,
                },
            )
    }

    proptest! {
        #![proptest_config(codec_config!(crate::CASES))]

        /// Property 1 for VRRPv2. `checksum` is the one field `encode` recomputes rather than
        /// carrying, so it is compared against what the encoder wrote rather than against the
        /// input — and the packet is additionally checked to verify in place, which is what a
        /// real peer does.
        #[test]
        fn v2_round_trips(advertisement in arb_v2()) {
            let bytes = advertisement.encode(None).unwrap();
            prop_assert!(checksum_is_valid(&bytes, None));
            let decoded = VrrpAdvertisement::decode(&bytes).unwrap();
            prop_assert_eq!(
                VrrpAdvertisement { checksum: 0, ..decoded },
                advertisement
            );
        }

        /// Property 1 for VRRPv3, whose checksum covers an IPv4 pseudo-header — so the same
        /// message to two destinations carries two different checksums, and both must verify.
        #[test]
        fn v3_round_trips(
            advertisement in arb_v3(),
            source in arb_v4(),
            destination in arb_v4(),
        ) {
            let pseudo = PseudoHeader::new(source, destination);
            let bytes = advertisement.encode(Some(&pseudo)).unwrap();
            prop_assert!(checksum_is_valid(&bytes, Some(&pseudo)));
            let decoded = VrrpAdvertisement::decode(&bytes).unwrap();
            prop_assert_eq!(
                VrrpAdvertisement { checksum: 0, ..decoded },
                advertisement
            );
        }

        /// A VRRPv3 advertisement encoded for one destination must **not** verify against
        /// another. Without this the pseudo-header could be ignored entirely and every other
        /// property here would still pass.
        #[test]
        fn v3_checksums_are_destination_specific(
            advertisement in arb_v3(),
            source in arb_v4(),
            a in arb_v4(),
            b in arb_v4(),
        ) {
            prop_assume!(a != b);
            let bytes = advertisement.encode(Some(&PseudoHeader::new(source, a))).unwrap();
            prop_assume!(!checksum_is_valid(&bytes, Some(&PseudoHeader::new(source, b))));
            prop_assert!(checksum_is_valid(&bytes, Some(&PseudoHeader::new(source, a))));
        }

        /// RFC 1071's defining property: the checksum of a buffer that already carries its
        /// own correct checksum is zero.
        #[test]
        fn internet_checksum_is_self_cancelling(
            data in proptest::collection::vec(any::<u8>(), 2..64),
        ) {
            let mut buffer = data.clone();
            buffer[0] = 0;
            buffer[1] = 0;
            let sum = internet_checksum(&buffer);
            buffer[0..2].copy_from_slice(&sum.to_be_bytes());
            prop_assert_eq!(internet_checksum(&buffer), 0);
        }

        /// Property 2: a v3 encode with no pseudo-header is an error, never a message-only
        /// checksum. A silently-wrong checksum is discarded by every conformant peer and is
        /// indistinguishable from this server being down.
        #[test]
        fn v3_refuses_to_encode_without_a_pseudo_header(advertisement in arb_v3()) {
            prop_assert!(advertisement.encode(None).is_err());
        }

        /// The values the format cannot express are refused rather than rounded. VRID 0 is
        /// not a virtual router; a v2 interval is whole seconds; a v3 interval is at most
        /// 40.95s; v3 has no authentication-type field.
        #[test]
        fn out_of_range_values_are_refused(
            fractional in 1u32..99,
            over in 4096u32..100_000,
            auth_type in 1u8..=255,
        ) {
            let v2 = VrrpAdvertisement {
                version: VRRP_VERSION_2,
                vrid: 1,
                priority: 100,
                advert_interval_seconds: 1.0,
                auth_type: 0,
                addresses: vec![Ipv4Addr::new(10, 0, 0, 1)],
                checksum: 0,
            };
            // Bound each result first: `prop_assert!` stringifies its expression into a
            // format string, so a struct literal inside it is a compile error.
            let vrid_zero = VrrpAdvertisement { vrid: 0, ..v2.clone() }.encode(None);
            let fractional_v2 = VrrpAdvertisement {
                advert_interval_seconds: 1.0 + fractional as f64 / 100.0,
                ..v2.clone()
            }
            .encode(None);
            prop_assert!(vrid_zero.is_err());
            prop_assert!(fractional_v2.is_err());

            let pseudo = PseudoHeader::new(Ipv4Addr::LOCALHOST, Ipv4Addr::LOCALHOST);
            let v3 = VrrpAdvertisement { version: VRRP_VERSION_3, ..v2 };
            let over_interval = VrrpAdvertisement {
                advert_interval_seconds: over as f64 / 100.0,
                ..v3.clone()
            }
            .encode(Some(&pseudo));
            let with_auth = VrrpAdvertisement { auth_type, ..v3 }.encode(Some(&pseudo));
            prop_assert!(over_interval.is_err());
            prop_assert!(with_auth.is_err());
        }
    }

    proptest! {
        #![proptest_config(codec_config!(crate::PANIC_CASES))]

        /// Property 3, through both variants of the dispatcher — a CARP advertisement shares
        /// its first octet with VRRPv2, so both readings of the same bytes are exercised.
        #[test]
        fn arbitrary_bytes_never_panic(data in proptest::collection::vec(any::<u8>(), 0..128)) {
            let _ = VrrpAdvertisement::decode(&data);
            let _ = Advertisement::decode(Variant::Vrrp, &data);
            let _ = Advertisement::decode(Variant::Carp, &data);
            let _ = checksum_is_valid(&data, None);
        }

        /// Property 3, biased: a well-formed header with an arbitrary address count, which is
        /// the `count * 4` arithmetic a hostile sender controls.
        #[test]
        fn arbitrary_address_counts_never_panic(
            version in prop::sample::select(vec![2u8, 3]),
            count in any::<u8>(),
            tail in proptest::collection::vec(any::<u8>(), 0..64),
        ) {
            let mut data = vec![(version << 4) | 1, 1, 100, count, 0, 1, 0, 0];
            data.extend_from_slice(&tail);
            let _ = VrrpAdvertisement::decode(&data);
        }
    }
}

// ===========================================================================================
// RADIUS — `src/server/radius/packet.rs`
// ===========================================================================================
//
// The positive example in this file: `encode_response` checks `MAX_PACKET_LEN` and
// `Attribute::encode` refuses a value past 253 rather than truncating it. Both bounds are
// asserted below so a future edit cannot quietly drop them.

#[cfg(feature = "radius")]
mod radius_props {
    use netget::server::radius::packet::{
        decode_attributes, decode_user_password, encode_response, encode_user_password,
        response_authenticator, Attribute, RadiusError, RadiusPacket,
    };
    use proptest::prelude::*;

    fn arb_attribute() -> impl Strategy<Value = Attribute> {
        (any::<u8>(), proptest::collection::vec(any::<u8>(), 0..=253))
            .prop_map(|(attr_type, value)| Attribute { attr_type, value })
    }

    /// Attributes whose encoded total fits a RADIUS datagram, which is what
    /// `encode_response` enforces.
    fn arb_attributes() -> impl Strategy<Value = Vec<Attribute>> {
        proptest::collection::vec(arb_attribute(), 0..12)
            .prop_filter("encoded attributes must fit a 4096-octet packet", |attrs| {
                20 + attrs.iter().map(|a| a.value.len() + 2).sum::<usize>() <= 4096
            })
    }

    proptest! {
        #![proptest_config(codec_config!(crate::CASES))]

        /// Property 1 for the attribute section.
        #[test]
        fn attributes_round_trip(attributes in arb_attributes()) {
            let mut body = Vec::new();
            for attribute in &attributes {
                body.extend_from_slice(&attribute.encode().unwrap());
            }
            prop_assert_eq!(decode_attributes(&body).unwrap(), attributes);
        }

        /// Property 1 for the whole packet, and the thing that actually matters about a
        /// RADIUS reply: the Response Authenticator. A reply a client discards as corrupt is
        /// just a timeout, so the digest is recomputed independently and compared.
        #[test]
        fn response_round_trips_and_is_correctly_signed(
            code in any::<u8>(),
            identifier in any::<u8>(),
            attributes in arb_attributes(),
            request_authenticator in any::<[u8; 16]>(),
            secret in proptest::collection::vec(any::<u8>(), 1..24),
        ) {
            let bytes =
                encode_response(code, identifier, &attributes, &request_authenticator, &secret)
                    .unwrap();
            let decoded = RadiusPacket::decode(&bytes).unwrap();
            prop_assert_eq!(decoded.code, code);
            prop_assert_eq!(decoded.identifier, identifier);
            prop_assert_eq!(&decoded.attributes, &attributes);

            let mut attr_bytes = Vec::new();
            for attribute in &attributes {
                attr_bytes.extend_from_slice(&attribute.encode().unwrap());
            }
            prop_assert_eq!(
                decoded.authenticator,
                response_authenticator(
                    code,
                    identifier,
                    &attr_bytes,
                    &request_authenticator,
                    &secret
                )
            );
        }

        /// Property 2, both halves, and both already hold: an attribute value past 253 is
        /// refused, and a response past 4096 octets is refused.
        #[test]
        fn over_long_values_are_refused(
            attr_type in any::<u8>(),
            len in 254usize..400,
            request_authenticator in any::<[u8; 16]>(),
        ) {
            let attribute = Attribute::new(attr_type, vec![0u8; len]);
            prop_assert_eq!(
                attribute.encode(),
                Err(RadiusError::AttributeTooLong { attr_type, len })
            );

            let big: Vec<Attribute> = (0..20)
                .map(|_| Attribute::new(1, vec![0u8; 253]))
                .collect();
            prop_assert!(matches!(
                encode_response(2, 0, &big, &request_authenticator, b"secret"),
                Err(RadiusError::ResponseTooLong(_))
            ));
        }

        /// Property 1 for the User-Password cipher, which is its own inverse under the same
        /// secret and Request Authenticator.
        ///
        /// The plaintext stops at 128 octets because that is the ceiling both directions now
        /// enforce; the test below pins the refusal above it.
        /// Trailing NULs are the NAS's own padding and are stripped on decode, so a plaintext
        /// that ends in one is not a value the format can carry.
        #[test]
        fn user_password_round_trips(
            password in proptest::collection::vec(1u8..=255, 0..=128),
            authenticator in any::<[u8; 16]>(),
            secret in proptest::collection::vec(any::<u8>(), 1..24),
        ) {
            let cipher = encode_user_password(&password, &authenticator, &secret).unwrap();
            prop_assert_eq!(cipher.len() % 16, 0);
            prop_assert!(cipher.len() <= 128);
            let plain = decode_user_password(&cipher, &authenticator, &secret).unwrap();
            prop_assert_eq!(plain, password);
        }
    }

    proptest! {
        #![proptest_config(codec_config!(crate::PANIC_CASES))]

        /// Property 3.
        #[test]
        fn arbitrary_bytes_never_panic(data in proptest::collection::vec(any::<u8>(), 0..300)) {
            let _ = RadiusPacket::decode(&data);
            let _ = decode_attributes(&data);
            let _ = decode_user_password(&data, &[0u8; 16], b"secret");
        }

        /// Property 3, biased: a valid 20-octet header in front of arbitrary attribute bytes,
        /// so the length check passes and the attribute walker is reached with a length byte
        /// the peer chose.
        #[test]
        fn valid_header_with_arbitrary_attributes_never_panics(
            code in any::<u8>(),
            identifier in any::<u8>(),
            body in proptest::collection::vec(any::<u8>(), 0..96),
        ) {
            let total = (20 + body.len()) as u16;
            let mut data = vec![code, identifier];
            data.extend_from_slice(&total.to_be_bytes());
            data.extend_from_slice(&[0u8; 16]);
            data.extend_from_slice(&body);
            let _ = RadiusPacket::decode(&data);
        }
    }

    // -------------------------------------------------------------------------------------
    // FINDINGS
    // -------------------------------------------------------------------------------------

    /// `encode_user_password` used to apply no length bound, while `decode_user_password`
    /// refuses any ciphertext past 128 octets (RFC 2865 §5.2). Minimal counterexample: a
    /// 129-octet plaintext, whose 144-octet ciphertext the codec's own decoder rejects.
    ///
    /// The smallest of the asymmetries here — the doc comment says the function exists for
    /// tests and for anything building an Access-Request, and the server never encrypts a
    /// password — but it was the same shape and it was one `if` away.
    #[test]
    fn encode_user_password_refuses_a_plaintext_its_own_decoder_would_reject() {
        assert!(encode_user_password(&vec![b'x'; 129], &[0u8; 16], b"secret").is_err());

        // The boundary: 128 octets is legal and decodes back to itself.
        let plaintext = vec![b'x'; 128];
        let cipher = encode_user_password(&plaintext, &[0u8; 16], b"secret").unwrap();
        assert_eq!(cipher.len(), 128);
        assert_eq!(
            decode_user_password(&cipher, &[0u8; 16], b"secret").unwrap(),
            plaintext
        );
    }
}

// ===========================================================================================
// NetBIOS Name Service — `src/server/netbios_ns/packet.rs`
// ===========================================================================================

#[cfg(feature = "netbios-ns")]
mod netbios_props {
    use netget::server::netbios_ns::packet::{
        decode_first_level, encode_first_level, encode_name_field, pad_netbios_name,
        read_name_field, split_netbios_name, ENCODED_NAME_LEN, NAME_LEN, WILDCARD_NAME,
    };
    use proptest::prelude::*;

    /// A name the wire format can hold: ASCII, at most fifteen octets, and with no trailing
    /// pad character — `split_netbios_name` trims trailing spaces and NULs, so a name that
    /// ends in one is not distinguishable from a shorter name once padded.
    fn arb_name() -> impl Strategy<Value = String> {
        "[A-Za-z0-9_$.-]{0,15}"
    }

    proptest! {
        #![proptest_config(codec_config!(crate::CASES))]

        /// Property 1: first-level encoding is a bijection on all 256 byte values, which is
        /// the whole reason it exists — a NetBIOS name is arbitrary octets, not text.
        #[test]
        fn first_level_encoding_round_trips(raw in any::<[u8; NAME_LEN]>()) {
            let encoded = encode_first_level(&raw);
            prop_assert_eq!(encoded.len(), ENCODED_NAME_LEN);
            prop_assert!(encoded.iter().all(|b| (b'A'..=b'P').contains(b)));
            prop_assert_eq!(decode_first_level(&encoded).unwrap(), raw);
        }

        /// Property 1: the human name and its service suffix survive padding.
        #[test]
        fn name_padding_round_trips(name in arb_name(), suffix in any::<u8>()) {
            let raw = pad_netbios_name(&name, suffix).unwrap();
            prop_assert_eq!(split_netbios_name(&raw), (name, suffix));
        }

        /// The wildcard is the one name padded with NULs rather than spaces (RFC 1001 §17),
        /// and it is the case a real `nmblookup -A` sends. Padding it with spaces was a live
        /// bug here, so it gets its own assertion alongside the general property.
        #[test]
        fn the_wildcard_is_nul_padded(suffix in any::<u8>()) {
            let raw = pad_netbios_name(WILDCARD_NAME, suffix).unwrap();
            prop_assert_eq!(&raw[1..NAME_LEN - 1], &[0u8; NAME_LEN - 2][..]);
            prop_assert_eq!(split_netbios_name(&raw), (WILDCARD_NAME.to_string(), suffix));
            prop_assert_eq!(
                &encode_first_level(&raw)[..2],
                b"CK",
                "the wildcard's first-level encoding must begin CK"
            );
        }

        /// Property 1 for the whole NAME field, scope labels included.
        #[test]
        fn name_field_round_trips(
            name in arb_name(),
            suffix in any::<u8>(),
            scope in proptest::option::of(
                proptest::collection::vec("[a-z0-9-]{1,20}", 1..3)
                    .prop_map(|labels| labels.join(".")),
            ),
        ) {
            let field = encode_name_field(&name, suffix, scope.as_deref()).unwrap();
            let read = read_name_field(&field, 0).unwrap();
            prop_assert_eq!(read.name, name);
            prop_assert_eq!(read.suffix, suffix);
            prop_assert_eq!(read.scope, scope);
            prop_assert_eq!(read.end, field.len());
            // The raw field is echoed verbatim in responses, so it must be exactly what was
            // written — not a re-encoding that might differ.
            prop_assert_eq!(read.raw, field);
        }

        /// Property 2: a name the format cannot hold is refused rather than truncated.
        /// Truncating changes which host is being talked about.
        #[test]
        fn over_long_and_non_ascii_names_are_refused(
            long in "[A-Z]{16,40}",
            suffix in any::<u8>(),
        ) {
            prop_assert!(pad_netbios_name(&long, suffix).is_err());
            prop_assert!(pad_netbios_name("café", suffix).is_err());
        }
    }

    proptest! {
        #![proptest_config(codec_config!(crate::PANIC_CASES))]

        /// Property 3.
        #[test]
        fn arbitrary_bytes_never_panic(
            data in proptest::collection::vec(any::<u8>(), 0..128),
            pos in 0usize..128,
        ) {
            let _ = decode_first_level(&data);
            if pos < data.len() {
                let _ = read_name_field(&data, pos);
            }
        }

        /// Property 3, biased: label-length octets drawn from the values that matter — 0x20
        /// (the encoded-name length), 0x00 (the root terminator), and the 0xC0 compression
        /// pointer that `read_name_field` refuses rather than follows.
        #[test]
        fn arbitrary_label_chains_never_panic(
            data in proptest::collection::vec(
                prop::sample::select(vec![0u8, 0x20, 0xC0, 0x40, 0x3F, b'A', b'P', b'Z']),
                0..96,
            )
        ) {
            let _ = read_name_field(&data, 0);
        }
    }
}

// ===========================================================================================
// Shared sanitisers — `src/utils/sanitize.rs`, `src/utils/truncate.rs`
// ===========================================================================================
//
// Not a codec, but the normalisation half of property 4 for all 140 servers at once: every
// protocol that strips a control character out of a value before logging it is supposed to go
// through here, and an idempotent normaliser is what makes "sanitised once" mean "sanitised".

mod sanitizer_props {
    use netget::utils::sanitize::{line_field, multiline, strip_controls, token};
    use netget::utils::{truncate_for_log, truncate_str};
    use proptest::prelude::*;

    proptest! {
        #![proptest_config(codec_config!(crate::CASES))]

        /// Property 4, for every sanitiser — all four are fixed points.
        ///
        /// `token` is here because it now is one: it used to trim *before* truncating, so a
        /// cut landing after a space left a trailing space a second call removed.
        #[test]
        fn sanitisers_are_idempotent(s in ".{0,64}", max in 0usize..48) {
            prop_assert_eq!(line_field(&line_field(&s)), line_field(&s));
            prop_assert_eq!(strip_controls(&strip_controls(&s)), strip_controls(&s));
            prop_assert_eq!(multiline(&multiline(&s)), multiline(&s));
            prop_assert_eq!(token(&token(&s, max), max), token(&s, max));
        }

        /// A sanitised value carries no control character at all — which is the property the
        /// log-forgery defences actually rest on. `multiline` keeps `\n` deliberately, so it
        /// is checked against its own narrower promise.
        #[test]
        fn sanitisers_remove_every_control_character(s in ".{0,64}") {
            prop_assert!(!line_field(&s).chars().any(char::is_control));
            prop_assert!(!strip_controls(&s).chars().any(char::is_control));
            prop_assert!(!token(&s, 24).chars().any(char::is_control));
            prop_assert!(!multiline(&s).chars().any(|c| c.is_control() && c != '\n'));
        }

        /// `line_field` substitutes rather than deletes, so it cannot join two fields into
        /// one token — the difference that makes it the right choice for a structured record.
        #[test]
        fn line_field_preserves_character_count(s in ".{0,64}") {
            prop_assert_eq!(line_field(&s).chars().count(), s.chars().count());
        }

        /// Property 4 for the truncators, and the char-boundary guarantee that exists because
        /// `&s[..n]` panicked on multi-byte UTF-8 in production.
        #[test]
        fn truncation_is_idempotent_and_never_splits_a_character(
            s in ".{0,64}",
            max in 0usize..48,
        ) {
            let once = truncate_str(&s, max);
            prop_assert_eq!(truncate_str(once, max), once);
            prop_assert!(once.len() <= max);
            prop_assert!(s.starts_with(once));
            // Only the byte budget is promised for the suffixed form; it must not panic and
            // must stay valid UTF-8, which returning a `String` already guarantees.
            let _ = truncate_for_log(&s, max);
        }
    }

    // -------------------------------------------------------------------------------------
    // FINDINGS
    // -------------------------------------------------------------------------------------

    /// `sanitize::token` used to be **not idempotent**. It trimmed and *then* truncated, so a
    /// cut that lands after a space left a trailing space that a second call removed.
    ///
    /// Minimal counterexample, hand-reduced from what proptest shrank to:
    /// `token("a b", 2)` was `"a "`, and `token("a ", 2)` is `"a"`.
    ///
    /// Minor but real. `token` is what produces an identifier — the one caller today is a
    /// BLE device name — and a normaliser that is not a fixed point means "already
    /// sanitised" is not a stable predicate: a value sanitised at ingest and sanitised again
    /// at render compared unequal to itself. It now trims *after* truncating.
    #[test]
    fn token_is_idempotent() {
        assert_eq!(token("a b", 2), "a");
        assert_eq!(token(&token("a b", 2), 2), token("a b", 2));
        // Leading whitespace is still dropped before the cut, so a padded value does not
        // spend its whole budget on spaces.
        assert_eq!(token("   ab", 2), "ab");
    }
}

// ===========================================================================================
// STP / RSTP — `src/server/stp/codec.rs`
// ===========================================================================================
//
// No findings: every bound this codec narrows is checked first, and `encode_frame` refuses a
// body whose 802.3 length field would land in EtherType space rather than narrowing into it.
// That refusal is asserted below so a future edit cannot quietly drop it.

#[cfg(feature = "stp")]
mod stp_props {
    use netget::server::stp::codec::{
        decode_frame, encode_frame, encode_tcn_bpdu, seconds_to_ticks, ticks_to_seconds, Bpdu,
        BpduFlags, BridgeId, ConfigBpdu, PortId, PortRole, BPDU_TYPE_CONFIG, BPDU_TYPE_RST,
        BRIDGE_PRIORITY_STEP, CONFIG_BPDU_LEN, LLC_HEADER_LEN, MAX_8023_LENGTH,
        MIN_ETHERNET_FRAME_LEN, PORT_PRIORITY_STEP, RST_BPDU_LEN, STP_MULTICAST_MAC,
    };
    use proptest::prelude::*;

    /// A bridge priority is the top four bits of a sixteen-bit field; the low twelve are the
    /// VLAN. Only multiples of 4096 exist, which is exactly what `BridgeId::new` enforces.
    fn arb_bridge_id() -> impl Strategy<Value = BridgeId> {
        (0u16..=15, 0u16..=4095, any::<[u8; 6]>()).prop_map(|(step, ext, mac)| BridgeId {
            priority: step * BRIDGE_PRIORITY_STEP,
            system_id_extension: ext,
            mac,
        })
    }

    /// 802.1t re-split the port identifier 4/12, so a port priority is a multiple of 16.
    fn arb_port_id() -> impl Strategy<Value = PortId> {
        (0u8..=15, 0u16..=4095).prop_map(|(step, number)| PortId {
            priority: step * PORT_PRIORITY_STEP,
            number,
        })
    }

    /// Timers are a 1/256-second field. Generating from the wire value is what makes the
    /// round trip exact — an arbitrary `f64` of seconds has no representation.
    fn arb_seconds() -> impl Strategy<Value = f64> {
        any::<u16>().prop_map(ticks_to_seconds)
    }

    fn arb_config_bpdu() -> impl Strategy<Value = ConfigBpdu> {
        (
            any::<u8>(),
            prop::sample::select(vec![BPDU_TYPE_CONFIG, BPDU_TYPE_RST]),
            any::<u8>().prop_map(BpduFlags::from_byte),
            arb_bridge_id(),
            any::<u32>(),
            arb_bridge_id(),
            arb_port_id(),
            arb_seconds(),
            arb_seconds(),
            arb_seconds(),
            arb_seconds(),
        )
            .prop_map(
                |(
                    version,
                    bpdu_type,
                    flags,
                    root,
                    root_path_cost,
                    bridge,
                    port,
                    message_age_seconds,
                    max_age_seconds,
                    hello_time_seconds,
                    forward_delay_seconds,
                )| ConfigBpdu {
                    version,
                    bpdu_type,
                    flags,
                    root,
                    root_path_cost,
                    bridge,
                    port,
                    message_age_seconds,
                    max_age_seconds,
                    hello_time_seconds,
                    forward_delay_seconds,
                },
            )
    }

    proptest! {
        #![proptest_config(codec_config!(crate::CASES))]

        /// Property 1: the flags octet is a bijection on all 256 values, including the RSTP
        /// port role in bits 2-3. An 802.1D BPDU decodes to `Unknown` and four false
        /// booleans, which is what the wire actually says.
        #[test]
        fn bpdu_flags_round_trip(byte in any::<u8>()) {
            prop_assert_eq!(BpduFlags::from_byte(byte).to_byte(), byte);
        }

        /// Property 1 for the two identifiers, whose whole point is that the priority and the
        /// VLAN / port number share one sixteen-bit field.
        #[test]
        fn identifiers_round_trip(bridge in arb_bridge_id(), port in arb_port_id()) {
            prop_assert_eq!(BridgeId::decode(&bridge.encode()).unwrap(), bridge);
            prop_assert_eq!(PortId::decode(&port.encode()).unwrap(), port);
        }

        /// Property 2: a priority the wire cannot carry is refused, not masked. The low bits
        /// belong to the VLAN, so masking would silently change which instance is meant.
        #[test]
        fn identifiers_refuse_unrepresentable_priorities(
            priority in 1u16..=61439,
            port_priority in 1u8..=239,
            ext in 4096u16..=u16::MAX,
            number in 4096u16..=u16::MAX,
            mac in any::<[u8; 6]>(),
        ) {
            prop_assume!(priority % BRIDGE_PRIORITY_STEP != 0);
            prop_assume!(port_priority % PORT_PRIORITY_STEP != 0);
            prop_assert!(BridgeId::new(priority, 0, mac).is_err());
            prop_assert!(BridgeId::new(0, ext, mac).is_err());
            prop_assert!(PortId::new(port_priority, 0).is_err());
            prop_assert!(PortId::new(0, number).is_err());
        }

        /// Property 1 for the timers: seconds in, the 1/256s field out, and back.
        #[test]
        fn timer_ticks_round_trip(ticks in any::<u16>()) {
            prop_assert_eq!(seconds_to_ticks(ticks_to_seconds(ticks)).unwrap(), ticks);
        }

        /// Property 1 for a whole configuration or RST BPDU.
        #[test]
        fn config_bpdu_round_trips(bpdu in arb_config_bpdu()) {
            let body = bpdu.encode().unwrap();
            let expected = if bpdu.bpdu_type == BPDU_TYPE_RST {
                RST_BPDU_LEN
            } else {
                CONFIG_BPDU_LEN
            };
            prop_assert_eq!(body.len(), expected);
            prop_assert_eq!(ConfigBpdu::decode(&body).unwrap(), bpdu.clone());
            prop_assert_eq!(Bpdu::decode(&body).unwrap(), Bpdu::Config(bpdu));
        }

        /// Property 1 through the 802.3 + LLC frame, which pads to the sixty-octet Ethernet
        /// minimum and must not hand the padding on as BPDU content.
        #[test]
        fn frame_round_trips(bpdu in arb_config_bpdu(), source in any::<[u8; 6]>()) {
            let body = bpdu.encode().unwrap();
            let frame = encode_frame(STP_MULTICAST_MAC, source, &body).unwrap();
            prop_assert!(frame.len() >= MIN_ETHERNET_FRAME_LEN);
            let decoded = decode_frame(&frame).unwrap();
            prop_assert_eq!(decoded.destination, STP_MULTICAST_MAC);
            prop_assert_eq!(decoded.source, source);
            prop_assert_eq!(&decoded.payload, &body);
            prop_assert_eq!(ConfigBpdu::decode(&decoded.payload).unwrap(), bpdu);
        }

        /// The four-octet TCN carries no fields at all, and survives the same padding.
        #[test]
        fn tcn_round_trips(source in any::<[u8; 6]>()) {
            let body = encode_tcn_bpdu();
            let frame = encode_frame(STP_MULTICAST_MAC, source, &body).unwrap();
            let decoded = decode_frame(&frame).unwrap();
            prop_assert_eq!(&decoded.payload, &body);
            prop_assert_eq!(
                Bpdu::decode(&decoded.payload).unwrap(),
                Bpdu::TopologyChangeNotification
            );
        }

        /// Property 2: a body whose 802.3 length field would be 1536 or more is refused.
        /// Narrowing with a bare `as u16` would write an EtherType there instead, and the
        /// frame would silently stop being 802.3 — every receiver reading it as Ethernet II
        /// and never parsing the BPDU. `decode_frame` rejects the same thing inbound.
        #[test]
        fn over_long_bodies_are_refused(len in (MAX_8023_LENGTH - LLC_HEADER_LEN + 1)..2048) {
            let body = vec![0u8; len];
            prop_assert!(encode_frame(STP_MULTICAST_MAC, [0u8; 6], &body).is_err());
        }
    }

    proptest! {
        #![proptest_config(codec_config!(crate::PANIC_CASES))]

        /// Property 3.
        #[test]
        fn arbitrary_bytes_never_panic(data in proptest::collection::vec(any::<u8>(), 0..128)) {
            let _ = decode_frame(&data);
            let _ = Bpdu::decode(&data);
            let _ = ConfigBpdu::decode(&data);
            let _ = BridgeId::decode(&data);
            let _ = PortId::decode(&data);
        }

        /// Property 3, biased: a well-formed Ethernet + LLC header in front of an arbitrary
        /// declared length, which is the `declared_len - LLC_HEADER_LEN` subtraction a
        /// hostile sender controls.
        #[test]
        fn arbitrary_declared_lengths_never_panic(
            declared in any::<u16>(),
            tail in proptest::collection::vec(any::<u8>(), 0..64),
        ) {
            let mut frame = vec![0u8; 12];
            frame.extend_from_slice(&declared.to_be_bytes());
            frame.extend_from_slice(&[0x42, 0x42, 0x03]);
            frame.extend_from_slice(&tail);
            if let Ok(decoded) = decode_frame(&frame) {
                let _ = Bpdu::decode(&decoded.payload);
            }
        }

        /// Every port role name the model may send resolves, and resolves back.
        #[test]
        fn port_role_names_round_trip(bits in 0u8..4) {
            let role = BpduFlags::from_byte(bits << 2).port_role;
            prop_assert_eq!(PortRole::from_name(role.as_str()).unwrap(), role);
        }
    }
}

// ===========================================================================================
// LLDP — `src/server/lldp/codec.rs`
// ===========================================================================================
//
// No findings. `push_tlv` refuses past the 9-bit length field, `push_text_tlv` refuses past
// 255, and the identifier fields refuse a control character rather than stripping it. The
// *decode* side strips instead, which is the right asymmetry — a neighbour cannot be asked to
// resend — so the round-trip generator produces control-free text, and the refusal gets its
// own assertion.

#[cfg(feature = "lldp")]
mod lldp_props {
    use netget::server::lldp::codec::{
        capability_bits, capability_names, decode_frame, encode_frame, format_mac, parse_mac,
        subtype_code, subtype_name, IdKind, Lldpdu, ManagementAddress, CHASSIS_ID_SUBTYPES,
        LLDP_MULTICAST_MAC, PORT_ID_SUBTYPES, SYSTEM_CAPABILITIES,
    };
    use proptest::prelude::*;
    use std::net::{Ipv4Addr, Ipv6Addr};

    /// An identifier value in the exact notation its subtype selects.
    ///
    /// A MAC comes back from `decode` as `format_mac` wrote it and an IP as `to_string` wrote
    /// it, so the generator produces those canonical forms: anything else is a value the
    /// codec *normalises*, which is a different property (asserted separately below).
    fn arb_id(kind: IdKind) -> impl Strategy<Value = (u8, String)> {
        let (mac_subtype, addr_subtype) = match kind {
            IdKind::Chassis => (4u8, 5u8),
            IdKind::Port => (3u8, 4u8),
        };
        let table: Vec<u8> = match kind {
            IdKind::Chassis => CHASSIS_ID_SUBTYPES,
            IdKind::Port => PORT_ID_SUBTYPES,
        }
        .iter()
        .map(|(c, _)| *c)
        .filter(|c| *c != mac_subtype && *c != addr_subtype)
        .collect();

        prop_oneof![
            any::<[u8; 6]>().prop_map(move |mac| (mac_subtype, format_mac(&mac))),
            any::<[u8; 4]>().prop_map(move |o| (addr_subtype, Ipv4Addr::from(o).to_string())),
            any::<[u8; 16]>().prop_map(move |o| (addr_subtype, Ipv6Addr::from(o).to_string())),
            (prop::sample::select(table), "[ -~]{1,200}"),
        ]
    }

    fn arb_management_address() -> impl Strategy<Value = ManagementAddress> {
        let address = prop_oneof![
            any::<[u8; 4]>().prop_map(|o| (1u8, Ipv4Addr::from(o).to_string())),
            any::<[u8; 16]>().prop_map(|o| (2u8, Ipv6Addr::from(o).to_string())),
            any::<[u8; 6]>().prop_map(|mac| (6u8, format_mac(&mac))),
        ];
        (address, any::<u8>(), any::<u32>()).prop_map(
            |((family, address), interface_numbering_subtype, interface_number)| {
                ManagementAddress {
                    family,
                    address,
                    interface_numbering_subtype,
                    interface_number,
                }
            },
        )
    }

    fn arb_lldpdu() -> impl Strategy<Value = Lldpdu> {
        (
            arb_id(IdKind::Chassis),
            arb_id(IdKind::Port),
            any::<u16>(),
            // Port Description and System Name are identifiers on decode (control characters
            // become spaces) and are refused on encode, so they are printable here.
            proptest::option::of("[ -~]{0,120}"),
            proptest::option::of("[ -~]{0,120}"),
            // System Description is `sysDescr` — a real one is a multi-line IOS banner, and
            // the codec deliberately keeps its newlines. Trailing NULs are trimmed on decode,
            // so they are not part of the contract.
            proptest::option::of("[ -~\n]{0,120}"),
            proptest::option::of((any::<u16>(), any::<u16>())),
            proptest::option::of(arb_management_address()),
        )
            .prop_map(
                |(
                    (chassis_id_subtype, chassis_id),
                    (port_id_subtype, port_id),
                    ttl,
                    port_description,
                    system_name,
                    system_description,
                    capabilities,
                    management_address,
                )| Lldpdu {
                    chassis_id_subtype,
                    chassis_id,
                    port_id_subtype,
                    port_id,
                    ttl,
                    port_description,
                    system_name,
                    system_description,
                    capabilities,
                    management_address,
                },
            )
    }

    proptest! {
        #![proptest_config(codec_config!(crate::CASES))]

        /// Property 1 for the LLDPDU.
        #[test]
        fn lldpdu_round_trips(lldpdu in arb_lldpdu()) {
            let bytes = lldpdu.encode().unwrap();
            let decoded = Lldpdu::decode(&bytes);
            prop_assert!(decoded.is_ok(), "{:?}", decoded);
            prop_assert_eq!(decoded.unwrap(), lldpdu);
        }

        /// Property 1 through the Ethernet frame.
        #[test]
        fn frame_round_trips(lldpdu in arb_lldpdu(), source in any::<[u8; 6]>()) {
            let frame = encode_frame(LLDP_MULTICAST_MAC, source, &lldpdu).unwrap();
            let decoded = decode_frame(&frame).unwrap();
            prop_assert_eq!(decoded.destination_mac, LLDP_MULTICAST_MAC);
            prop_assert_eq!(decoded.source_mac, source);
            prop_assert_eq!(decoded.lldpdu, lldpdu);
        }

        /// Property 4: MAC notation is normalised to one canonical form, idempotently. The
        /// *decode* side always produces `format_mac`'s output, so anything else a model
        /// writes is normalised through it.
        #[test]
        fn mac_notation_is_normalised_idempotently(mac in any::<[u8; 6]>()) {
            let canonical = format_mac(&mac);
            prop_assert_eq!(parse_mac(&canonical).unwrap(), mac);
            prop_assert_eq!(format_mac(&parse_mac(&canonical).unwrap()), canonical.clone());
            // The notations a model actually writes all land on the same six octets.
            let upper = canonical.to_ascii_uppercase();
            let dashed = canonical.replace(':', "-");
            prop_assert_eq!(parse_mac(&upper).unwrap(), mac);
            prop_assert_eq!(parse_mac(&dashed).unwrap(), mac);
        }

        /// Property 4: subtype names and codes are a bijection within each table, and the two
        /// tables are *different* — a MAC address is subtype 4 for a chassis and 3 for a
        /// port, which the module calls the single easiest thing to get wrong here.
        #[test]
        fn subtype_names_round_trip(index in 0usize..7) {
            for (kind, table) in [
                (IdKind::Chassis, CHASSIS_ID_SUBTYPES),
                (IdKind::Port, PORT_ID_SUBTYPES),
            ] {
                let (code, name) = table[index];
                prop_assert_eq!(subtype_code(kind, name), Some(code));
                prop_assert_eq!(subtype_name(kind, code), name.to_string());
                // Case and surrounding space are normalised away.
                let loose = format!("  {}  ", name.to_ascii_uppercase());
                prop_assert_eq!(subtype_code(kind, &loose), Some(code));
            }
            prop_assert_eq!(subtype_code(IdKind::Chassis, "mac_address"), Some(4));
            prop_assert_eq!(subtype_code(IdKind::Port, "mac_address"), Some(3));
        }

        /// Property 2: text the 802.1AB length fields cannot carry is refused, and a control
        /// character in an identifier is refused rather than stripped — a newline in a system
        /// name forges a whole neighbour entry in every display that prints it.
        #[test]
        fn unrepresentable_text_is_refused(
            long in "[a-z]{256,300}",
            control in prop::sample::select(vec!['\n', '\r', '\0', '\u{1b}']),
        ) {
            let base = Lldpdu {
                chassis_id_subtype: 7,
                chassis_id: "netget".to_string(),
                port_id_subtype: 7,
                port_id: "eth0".to_string(),
                ttl: 120,
                port_description: None,
                system_name: None,
                system_description: None,
                capabilities: None,
                management_address: None,
            };
            let too_long = Lldpdu {
                system_name: Some(long.clone()),
                ..base.clone()
            };
            let forged_name = Lldpdu {
                system_name: Some(format!("switch{control}Trustme")),
                ..base.clone()
            };
            let forged_chassis = Lldpdu {
                chassis_id: format!("a{control}b"),
                ..base.clone()
            };
            let long_chassis = Lldpdu {
                chassis_id: long,
                ..base
            };
            prop_assert!(too_long.encode().is_err());
            prop_assert!(forged_name.encode().is_err());
            prop_assert!(forged_chassis.encode().is_err());
            prop_assert!(long_chassis.encode().is_err());
        }

        /// Every set bit is named — the eleven 802.1AB defines by name, the other five as
        /// `reserved_bit_N` — and no clear bit produces one. Reporting a reserved bit rather
        /// than dropping it is deliberate: the wire said it, so the model is told.
        ///
        /// (The first version of this property asserted `(bits & 0x07FF).count_ones()`, on
        /// the assumption that an undefined bit is discarded. That was an over-strict
        /// property, not a defect: `capability_names` names all sixteen on purpose.)
        #[test]
        fn every_capability_bit_is_named(bits in any::<u16>()) {
            let names = capability_names(bits);
            prop_assert_eq!(names.len(), bits.count_ones() as usize);
            let defined: Vec<&str> = SYSTEM_CAPABILITIES.iter().map(|(_, n)| *n).collect();
            for (i, name) in names.iter().enumerate() {
                prop_assert!(
                    defined.contains(&name.as_str()) || name.starts_with("reserved_bit_"),
                    "capability {i} came back as {name:?}"
                );
            }
            // A named bit round-trips; a reserved one is refused rather than silently
            // dropped, so a model cannot believe it advertised something it did not.
            let named: Vec<String> = names
                .iter()
                .filter(|n| defined.contains(&n.as_str()))
                .cloned()
                .collect();
            let expected: u16 = SYSTEM_CAPABILITIES
                .iter()
                .filter(|(b, _)| bits & b != 0)
                .map(|(b, _)| *b)
                .fold(0, |acc, b| acc | b);
            prop_assert_eq!(capability_bits(&named).unwrap(), expected);
            if names.iter().any(|n| n.starts_with("reserved_bit_")) {
                prop_assert!(capability_bits(&names).is_err());
            }
        }
    }

    proptest! {
        #![proptest_config(codec_config!(crate::PANIC_CASES))]

        /// Property 3.
        #[test]
        fn arbitrary_bytes_never_panic(data in proptest::collection::vec(any::<u8>(), 0..160)) {
            let _ = decode_frame(&data);
            let _ = Lldpdu::decode(&data);
            let _ = parse_mac(&String::from_utf8_lossy(&data));
        }

        /// Property 3, biased: a valid Ethernet header with the LLDP EtherType in front of
        /// arbitrary TLV bytes, so the frame check passes and the 9-bit-length TLV walker is
        /// reached with lengths the peer chose.
        #[test]
        fn arbitrary_tlv_chains_never_panic(
            tlvs in proptest::collection::vec(any::<u8>(), 0..128),
        ) {
            let mut frame = vec![0u8; 12];
            frame.extend_from_slice(&0x88CCu16.to_be_bytes());
            frame.extend_from_slice(&tlvs);
            let _ = decode_frame(&frame);
        }
    }
}

// ===========================================================================================
// CDP — `src/server/cdp/codec.rs`
// ===========================================================================================
//
// No findings. `push_tlv` bounds before narrowing (`checked_add(4)` then `u16::try_from`) and
// `encode_frame` refuses a body past 1500 rather than writing an EtherType into the 802.3
// length field. Both are asserted.

#[cfg(feature = "cdp")]
mod cdp_props {
    use netget::server::cdp::codec::{
        capability_bits, decode_frame, decode_payload, encode_frame, encode_payload, mac_to_string,
        parse_mac, CdpAddress, CdpAdvertisement, Duplex, CAPABILITY_FLAGS, CDP_MULTICAST_MAC,
        MAX_8023_LENGTH, MAX_TEXT_TLV,
    };
    use proptest::prelude::*;
    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    fn arb_address() -> impl Strategy<Value = CdpAddress> {
        prop_oneof![
            any::<[u8; 4]>().prop_map(|o| CdpAddress::new(IpAddr::V4(Ipv4Addr::from(o)))),
            any::<[u8; 16]>().prop_map(|o| CdpAddress::new(IpAddr::V6(Ipv6Addr::from(o)))),
        ]
    }

    /// A text TLV: printable, and inside the 255-octet cap `check_text_field` enforces.
    /// A control character is refused rather than stripped on this side (the model can be
    /// told), which is why the generator does not produce one — see the refusal property.
    fn arb_text() -> impl Strategy<Value = Option<String>> {
        proptest::option::of("[ -~]{0,120}")
    }

    fn arb_advertisement() -> impl Strategy<Value = CdpAdvertisement> {
        (
            prop::sample::select(vec![1u8, 2]),
            any::<u8>(),
            arb_text(),
            arb_text(),
            arb_text(),
            // Software Version is a real IOS banner and is deliberately allowed newlines.
            proptest::option::of("[ -~\n]{0,120}"),
            proptest::option::of(any::<u32>()),
            proptest::option::of(any::<u16>()),
            proptest::option::of(prop_oneof![Just(Duplex::Half), Just(Duplex::Full)]),
            // `decode_address_list` stops at 64 entries, so a longer list is not something
            // the wire format round-trips; four is what a real device sends.
            proptest::collection::vec(arb_address(), 0..4),
            proptest::collection::vec(arb_address(), 0..4),
        )
            .prop_map(
                |(
                    version,
                    ttl,
                    device_id,
                    port_id,
                    platform,
                    software_version,
                    capabilities,
                    native_vlan,
                    duplex,
                    addresses,
                    management_addresses,
                )| CdpAdvertisement {
                    version,
                    ttl,
                    device_id,
                    port_id,
                    platform,
                    software_version,
                    capabilities,
                    native_vlan,
                    duplex,
                    addresses,
                    management_addresses,
                    // Decode-direction only: `OtherTlv` records a type and a length, and
                    // `encode_payload` has nothing to write for it.
                    other_tlvs: Vec::new(),
                },
            )
    }

    proptest! {
        #![proptest_config(codec_config!(crate::CASES))]

        /// Property 1, plus the checksum verifying in place — which is what a real device
        /// checks before it will show the neighbour at all.
        #[test]
        fn payload_round_trips(advertisement in arb_advertisement()) {
            let payload = encode_payload(&advertisement).unwrap();
            let decoded = decode_payload(&payload).unwrap();
            prop_assert!(
                decoded.checksum_valid(),
                "declared {:04x} computed {:04x}",
                decoded.declared_checksum,
                decoded.computed_checksum
            );
            prop_assert_eq!(decoded.advertisement, advertisement);
        }

        /// Property 1 through the 802.3 + LLC/SNAP frame.
        #[test]
        fn frame_round_trips(advertisement in arb_advertisement(), source in any::<[u8; 6]>()) {
            let payload = encode_payload(&advertisement).unwrap();
            let frame = encode_frame(source, &payload).unwrap();
            let (header, body) = decode_frame(&frame).unwrap();
            prop_assert_eq!(header.destination_mac, CDP_MULTICAST_MAC);
            prop_assert_eq!(header.source_mac, source);
            prop_assert_eq!(body, &payload[..]);
            prop_assert_eq!(decode_payload(body).unwrap().advertisement, advertisement);
        }

        /// Property 4: MAC notation normalises to one canonical form.
        #[test]
        fn mac_notation_is_normalised_idempotently(mac in any::<[u8; 6]>()) {
            let canonical = mac_to_string(&mac);
            prop_assert_eq!(parse_mac(&canonical).unwrap(), mac);
            prop_assert_eq!(mac_to_string(&parse_mac(&canonical).unwrap()), canonical);
        }

        /// Property 4: capability names resolve to bits and back, case-insensitively.
        #[test]
        fn capability_names_round_trip(index in 0usize..CAPABILITY_FLAGS.len()) {
            let (bit, name) = CAPABILITY_FLAGS[index];
            prop_assert_eq!(capability_bits(&[name.to_string()]).unwrap(), bit);
            let upper = name.to_ascii_uppercase();
            prop_assert_eq!(capability_bits(&[upper]).unwrap(), bit);
        }

        /// Property 2: a control character in an identifier is refused, text past 255 octets
        /// is refused, and a frame body past 1500 is refused rather than narrowed into
        /// EtherType space.
        #[test]
        fn unrepresentable_values_are_refused(
            long in "[a-z]{256,300}",
            control in prop::sample::select(vec!['\n', '\r', '\0', '\u{1b}']),
            body_len in (MAX_8023_LENGTH + 1)..2048,
        ) {
            prop_assert!(long.len() > MAX_TEXT_TLV);
            let base = CdpAdvertisement::default();
            let forged = CdpAdvertisement {
                device_id: Some(format!("switch{control}Trustme")),
                ..base.clone()
            };
            let too_long = CdpAdvertisement {
                device_id: Some(long),
                ..base.clone()
            };
            let bad_version = CdpAdvertisement { version: 7, ..base };
            let big_frame = vec![0u8; body_len];
            prop_assert!(encode_payload(&forged).is_err());
            prop_assert!(encode_payload(&too_long).is_err());
            prop_assert!(encode_payload(&bad_version).is_err());
            prop_assert!(encode_frame([0u8; 6], &big_frame).is_err());
        }
    }

    proptest! {
        #![proptest_config(codec_config!(crate::PANIC_CASES))]

        /// Property 3.
        #[test]
        fn arbitrary_bytes_never_panic(data in proptest::collection::vec(any::<u8>(), 0..160)) {
            let _ = decode_payload(&data);
            if let Ok((_, body)) = decode_frame(&data) {
                let _ = decode_payload(body);
            }
        }

        /// Property 3, biased: a valid header in front of arbitrary TLV bytes, so the
        /// LLC/SNAP check passes and the TLV walker — including the address-list parser, whose
        /// count the peer chooses — is actually reached.
        #[test]
        fn arbitrary_tlv_chains_never_panic(
            tlvs in proptest::collection::vec(any::<u8>(), 0..128),
        ) {
            let mut payload = vec![2u8, 180, 0, 0];
            payload.extend_from_slice(&tlvs);
            let _ = decode_payload(&payload);
        }
    }
}

// ===========================================================================================
// NDEF — `src/client/nfc/ndef.rs`
// ===========================================================================================
//
// `push_record` used to write `type_field.len().min(255) as u8` as the TYPE LENGTH while
// writing the *whole* type field, so a `mime_type` or `domain_type` of 256 bytes or more
// produced a message whose own decoder reads the wrong number of type octets and then
// misattributes the remainder. It now refuses, like the rest of this codec.

#[cfg(feature = "nfc-client")]
mod ndef_props {
    use netget::client::nfc::ndef::{decode_message, encode_message, MAX_MESSAGE_LEN, MAX_RECORDS};
    use proptest::prelude::*;
    use serde_json::{json, Value};

    /// Text a Text record can carry: the codec refuses C0/C1 controls and the Unicode
    /// bidirectional overrides, and keeps tab / newline / carriage return.
    fn arb_record_text() -> impl Strategy<Value = Value> {
        "[ -~\t\n]{0,60}".prop_map(|text| json!({ "type": "text", "text": text }))
    }

    /// A URI is printable US-ASCII with no whitespace (RFC 3986), which is what `check_uri`
    /// enforces — anything else has to be percent-encoded before it gets here.
    fn arb_record_uri() -> impl Strategy<Value = Value> {
        "[!-~]{1,60}".prop_map(|uri| json!({ "type": "uri", "uri": uri }))
    }

    fn arb_record_mime() -> impl Strategy<Value = Value> {
        ("[a-z]{1,12}/[a-z]{1,12}", "([0-9a-f]{2}){0,20}").prop_map(|(mime_type, payload)| {
            json!({ "type": "mime", "mime_type": mime_type, "payload_hex": payload })
        })
    }

    fn arb_record_external() -> impl Strategy<Value = Value> {
        ("[a-z]{1,10}\\.[a-z]{2,4}:[a-z]{1,10}", "[ -~]{1,40}").prop_map(
            |(domain_type, payload_text)| {
                json!({
                    "type": "external",
                    "domain_type": domain_type,
                    "payload_text": payload_text,
                })
            },
        )
    }

    proptest! {
        #![proptest_config(codec_config!(crate::CASES))]

        /// Property 1 for Text records, whose payload is a status byte, an IANA language tag
        /// and the text. The language length lives in six bits of the status byte, which is
        /// the field a lazy implementation gets wrong.
        #[test]
        fn text_records_round_trip(
            text in "[ -~\t\n]{0,60}",
            language in "[a-z]{2}(-[A-Z]{2})?",
        ) {
            let record = json!({ "type": "text", "text": text, "language": language });
            let decoded = decode_message(&encode_message(&[record]).unwrap()).unwrap();
            prop_assert_eq!(decoded.len(), 1);
            prop_assert_eq!(&decoded[0]["type"], "text");
            prop_assert_eq!(&decoded[0]["text"], &Value::String(text));
            prop_assert_eq!(&decoded[0]["language"], &Value::String(language));
            prop_assert_eq!(&decoded[0]["text_encoding"], "utf-8");
            prop_assert_eq!(&decoded[0]["message_end"], &Value::Bool(true));
        }

        /// Property 1 for URI records, whose whole point is the 36-entry prefix table: the
        /// longest matching prefix is replaced by one byte and must expand back exactly.
        #[test]
        fn uri_records_round_trip(uri in "[!-~]{1,60}") {
            let record = json!({ "type": "uri", "uri": uri });
            let decoded = decode_message(&encode_message(&[record]).unwrap()).unwrap();
            prop_assert_eq!(decoded.len(), 1);
            prop_assert_eq!(&decoded[0]["type"], "uri");
            prop_assert_eq!(&decoded[0]["uri"], &Value::String(uri));
        }

        /// The prefix abbreviation is exercised on purpose: `https://www.` must become code
        /// 2 rather than code 4 plus a literal `www.`, and either way must expand back.
        #[test]
        fn uri_prefixes_expand_back(
            prefix in prop::sample::select(vec![
                "http://www.", "https://www.", "http://", "https://", "tel:", "mailto:",
                "urn:epc:id:", "file://",
            ]),
            rest in "[!-~]{1,30}",
        ) {
            let uri = format!("{prefix}{rest}");
            let record = json!({ "type": "uri", "uri": uri.clone() });
            let decoded = decode_message(&encode_message(&[record]).unwrap()).unwrap();
            prop_assert_eq!(&decoded[0]["uri"], &Value::String(uri));
        }

        /// Property 1 for MIME and External records, whose TYPE field is the media type or
        /// the `domain:type` and whose payload is opaque.
        #[test]
        fn typed_records_round_trip(
            mime in arb_record_mime(),
            external in arb_record_external(),
        ) {
            for record in [mime, external] {
                let bytes = encode_message(std::slice::from_ref(&record)).unwrap();
                let decoded = decode_message(&bytes).unwrap();
                prop_assert_eq!(decoded.len(), 1);
                prop_assert_eq!(&decoded[0]["type"], &record["type"]);
                for key in ["mime_type", "domain_type"] {
                    if let Some(expected) = record.get(key) {
                        prop_assert_eq!(&decoded[0][key], expected);
                    }
                }
                if let Some(text) = record.get("payload_text") {
                    prop_assert_eq!(&decoded[0]["payload_text"], text);
                }
                if let Some(hex) = record.get("payload_hex").and_then(|v| v.as_str()) {
                    let seen = decoded[0]["payload_hex"]
                        .as_str()
                        .map(str::to_ascii_lowercase);
                    prop_assert_eq!(seen, Some(hex.to_ascii_lowercase()));
                }
            }
        }

        /// Property 1 for a whole multi-record message: the MB/ME flags must mark exactly the
        /// first and last record, or a reader stops early or runs on.
        #[test]
        fn multi_record_messages_round_trip(
            records in proptest::collection::vec(
                prop_oneof![
                    arb_record_text(),
                    arb_record_uri(),
                    arb_record_mime(),
                    arb_record_external(),
                ],
                1..6,
            ),
        ) {
            let bytes = encode_message(&records).unwrap();
            let decoded = decode_message(&bytes).unwrap();
            prop_assert_eq!(decoded.len(), records.len());
            for (i, record) in decoded.iter().enumerate() {
                prop_assert_ne!(&record["type"], "undecodable");
                let last = i + 1 == records.len();
                prop_assert_eq!(&record["message_end"], &Value::Bool(last));
            }
        }

        /// Property 2: the message stays inside the two-byte NLEN a Type 4 tag can describe,
        /// and the record count stays inside `MAX_RECORDS`. Both are refusals, not
        /// truncations — a tag written with a body that disagrees with its own NLEN is worse
        /// than a tag not written at all.
        #[test]
        fn messages_are_bounded(records in proptest::collection::vec(arb_record_text(), 1..6)) {
            let bytes = encode_message(&records).unwrap();
            prop_assert!(bytes.len() <= MAX_MESSAGE_LEN);

            let too_many: Vec<Value> = (0..MAX_RECORDS + 1)
                .map(|_| json!({ "type": "text", "text": "x" }))
                .collect();
            prop_assert!(encode_message(&too_many).is_err());
            prop_assert!(encode_message(&[]).is_err());
        }

        /// Values the format cannot carry are refused rather than escaped or truncated: a
        /// control or bidirectional-override character (which changes how a record renders on
        /// a phone without changing what it says), and a URI outside RFC 3986's ASCII.
        #[test]
        fn unsafe_text_and_uris_are_refused(
            bad in prop::sample::select(vec![
                '\u{0000}', '\u{0007}', '\u{001b}', '\u{007f}', '\u{202e}', '\u{2066}',
            ]),
        ) {
            // Bound each result first: `prop_assert!` stringifies its expression into a
            // format string, so a JSON literal inside it is a compile error.
            let unsafe_text = encode_message(&[json!({ "type": "text", "text": format!("a{bad}b") })]);
            let spaced_uri = encode_message(&[json!({ "type": "uri", "uri": "http://a b" })]);
            let empty_uri = encode_message(&[json!({ "type": "uri", "uri": "" })]);
            let nested = encode_message(&[json!({ "type": "nested" })]);
            prop_assert!(unsafe_text.is_err());
            prop_assert!(spaced_uri.is_err());
            prop_assert!(empty_uri.is_err());
            prop_assert!(nested.is_err());
        }
    }

    proptest! {
        #![proptest_config(codec_config!(crate::PANIC_CASES))]

        /// Property 3. A tag is untrusted input and a malformed tail is reported as a final
        /// `undecodable` record rather than an error, so the assertion is that it returns.
        #[test]
        fn arbitrary_bytes_never_panic(data in proptest::collection::vec(any::<u8>(), 0..256)) {
            let _ = decode_message(&data);
        }

        /// Property 3, biased: record headers built from the flag bits that matter (short vs
        /// long payload length, ID present, chunked) in front of arbitrary lengths, which is
        /// where a 32-bit payload length the tag made up would bite.
        #[test]
        fn arbitrary_record_headers_never_panic(
            data in proptest::collection::vec(
                prop::sample::select(vec![
                    0xD1u8, 0x91, 0x51, 0x11, 0xC1, 0x08, 0x20, 0xFF, 0x00, 0x01, b'T', b'U',
                ]),
                0..96,
            )
        ) {
            let _ = decode_message(&data);
        }
    }

    // -------------------------------------------------------------------------------------
    // FINDINGS
    // -------------------------------------------------------------------------------------

    /// A TYPE field of 256 octets or more used to be written in full under a TYPE LENGTH of
    /// 255, so the message did not describe itself. Minimal counterexample: a `mime` record
    /// whose `mime_type` is 256 characters — one octet over — after which the decoder reads
    /// 255 type octets and misreads everything following.
    ///
    /// `encode_one` checks that a `mime_type` is non-empty ASCII and that a `domain_type`
    /// contains a colon; neither bounded the length. The module's own comment said "every
    /// type this encoder produces is a one-byte RTD, a media type or a domain:type, all far
    /// shorter" — true of what a sensible model sends, and an assumption rather than a check.
    #[test]
    fn an_over_long_type_field_is_refused() {
        for over in ["a".repeat(256), "a".repeat(4096)] {
            let record = json!({ "type": "mime", "mime_type": over, "payload_hex": "00" });
            assert!(
                encode_message(std::slice::from_ref(&record)).is_err(),
                "a {}-octet TYPE field must be refused, not written under a TYPE LENGTH of 255",
                over.len()
            );
        }

        // An `external` record's `domain_type` is the other unbounded field, and the same
        // bound has to apply to it.
        let domain = format!("{}:t", "a".repeat(255));
        let record = json!({ "type": "external", "domain_type": domain, "payload_hex": "00" });
        assert!(encode_message(std::slice::from_ref(&record)).is_err());

        // The boundary: 255 octets is the most the TYPE LENGTH can describe, and it still
        // encodes and still describes itself.
        let legal = "a".repeat(255);
        let record = json!({ "type": "mime", "mime_type": legal, "payload_hex": "00" });
        let bytes = encode_message(std::slice::from_ref(&record)).unwrap();
        let decoded = decode_message(&bytes).unwrap();
        assert_eq!(decoded[0]["mime_type"], record["mime_type"]);
    }
}

// ===========================================================================================
// CAN — `src/server/can/frame.rs`
// ===========================================================================================
//
// `CanFrame::validate` used to return `Ok` immediately for an error frame, so the payload
// length check was skipped — and `to_wire_bytes` then indexes `out[8..8 + data.len()]` into a
// fixed 16- or 72-octet buffer. An error frame carrying more than 8 (classic) or 64 (FD)
// octets panicked rather than returning `Err`. The length check now runs for error frames too.

#[cfg(feature = "can")]
mod can_props {
    use netget::server::can::frame::{
        dlc_for_len, len_for_dlc, CanFrame, CANFD_MAX_DLEN, CANFD_MTU, CAN_EFF_MASK, CAN_MAX_DLEN,
        CAN_MTU, CAN_SFF_MASK, FD_DLC_TO_LEN,
    };
    use proptest::prelude::*;

    /// A classic CAN data or remote frame. `rtr_dlc` is meaningless on a data frame and the
    /// wire layout carries no room for it, so it is zero there.
    fn arb_classic() -> impl Strategy<Value = CanFrame> {
        (
            any::<bool>(),
            any::<u32>(),
            proptest::collection::vec(any::<u8>(), 0..=CAN_MAX_DLEN),
            any::<bool>(),
            0u8..=8,
        )
            .prop_map(|(extended, id, data, rtr, rtr_dlc)| CanFrame {
                id: id & if extended { CAN_EFF_MASK } else { CAN_SFF_MASK },
                extended,
                rtr,
                error: false,
                fd: false,
                brs: false,
                esi: false,
                data: if rtr { Vec::new() } else { data },
                rtr_dlc: if rtr { rtr_dlc } else { 0 },
            })
    }

    /// A CAN FD frame. Only the sixteen lengths in the DLC table exist — a nine-byte payload
    /// has no representation, and the codec refuses rather than padding it.
    fn arb_fd() -> impl Strategy<Value = CanFrame> {
        (
            any::<bool>(),
            any::<u32>(),
            prop::sample::select(FD_DLC_TO_LEN.to_vec()),
            any::<bool>(),
            any::<bool>(),
        )
            .prop_flat_map(|(extended, id, len, brs, esi)| {
                proptest::collection::vec(any::<u8>(), len..=len).prop_map(move |data| CanFrame {
                    id: id & if extended { CAN_EFF_MASK } else { CAN_SFF_MASK },
                    extended,
                    rtr: false,
                    error: false,
                    fd: true,
                    brs,
                    esi,
                    data,
                    rtr_dlc: 0,
                })
            })
    }

    proptest! {
        #![proptest_config(codec_config!(crate::CASES))]

        /// Property 1 for the classic `struct can_frame` layout.
        #[test]
        fn classic_frames_round_trip(frame in arb_classic()) {
            let bytes = frame.to_wire_bytes().unwrap();
            prop_assert_eq!(bytes.len(), CAN_MTU);
            prop_assert_eq!(CanFrame::from_wire_bytes(&bytes).unwrap(), frame);
        }

        /// Property 1 for `struct canfd_frame`, where BRS/ESI/FDF live in a flags octet the
        /// classic layout uses as padding.
        #[test]
        fn fd_frames_round_trip(frame in arb_fd()) {
            let bytes = frame.to_wire_bytes().unwrap();
            prop_assert_eq!(bytes.len(), CANFD_MTU);
            prop_assert_eq!(CanFrame::from_wire_bytes(&bytes).unwrap(), frame);
        }

        /// Property 1 for the DLC table, which is `0..=8` and then `12, 16, 20, 24, 32, 48,
        /// 64` — three different step sizes, which is why implementations that compute it
        /// rather than look it up get it wrong.
        #[test]
        fn dlc_round_trips(dlc in 0u8..16) {
            prop_assert_eq!(dlc_for_len(len_for_dlc(dlc, true), true).unwrap(), dlc);
        }

        /// Property 2: a length the format cannot express is refused, not rounded up.
        /// Padding a nine-byte payload to twelve would put three bytes on the wire the model
        /// did not write.
        #[test]
        fn unencodable_lengths_are_refused(len in 0usize..=64) {
            let encodable = FD_DLC_TO_LEN.contains(&len);
            prop_assert_eq!(dlc_for_len(len, true).is_ok(), encodable);
            prop_assert_eq!(dlc_for_len(len, false).is_ok(), len <= CAN_MAX_DLEN);
        }

        /// Property 2: the combinations the bus has no representation for are refused by
        /// name. CAN FD has no remote frames (the RTR bit was reused as RRS); a remote frame
        /// carries no data; the bit-rate switch is an FD feature; an identifier must fit its
        /// declared width.
        #[test]
        fn impossible_frames_are_refused(
            over_11 in (CAN_SFF_MASK + 1)..=CAN_EFF_MASK,
            over_29 in (CAN_EFF_MASK + 1)..=u32::MAX,
            len in (CANFD_MAX_DLEN + 1)..128,
        ) {
            let base = CanFrame {
                id: 0x123,
                extended: false,
                rtr: false,
                error: false,
                fd: false,
                brs: false,
                esi: false,
                data: vec![1, 2, 3],
                rtr_dlc: 0,
            };
            let wide_standard = CanFrame { id: over_11, ..base.clone() };
            let wide_extended = CanFrame {
                id: over_29,
                extended: true,
                ..base.clone()
            };
            let fd_remote = CanFrame {
                fd: true,
                rtr: true,
                data: Vec::new(),
                ..base.clone()
            };
            let remote_with_data = CanFrame { rtr: true, ..base.clone() };
            let classic_brs = CanFrame { brs: true, ..base.clone() };
            let over_long = CanFrame {
                fd: true,
                data: vec![0u8; len],
                ..base
            };
            prop_assert!(wide_standard.validate().is_err());
            prop_assert!(wide_extended.validate().is_err());
            prop_assert!(fd_remote.validate().is_err());
            prop_assert!(remote_with_data.validate().is_err());
            prop_assert!(classic_brs.validate().is_err());
            prop_assert!(over_long.validate().is_err());
        }
    }

    proptest! {
        #![proptest_config(codec_config!(crate::PANIC_CASES))]

        /// Property 3. The two valid lengths are the only ones accepted; everything else is
        /// an `Err` naming both.
        #[test]
        fn arbitrary_bytes_never_panic(data in proptest::collection::vec(any::<u8>(), 0..80)) {
            let valid = data.len() == CAN_MTU || data.len() == CANFD_MTU;
            prop_assert_eq!(CanFrame::from_wire_bytes(&data).is_ok(), valid);
        }

        /// Property 3, biased: a correctly sized buffer whose identifier word and length
        /// octet the peer chose, which is where the flag decoding and the `len` clamp live.
        #[test]
        fn arbitrary_well_sized_frames_never_panic(
            id_word in any::<u32>(),
            len in any::<u8>(),
            flags in any::<u8>(),
            fd in any::<bool>(),
        ) {
            let mut data = vec![0u8; if fd { CANFD_MTU } else { CAN_MTU }];
            data[0..4].copy_from_slice(&id_word.to_le_bytes());
            data[4] = len;
            data[5] = flags;
            let frame = CanFrame::from_wire_bytes(&data).unwrap();
            // Whatever came off the bus must also survive being described to the model.
            let _ = frame.describe();
            let _ = frame.to_event_data();
            let _ = frame.error_classes();
            let _ = frame.bus_state();
        }
    }

    // -------------------------------------------------------------------------------------
    // FINDINGS
    // -------------------------------------------------------------------------------------

    /// `validate` short-circuits on an error frame — reasonably, since an error frame's
    /// identifier is a class bitmask and the 11/29-bit rules do not apply to it — but it used
    /// to short-circuit *before* the payload length check too. `to_wire_bytes` then wrote into
    /// `out[8..8 + data.len()]` of a 16-octet buffer and **panicked**.
    ///
    /// Minimal counterexample: `CanFrame { error: true, data: vec![0; 9], .. }`, classic.
    ///
    /// Latent rather than live: `from_action` never sets `error: true` (the model cannot
    /// build one), and the only other producer is `from_wire_bytes`, which clamps the length
    /// to 8 or 64 first. It was a panic in a `pub fn` on a struct with `pub` fields, and a
    /// panic inside a connection task is swallowed by `tokio::spawn` — the failure mode this
    /// repository has hit three times. The error-frame early return now sits *below* the
    /// `dlc_for_len` check.
    #[test]
    fn an_over_long_error_frame_is_refused_not_a_panic() {
        let frame = CanFrame {
            id: 0x04,
            extended: false,
            rtr: false,
            error: true,
            fd: false,
            brs: false,
            esi: false,
            data: vec![0u8; 9],
            rtr_dlc: 0,
        };
        assert!(
            frame.validate().is_err(),
            "an error frame with a 9-octet payload must be refused before to_wire_bytes \
             indexes past its 16-octet buffer"
        );
        // The panic was inside `to_wire_bytes`, which calls `validate` first — so the refusal
        // has to reach there, not merely be available to a caller who thinks to ask.
        assert!(frame.to_wire_bytes().is_err());

        // An FD error frame has the same shape, and a length that is not one of the sixteen
        // encodable FD sizes is refused for an error frame as for any other.
        let fd = CanFrame {
            fd: true,
            data: vec![0u8; 65],
            ..frame.clone()
        };
        assert!(fd.validate().is_err());
        assert!(fd.to_wire_bytes().is_err());

        // An error frame carrying a legal payload is still accepted: this is a bound, not a
        // ban on error frames.
        let legal = CanFrame {
            data: vec![0u8; 8],
            ..frame
        };
        assert!(legal.validate().is_ok());
        assert_eq!(legal.to_wire_bytes().unwrap().len(), CAN_MTU);
    }
}
