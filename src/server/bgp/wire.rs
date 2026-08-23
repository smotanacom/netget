//! BGP-4 wire format: encoding, decoding, and the model-facing JSON projection.
//!
//! # Why netgauze rather than a hand-rolled codec
//!
//! Everything on this path is attacker-controlled. Capability TLVs, path-attribute flags,
//! extended attribute lengths, the two-byte/four-byte AS_PATH ambiguity of RFC 6793 and the
//! multiprotocol attributes of RFC 4760 are all places where a plausible-looking hand-rolled
//! parser silently disagrees with a real router. `netgauze-bgp-pkt` was already a declared
//! dependency of the `bgp` feature and was not used by a single line of code; it is a
//! maintained implementation of RFC 4271 and the surrounding RFCs, so all parsing and the
//! structurally interesting encodings (OPEN, UPDATE) now go through it.
//!
//! Access is via [`netgauze_bgp_pkt::codec::BgpCodec`], which implements `tokio_util`'s
//! `Encoder`/`Decoder`. That matters for a mundane reason: netgauze's more direct
//! `WritablePdu`/`ReadablePduWithOneInput` traits live in `netgauze-parse-utils`, which is not
//! a declared dependency of this crate, and neither is `ipnet`. The codec route needs only
//! `netgauze-bgp-pkt`, `tokio-util` and `bytes`, all of which are already present.
//!
//! Two consequences of that constraint are visible below and both would disappear if
//! `netgauze-parse-utils` and `ipnet` were added to `Cargo.toml`:
//!
//! * [`ipv4_unicast`] builds an [`Ipv4Unicast`] through serde instead of `Ipv4Net`.
//! * A per-message codec is constructed rather than reusing one.
//!
//! # What is deliberately *not* netgauze
//!
//! KEEPALIVE and NOTIFICATION are hand-encoded. KEEPALIVE is a bare header. NOTIFICATION is a
//! header plus two octets, and netgauze models it as a closed enum of the (code, subcode) pairs
//! it knows, which cannot represent an arbitrary subcode. Since `send_bgp_notification` is an
//! LLM-facing action taking numeric codes, hand-encoding is both simpler and strictly more
//! expressive. Neither message has any structure to get wrong.

use anyhow::{anyhow, bail, Context, Result};
use bytes::BytesMut;
use std::net::Ipv4Addr;
use tokio_util::codec::{Decoder, Encoder};

use netgauze_bgp_pkt::{
    capabilities::{BgpCapability, FourOctetAsCapability},
    codec::BgpCodec,
    nlri::{Ipv4Unicast, Ipv4UnicastAddress},
    open::{BgpOpenMessage, BgpOpenMessageParameter},
    path_attribute::{
        As2PathSegment, As4Path, As4PathSegment, AsPath, AsPathSegmentType, LocalPreference,
        MultiExitDiscriminator, NextHop, Origin, PathAttribute, PathAttributeValue,
    },
    update::BgpUpdateMessage,
    BgpMessage,
};

/// Every BGP message begins with 16 octets of ones (RFC 4271 section 4.1).
pub const BGP_MARKER: [u8; 16] = [0xff; 16];

/// Marker (16) + length (2) + type (1).
pub const BGP_HEADER_LEN: usize = 19;

/// RFC 4271 section 4: no message may exceed 4096 octets. RFC 8654 raises this for peers that
/// negotiate the extended-message capability; NetGet does not advertise it, so the hard limit
/// applies in both directions and is enforced on input before a single body byte is read.
pub const BGP_MAX_MESSAGE_LEN: usize = 4096;

/// RFC 6793: the reserved two-octet ASN a four-octet speaker puts in the OPEN `My Autonomous
/// System` field when its real ASN does not fit.
pub const AS_TRANS: u16 = 23456;

/// BGP message type octets (RFC 4271 section 4.1).
pub const MSG_OPEN: u8 = 1;
pub const MSG_UPDATE: u8 = 2;
pub const MSG_NOTIFICATION: u8 = 3;
pub const MSG_KEEPALIVE: u8 = 4;
pub const MSG_ROUTE_REFRESH: u8 = 5;

/// A NOTIFICATION (error code, error subcode) pair.
pub type NotifyCode = (u8, u8);

// Message Header Error (RFC 4271 section 6.1)
pub const ERR_HEADER: u8 = 1;
pub const SUB_CONNECTION_NOT_SYNCHRONIZED: u8 = 1;
pub const SUB_BAD_MESSAGE_LENGTH: u8 = 2;
pub const SUB_BAD_MESSAGE_TYPE: u8 = 3;
// OPEN Message Error (RFC 4271 section 6.2)
pub const ERR_OPEN: u8 = 2;
pub const SUB_UNSUPPORTED_VERSION: u8 = 1;
pub const SUB_BAD_PEER_AS: u8 = 2;
pub const SUB_BAD_BGP_IDENTIFIER: u8 = 3;
pub const SUB_UNACCEPTABLE_HOLD_TIME: u8 = 6;
// UPDATE Message Error (RFC 4271 section 6.3)
pub const ERR_UPDATE: u8 = 3;
// Hold Timer Expired (RFC 4271 section 6.5)
pub const ERR_HOLD_TIMER_EXPIRED: u8 = 4;
// Finite State Machine Error (RFC 4271 section 6.6)
pub const ERR_FSM: u8 = 5;
pub const SUB_FSM_OPENSENT: u8 = 1;
pub const SUB_FSM_OPENCONFIRM: u8 = 2;
pub const SUB_FSM_ESTABLISHED: u8 = 3;
// Cease (RFC 4271 section 6.7; subcodes from RFC 4486 section 3)
pub const ERR_CEASE: u8 = 6;
pub const SUB_CEASE_ADMIN_SHUTDOWN: u8 = 2;
/// RFC 4486 section 4: the speaker is rejecting this connection. Permanent as far as the peer
/// is concerned — it should not treat the rejection as a transient resource condition.
pub const SUB_CEASE_CONNECTION_REJECTED: u8 = 5;
/// RFC 4486 section 8: the speaker cannot carry the peering right now for want of resources.
/// A conforming implementation applies a damping/backoff to this subcode and retries, which is
/// exactly the behaviour a saturated backend wants.
pub const SUB_CEASE_OUT_OF_RESOURCES: u8 = 8;

/// Why a 19-octet header was rejected, and the NOTIFICATION it earns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum HeaderError {
    /// Marker was not all ones — the stream is out of sync and cannot be resynchronised.
    BadMarker,
    /// Length outside \[19, 4096\], or below the minimum for the declared type.
    BadLength(u16),
}

impl HeaderError {
    pub fn notify_code(&self) -> NotifyCode {
        match self {
            Self::BadMarker => (ERR_HEADER, SUB_CONNECTION_NOT_SYNCHRONIZED),
            Self::BadLength(_) => (ERR_HEADER, SUB_BAD_MESSAGE_LENGTH),
        }
    }

    /// RFC 4271 section 6.1 says the Data field carries the erroneous length.
    pub fn notify_data(&self) -> Vec<u8> {
        match self {
            Self::BadMarker => Vec::new(),
            Self::BadLength(len) => len.to_be_bytes().to_vec(),
        }
    }
}

/// Validate a BGP message header without touching the body.
///
/// Returns `(total_message_length, message_type)`. The length is the *whole* message including
/// these 19 octets, exactly as it appears on the wire, and is guaranteed on return to be in
/// `[BGP_HEADER_LEN, BGP_MAX_MESSAGE_LEN]` — so `len - BGP_HEADER_LEN` cannot underflow and
/// allocating `len` bytes is bounded.
///
/// The per-type minimums come from RFC 4271 section 4.1: OPEN is at least 29, UPDATE at least
/// 23, NOTIFICATION at least 21, KEEPALIVE exactly 19.
pub fn parse_header(header: &[u8; BGP_HEADER_LEN]) -> Result<(usize, u8), HeaderError> {
    if header[..16] != BGP_MARKER {
        return Err(HeaderError::BadMarker);
    }
    let len = u16::from_be_bytes([header[16], header[17]]);
    let msg_type = header[18];

    if (len as usize) < BGP_HEADER_LEN || (len as usize) > BGP_MAX_MESSAGE_LEN {
        return Err(HeaderError::BadLength(len));
    }
    let min = match msg_type {
        MSG_OPEN => 29,
        MSG_UPDATE => 23,
        MSG_NOTIFICATION => 21,
        MSG_KEEPALIVE => 19,
        // ROUTE-REFRESH is 23; an unknown type is not a length error, it is a type error, and
        // is reported as such after the body has been consumed so the stream stays in sync.
        MSG_ROUTE_REFRESH => 23,
        _ => BGP_HEADER_LEN as u16,
    };
    if len < min {
        return Err(HeaderError::BadLength(len));
    }
    Ok((len as usize, msg_type))
}

/// Decode one complete BGP message (header included).
///
/// `asn4` must be the *negotiated* four-octet-AS state: it decides whether AS_PATH in an UPDATE
/// is read as two-octet or four-octet ASNs. Getting it wrong does not fail loudly, it silently
/// yields the wrong AS path, which is why it is threaded through from the session rather than
/// defaulted.
///
/// The error carries the NOTIFICATION that RFC 4271 section 6 prescribes for that failure.
pub fn decode(bytes: &[u8], asn4: bool) -> std::result::Result<BgpMessage, DecodeError> {
    let mut codec = BgpCodec::new(asn4);
    let mut buf = BytesMut::from(bytes);
    match codec.decode(&mut buf) {
        Ok(Some((msg, _ignored))) => Ok(msg),
        // `decode` returns Ok(None) when it wants more bytes. The caller framed the message
        // from its own length field, so this means the length field disagreed with the body.
        Ok(None) => Err(DecodeError {
            notify: (ERR_HEADER, SUB_BAD_MESSAGE_LENGTH),
            detail: "message body shorter than its length field".to_string(),
        }),
        Err(e) => Err(DecodeError::from_codec(e)),
    }
}

/// A message that could not be parsed, together with the NOTIFICATION it earns.
#[derive(Debug, Clone)]
pub struct DecodeError {
    pub notify: NotifyCode,
    pub detail: String,
}

impl std::fmt::Display for DecodeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{} (NOTIFICATION {}/{})",
            self.detail, self.notify.0, self.notify.1
        )
    }
}

impl DecodeError {
    fn from_codec(e: netgauze_bgp_pkt::codec::BgpCodecDecoderError) -> Self {
        use netgauze_bgp_pkt::codec::BgpCodecDecoderError as E;
        use netgauze_bgp_pkt::wire::deserializer::open::BgpOpenMessageParsingError as OpenErr;
        use netgauze_bgp_pkt::wire::deserializer::BgpMessageParsingError as MsgErr;

        let detail = format!("{e:?}");
        let notify = match &e {
            E::IoError(_) => (ERR_CEASE, 0),
            E::Incomplete(_) => (ERR_HEADER, SUB_BAD_MESSAGE_LENGTH),
            E::BgpMessageParsingError(inner) => match inner {
                MsgErr::ConnectionNotSynchronized(_) => {
                    (ERR_HEADER, SUB_CONNECTION_NOT_SYNCHRONIZED)
                }
                MsgErr::BadMessageLength(_) => (ERR_HEADER, SUB_BAD_MESSAGE_LENGTH),
                MsgErr::UndefinedBgpMessageType(_) => (ERR_HEADER, SUB_BAD_MESSAGE_TYPE),
                MsgErr::BgpOpenMessageParsingError(open) => match open {
                    OpenErr::UnsupportedVersionNumber(_) => (ERR_OPEN, SUB_UNSUPPORTED_VERSION),
                    OpenErr::UnacceptableHoldTime(_) => (ERR_OPEN, SUB_UNACCEPTABLE_HOLD_TIME),
                    OpenErr::InvalidBgpId(_) => (ERR_OPEN, SUB_BAD_BGP_IDENTIFIER),
                    _ => (ERR_OPEN, 0),
                },
                MsgErr::BgpUpdateMessageParsingError(_) => (ERR_UPDATE, 0),
                // A malformed NOTIFICATION, or anything else, is not worth a reply beyond
                // closing: RFC 4271 forbids answering a NOTIFICATION with a NOTIFICATION.
                _ => (ERR_CEASE, 0),
            },
        };
        Self { notify, detail }
    }
}

/// Encode a message netgauze can represent (OPEN, UPDATE, KEEPALIVE, ROUTE-REFRESH).
///
/// The encoder is a pure function of the message value — in particular the codec's `asn4` flag
/// does *not* influence the output, so the two/four-octet AS_PATH choice must be made when the
/// [`AsPath`] is built. See [`build_update`].
pub fn encode(msg: BgpMessage) -> Result<Vec<u8>> {
    let mut codec = BgpCodec::default();
    let mut out = BytesMut::new();
    codec
        .encode(msg, &mut out)
        .map_err(|e| anyhow!("BGP encoding failed: {e:?}"))?;
    if out.len() > BGP_MAX_MESSAGE_LEN {
        bail!(
            "BGP message is {} bytes, over the RFC 4271 maximum of {}",
            out.len(),
            BGP_MAX_MESSAGE_LEN
        );
    }
    Ok(out.to_vec())
}

/// A bare 19-octet KEEPALIVE (RFC 4271 section 4.4).
pub fn encode_keepalive() -> Vec<u8> {
    let mut msg = Vec::with_capacity(BGP_HEADER_LEN);
    msg.extend_from_slice(&BGP_MARKER);
    msg.extend_from_slice(&(BGP_HEADER_LEN as u16).to_be_bytes());
    msg.push(MSG_KEEPALIVE);
    msg
}

/// A NOTIFICATION (RFC 4271 section 4.5): header, error code, error subcode, opaque data.
///
/// Hand-encoded on purpose — see the module comment.
pub fn encode_notification(error_code: u8, error_subcode: u8, data: &[u8]) -> Result<Vec<u8>> {
    let total = BGP_HEADER_LEN + 2 + data.len();
    if total > BGP_MAX_MESSAGE_LEN {
        bail!(
            "BGP NOTIFICATION with {} data bytes exceeds the {}-byte maximum",
            data.len(),
            BGP_MAX_MESSAGE_LEN
        );
    }
    let mut msg = Vec::with_capacity(total);
    msg.extend_from_slice(&BGP_MARKER);
    msg.extend_from_slice(&(total as u16).to_be_bytes());
    msg.push(MSG_NOTIFICATION);
    msg.push(error_code);
    msg.push(error_subcode);
    msg.extend_from_slice(data);
    Ok(msg)
}

/// Build our OPEN.
///
/// RFC 6793: the two-octet `My Autonomous System` field carries [`AS_TRANS`] when the real ASN
/// does not fit, and the real value always travels in the four-octet-AS capability. NetGet
/// advertises that capability unconditionally, which is both what every current implementation
/// does and what lets the read side decode AS_PATH correctly whenever the peer agrees.
///
/// The previous implementation wrote `local_as as u16` with no capability, so any ASN above
/// 65535 was silently truncated to a different, valid-looking ASN.
pub fn build_open(local_as: u32, hold_time: u16, router_id: Ipv4Addr) -> BgpMessage {
    let my_as = u16::try_from(local_as).unwrap_or(AS_TRANS);
    BgpMessage::Open(BgpOpenMessage::new(
        my_as,
        hold_time,
        router_id,
        vec![BgpOpenMessageParameter::Capabilities(vec![
            BgpCapability::FourOctetAs(FourOctetAsCapability::new(local_as)),
        ])],
    ))
}

/// Parse `a.b.c.d/len` into a netgauze NLRI prefix, masking host bits.
///
/// Host bits matter: a prefix is written as `len` followed by `ceil(len/8)` octets, so
/// `10.0.0.17/28` would otherwise put the `17` on the wire inside the host part.
///
/// The `Ipv4Unicast` newtype wraps `ipnet::Ipv4Net`, which this crate does not depend on
/// directly; serde is the available route to a value. It is a plain string round-trip and it
/// bypasses netgauze's own unicast check, so the multicast/broadcast rejection is done here.
pub fn ipv4_unicast(prefix: &str) -> Result<Ipv4Unicast> {
    let (addr_str, len_str) = prefix
        .split_once('/')
        .with_context(|| format!("BGP prefix {prefix:?} must be in CIDR form, e.g. 10.0.0.0/24"))?;
    let addr: Ipv4Addr = addr_str
        .parse()
        .with_context(|| format!("BGP prefix {prefix:?} has an invalid IPv4 address"))?;
    let len: u8 = len_str
        .parse()
        .with_context(|| format!("BGP prefix {prefix:?} has an invalid prefix length"))?;
    if len > 32 {
        bail!("BGP prefix {prefix:?} has prefix length {len}, which exceeds 32");
    }
    if addr.is_multicast() || addr.is_broadcast() {
        bail!("BGP prefix {prefix:?} is not a unicast address");
    }

    let mask: u32 = if len == 0 {
        0
    } else {
        u32::MAX << (32 - u32::from(len))
    };
    let network = Ipv4Addr::from(u32::from(addr) & mask);

    serde_json::from_value::<Ipv4Unicast>(serde_json::Value::String(format!("{network}/{len}")))
        .with_context(|| format!("BGP prefix {prefix:?} is not a valid IPv4 network"))
}

/// Everything `send_bgp_update` needs, already validated.
#[derive(Debug, Clone)]
pub struct UpdateIntent {
    pub withdrawn: Vec<String>,
    pub nlri: Vec<String>,
    pub next_hop: Option<Ipv4Addr>,
    pub as_path: Vec<u32>,
    pub origin: Origin,
    pub med: Option<u32>,
    pub local_pref: Option<u32>,
}

/// Build an UPDATE.
///
/// `peer_asn4` is the negotiated four-octet-AS state and it changes the bytes on the wire:
///
/// * negotiated — AS_PATH carries four-octet ASNs (RFC 6793 section 4).
/// * not negotiated — AS_PATH must carry two-octet ASNs. Any ASN that does not fit is replaced
///   by [`AS_TRANS`] and the true path is added as the optional-transitive AS4_PATH attribute.
///   Sending a four-octet AS_PATH to a peer that did not negotiate it is a protocol violation
///   that a real router answers with NOTIFICATION 3/11 (Malformed AS_PATH).
///
/// RFC 4271 section 9.1 makes ORIGIN, AS_PATH and NEXT_HOP mandatory whenever NLRI is present;
/// a withdrawal-only UPDATE carries no attributes at all.
pub fn build_update(intent: &UpdateIntent, peer_asn4: bool) -> Result<BgpMessage> {
    let withdrawn = intent
        .withdrawn
        .iter()
        .map(|p| ipv4_unicast(p).map(Ipv4UnicastAddress::new_no_path_id))
        .collect::<Result<Vec<_>>>()?;
    let nlri = intent
        .nlri
        .iter()
        .map(|p| ipv4_unicast(p).map(Ipv4UnicastAddress::new_no_path_id))
        .collect::<Result<Vec<_>>>()?;

    if withdrawn.is_empty() && nlri.is_empty() {
        bail!("send_bgp_update needs at least one prefix in nlri or withdrawn_routes");
    }

    let mut attrs: Vec<PathAttribute> = Vec::new();

    if !nlri.is_empty() {
        let next_hop = intent
            .next_hop
            .context("send_bgp_update with nlri requires next_hop (RFC 4271 makes NEXT_HOP mandatory for an announcement)")?;

        attrs.push(well_known(PathAttributeValue::Origin(intent.origin))?);

        if peer_asn4 {
            let segments = if intent.as_path.is_empty() {
                vec![]
            } else {
                vec![As4PathSegment::new(
                    AsPathSegmentType::AsSequence,
                    intent.as_path.clone(),
                )]
            };
            attrs.push(well_known(PathAttributeValue::AsPath(
                AsPath::As4PathSegments(segments),
            ))?);
        } else {
            let needs_as4 = intent.as_path.iter().any(|&asn| asn > u32::from(u16::MAX));
            let segments = if intent.as_path.is_empty() {
                vec![]
            } else {
                vec![As2PathSegment::new(
                    AsPathSegmentType::AsSequence,
                    intent
                        .as_path
                        .iter()
                        .map(|&asn| u16::try_from(asn).unwrap_or(AS_TRANS))
                        .collect(),
                )]
            };
            attrs.push(well_known(PathAttributeValue::AsPath(
                AsPath::As2PathSegments(segments),
            ))?);
            if needs_as4 {
                // Optional (true), transitive (true), not partial.
                attrs.push(
                    PathAttribute::from(
                        true,
                        true,
                        false,
                        false,
                        PathAttributeValue::As4Path(As4Path::new(vec![As4PathSegment::new(
                            AsPathSegmentType::AsSequence,
                            intent.as_path.clone(),
                        )])),
                    )
                    .map_err(|(_, e)| anyhow!("invalid AS4_PATH attribute: {e:?}"))?,
                );
            }
        }

        attrs.push(well_known(PathAttributeValue::NextHop(NextHop::new(
            next_hop,
        )))?);

        if let Some(local_pref) = intent.local_pref {
            attrs.push(well_known(PathAttributeValue::LocalPreference(
                LocalPreference::new(local_pref),
            ))?);
        }
        if let Some(med) = intent.med {
            // MULTI_EXIT_DISC is optional non-transitive.
            attrs.push(
                PathAttribute::from(
                    true,
                    false,
                    false,
                    false,
                    PathAttributeValue::MultiExitDiscriminator(MultiExitDiscriminator::new(med)),
                )
                .map_err(|(_, e)| anyhow!("invalid MULTI_EXIT_DISC attribute: {e:?}"))?,
            );
        }
    }

    Ok(BgpMessage::Update(BgpUpdateMessage::new(
        withdrawn, attrs, nlri,
    )))
}

/// Well-known mandatory: optional=false, transitive=true, partial=false, one-octet length.
fn well_known(value: PathAttributeValue) -> Result<PathAttribute> {
    PathAttribute::from(false, true, false, false, value)
        .map_err(|(_, e)| anyhow!("invalid well-known path attribute: {e:?}"))
}

/// Render a decoded UPDATE as the structured JSON handed to the model.
///
/// Deliberately field-per-concept rather than a hex blob: the root CLAUDE.md forbids putting
/// raw bytes in event data, and this body used to be delivered as `hex::encode(body)`, which no
/// model can act on.
pub fn update_to_json(update: &BgpUpdateMessage) -> serde_json::Value {
    let withdrawn: Vec<String> = update
        .withdraw_routes()
        .iter()
        .map(|r| r.network().to_string())
        .collect();
    let nlri: Vec<String> = update
        .nlri()
        .iter()
        .map(|r| r.network().to_string())
        .collect();

    let attributes: Vec<serde_json::Value> = update
        .path_attributes()
        .iter()
        .map(path_attribute_to_json)
        .collect();

    // Convenience projections so a handler does not have to walk the attribute list for the
    // three fields a routing decision almost always turns on.
    let mut origin = serde_json::Value::Null;
    let mut next_hop = serde_json::Value::Null;
    let mut as_path = serde_json::Value::Null;
    for attr in update.path_attributes() {
        match attr.value() {
            PathAttributeValue::Origin(o) => origin = serde_json::json!(format!("{o:?}")),
            PathAttributeValue::NextHop(n) => {
                next_hop = serde_json::json!(n.next_hop().to_string())
            }
            PathAttributeValue::AsPath(p) => {
                as_path = serde_json::json!(Vec::<u32>::from(p.clone()))
            }
            _ => {}
        }
    }

    serde_json::json!({
        "withdrawn_routes": withdrawn,
        "nlri": nlri,
        "origin": origin,
        "next_hop": next_hop,
        "as_path": as_path,
        "path_attributes": attributes,
        "end_of_rib": update.end_of_rib().is_some(),
    })
}

fn path_attribute_to_json(attr: &PathAttribute) -> serde_json::Value {
    let type_name = match attr.path_attribute_type() {
        Ok(t) => format!("{t:?}"),
        Err(code) => format!("UNKNOWN({code})"),
    };
    let mut out = serde_json::json!({
        "type_name": type_name,
        "optional": attr.optional(),
        "transitive": attr.transitive(),
    });

    match attr.value() {
        PathAttributeValue::Origin(o) => out["origin"] = serde_json::json!(format!("{o:?}")),
        PathAttributeValue::AsPath(p) => {
            out["as_path"] = serde_json::json!(Vec::<u32>::from(p.clone()));
            out["four_octet"] = serde_json::json!(matches!(p, AsPath::As4PathSegments(_)));
        }
        PathAttributeValue::As4Path(p) => {
            let asns: Vec<u32> = p
                .segments()
                .iter()
                .flat_map(|s| s.as_numbers().iter().copied())
                .collect();
            out["as4_path"] = serde_json::json!(asns);
        }
        PathAttributeValue::NextHop(n) => {
            out["next_hop"] = serde_json::json!(n.next_hop().to_string())
        }
        PathAttributeValue::MultiExitDiscriminator(m) => out["med"] = serde_json::json!(m.metric()),
        PathAttributeValue::LocalPreference(l) => out["local_pref"] = serde_json::json!(l.metric()),
        PathAttributeValue::AtomicAggregate(_) => out["atomic_aggregate"] = serde_json::json!(true),
        PathAttributeValue::Communities(c) => {
            let communities: Vec<String> = c
                .communities()
                .iter()
                .map(|c| format!("{}:{}", c.collection_asn(), c.collection_value()))
                .collect();
            out["communities"] = serde_json::json!(communities);
        }
        // Everything else (MP_REACH/MP_UNREACH, aggregator, large/extended communities,
        // BGP-LS, ...) is reported by name only. It is parsed correctly by netgauze; it is
        // simply not projected into a shape a model can usefully reason about yet.
        _ => {}
    }
    out
}

/// Describe our own capabilities and the peer's for the `bgp_open` event.
pub fn capabilities_to_json(open: &BgpOpenMessage) -> serde_json::Value {
    let names: Vec<String> = open
        .capabilities()
        .into_iter()
        .map(|c| match c {
            BgpCapability::FourOctetAs(a) => format!("four_octet_as({})", a.asn4()),
            other => format!("{other:?}")
                .split_whitespace()
                .next()
                .unwrap_or("unknown")
                .trim_end_matches('(')
                .to_string(),
        })
        .collect();
    serde_json::json!(names)
}

/// Human-readable RFC 4271 section 6 error names, for logs and for the model.
pub fn error_name(code: u8) -> &'static str {
    match code {
        1 => "Message Header Error",
        2 => "OPEN Message Error",
        3 => "UPDATE Message Error",
        4 => "Hold Timer Expired",
        5 => "Finite State Machine Error",
        6 => "Cease",
        _ => "Unknown",
    }
}

pub fn error_subcode_name(code: u8, subcode: u8) -> &'static str {
    match (code, subcode) {
        (1, 1) => "Connection Not Synchronized",
        (1, 2) => "Bad Message Length",
        (1, 3) => "Bad Message Type",
        (2, 1) => "Unsupported Version Number",
        (2, 2) => "Bad Peer AS",
        (2, 3) => "Bad BGP Identifier",
        (2, 4) => "Unsupported Optional Parameter",
        (2, 6) => "Unacceptable Hold Time",
        (2, 7) => "Unsupported Capability",
        (3, 1) => "Malformed Attribute List",
        (3, 2) => "Unrecognized Well-known Attribute",
        (3, 3) => "Missing Well-known Attribute",
        (3, 4) => "Attribute Flags Error",
        (3, 5) => "Attribute Length Error",
        (3, 6) => "Invalid ORIGIN Attribute",
        (3, 8) => "Invalid NEXT_HOP Attribute",
        (3, 9) => "Optional Attribute Error",
        (3, 10) => "Invalid Network Field",
        (3, 11) => "Malformed AS_PATH",
        (5, 1) => "Unexpected Message in OpenSent State",
        (5, 2) => "Unexpected Message in OpenConfirm State",
        (5, 3) => "Unexpected Message in Established State",
        (6, 1) => "Maximum Number of Prefixes Reached",
        (6, 2) => "Administrative Shutdown",
        (6, 3) => "Peer De-configured",
        (6, 4) => "Administrative Reset",
        (6, 5) => "Connection Rejected",
        (6, 6) => "Other Configuration Change",
        (6, 7) => "Connection Collision Resolution",
        (6, 8) => "Out of Resources",
        _ => "Unspecified",
    }
}

/// Turn the validated intent produced by a `send_bgp_*` action into wire bytes.
///
/// The action executor cannot do this itself: `Protocol::execute_action` is a pure function of
/// the action JSON with no access to the session, and the correct encoding of an UPDATE depends
/// on whether four-octet AS was negotiated with *this* peer. So the executor validates and
/// normalises, returning `ActionResult::Custom`, and the session calls this with the negotiated
/// state.
pub fn encode_intent(intent: &serde_json::Value, peer_asn4: bool) -> Result<Vec<u8>> {
    let kind = intent
        .get("kind")
        .and_then(|v| v.as_str())
        .context("BGP action intent is missing its 'kind'")?;

    match kind {
        "open" => {
            let my_as = intent
                .get("my_as")
                .and_then(|v| v.as_u64())
                .context("BGP open intent is missing my_as")? as u32;
            let hold_time = intent
                .get("hold_time")
                .and_then(|v| v.as_u64())
                .context("BGP open intent is missing hold_time")?
                as u16;
            let router_id: Ipv4Addr = intent
                .get("router_id")
                .and_then(|v| v.as_str())
                .context("BGP open intent is missing router_id")?
                .parse()
                .context("BGP open intent has an invalid router_id")?;
            encode(build_open(my_as, hold_time, router_id))
        }
        "keepalive" => Ok(encode_keepalive()),
        "notification" => {
            let code = intent
                .get("error_code")
                .and_then(|v| v.as_u64())
                .context("BGP notification intent is missing error_code")?
                as u8;
            let subcode = intent
                .get("error_subcode")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as u8;
            let data = match intent.get("data").and_then(|v| v.as_str()) {
                Some(hex_str) if !hex_str.is_empty() => {
                    hex::decode(hex_str).context("BGP notification intent has non-hex data")?
                }
                _ => Vec::new(),
            };
            encode_notification(code, subcode, &data)
        }
        "update" => {
            let strings = |key: &str| -> Vec<String> {
                intent
                    .get(key)
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_str().map(str::to_string))
                            .collect()
                    })
                    .unwrap_or_default()
            };
            let next_hop = match intent.get("next_hop").and_then(|v| v.as_str()) {
                Some(s) => Some(
                    s.parse::<Ipv4Addr>()
                        .context("BGP update intent has an invalid next_hop")?,
                ),
                None => None,
            };
            let origin = match intent.get("origin").and_then(|v| v.as_str()) {
                Some("EGP") => Origin::EGP,
                Some("INCOMPLETE") => Origin::Incomplete,
                _ => Origin::IGP,
            };
            let intent = UpdateIntent {
                withdrawn: strings("withdrawn_routes"),
                nlri: strings("nlri"),
                next_hop,
                as_path: intent
                    .get("as_path")
                    .and_then(|v| v.as_array())
                    .map(|a| {
                        a.iter()
                            .filter_map(|v| v.as_u64())
                            .map(|n| n as u32)
                            .collect()
                    })
                    .unwrap_or_default(),
                origin,
                med: intent.get("med").and_then(|v| v.as_u64()).map(|n| n as u32),
                local_pref: intent
                    .get("local_pref")
                    .and_then(|v| v.as_u64())
                    .map(|n| n as u32),
            };
            encode(build_update(&intent, peer_asn4)?)
        }
        other => bail!("unknown BGP action intent kind {other:?}"),
    }
}
