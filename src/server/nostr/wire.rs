//! Nostr NIP-01 on the wire: client messages in, relay messages out, and the two things NetGet
//! owns outright — an event's id and its signature.
//!
//! Everything here is pure: no socket, no model, no state. The server loop in `mod.rs` calls
//! [`parse_client_message`] on every text frame and gets back either a message worth deciding
//! about or a [`Refusal`] it answers itself.
//!
//! # The id is recomputed, the signature is checked, before anything else sees the event
//!
//! An `EVENT` whose `id` is not the SHA-256 of its own canonical serialisation, or whose `sig`
//! is not a valid BIP-340 signature of that id by its `pubkey`, is not an event at all — it is
//! answered `["OK", <id>, false, "invalid: …"]` here and never reaches the model. There is no
//! decision in it, and a model asked to judge a forged event could only get it wrong.
//!
//! # The canonical serialisation, and where the implementations disagree
//!
//! NIP-01 defines the id as `sha256` of `[0,<pubkey>,<created_at>,<kind>,<tags>,<content>]` with
//! no whitespace, `\n \" \\ \r \t \b \f` escaped as shown and "all other characters included
//! verbatim". For text without C0 control characters other than `\n \r \t` every
//! implementation agrees and there is one serialisation. For text with them, there are three
//! in use, measured rather than assumed (`tests/server/nostr/wire_test.rs`):
//!
//! - **JSON** — `\b` and `\f` named, every other C0 control `\u00xx`. What `serde_json`,
//!   nostr-tools (`JSON.stringify`) and rust-nostr write.
//! - **NIP-01 as written** — `\b` and `\f` named, every other C0 control raw.
//! - **go-nostr** — only `\n \" \\ \r \t` escaped; `\b`, `\f` and the rest raw. nak 0.20.7
//!   signs this way: an event it signed with a U+0008 in its content verifies under this form
//!   and under neither of the others.
//!
//! A relay that knew one of them would refuse events from the others' users as forged. So
//! [`verify_event`] accepts an id computed under any of the three — the signature is over the
//! id, so accepting a second serialisation admits no event its author did not sign — and
//! computes only the first when the text has nothing the three disagree about. Events NetGet
//! signs itself are serialised the JSON way and never contain such a character:
//! [`parse_supplied_event`] drops C0 controls other than `\n \r \t` from what the model
//! supplies, so every client computes the same id for them.
//!
//! # Depth
//!
//! Frames are parsed with `serde_json`, whose recursion limit (128) is on in this tree — no
//! crate enables `unbounded_depth` at any feature set — so a depth bomb is a parse error, not a
//! stack overflow. That bound is enough for stack safety; what reaches the model is bounded far
//! tighter by the schema checks below, which accept tags only as arrays of strings and filter
//! values only as scalars or arrays of scalars.

use secp256k1::{schnorr, Keypair, Message, Secp256k1, SecretKey, XOnlyPublicKey};
use serde_json::{json, Map, Value};
use sha2::{Digest, Sha256};
use std::sync::LazyLock;

/// Largest WebSocket message (and frame) accepted from a client, in bytes. strfry's default
/// `maxWebsocketPayloadSize` is the same 131072; a text note is a few hundred bytes, and a
/// larger message is refused by the framing layer before it is buffered in full.
pub const MAX_MESSAGE_BYTES: usize = 128 * 1024;

/// Open subscriptions one connection may hold. A REQ past it is answered `CLOSED`.
pub const MAX_SUBSCRIPTIONS: usize = 20;

/// Filters one REQ may carry. Each is a separate query the model is shown.
pub const MAX_FILTERS: usize = 10;

/// NIP-01: a subscription id is "an arbitrary, non-empty string of max length 64 chars".
pub const MAX_SUBSCRIPTION_ID_CHARS: usize = 64;

/// Tags one event may carry. strfry's `maxNumTags` default is 2000.
pub const MAX_TAGS: usize = 2000;

/// Events one `send_nostr_events` answer may ask NetGet to sign.
pub const MAX_EVENTS_PER_ANSWER: usize = 500;

/// NIP-01 kinds are integers between 0 and 65535.
pub const MAX_KIND: u64 = 65_535;

/// The machine-readable prefixes NIP-01 gives an `OK` or `CLOSED` message.
pub const REASON_PREFIXES: &[&str] = &[
    "duplicate",
    "pow",
    "blocked",
    "rate-limited",
    "invalid",
    "restricted",
    "error",
];

static SECP: LazyLock<Secp256k1<secp256k1::All>> = LazyLock::new(Secp256k1::new);

// ---------------------------------------------------------------------------
// Events
// ---------------------------------------------------------------------------

/// A NIP-01 event whose id and signature have been checked, or that NetGet signed itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Event {
    pub id: String,
    pub pubkey: String,
    pub created_at: u64,
    pub kind: u64,
    pub tags: Vec<Vec<String>>,
    pub content: String,
    pub sig: String,
}

impl Event {
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "pubkey": self.pubkey,
            "created_at": self.created_at,
            "kind": self.kind,
            "tags": self.tags,
            "content": self.content,
            "sig": self.sig,
        })
    }
}

/// How strings are escaped in a serialisation. See the module doc.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Escaping {
    /// `serde_json`, nostr-tools, rust-nostr.
    Json,
    /// NIP-01's text taken literally.
    Nip01Literal,
    /// go-nostr (nak).
    GoNostr,
}

fn push_escaped(out: &mut String, s: &str, escaping: Escaping) {
    if escaping == Escaping::Json {
        out.push_str(&serde_json::to_string(s).expect("a string always serialises"));
        return;
    }
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' if escaping == Escaping::Nip01Literal => out.push_str("\\b"),
            '\u{c}' if escaping == Escaping::Nip01Literal => out.push_str("\\f"),
            other => out.push(other),
        }
    }
    out.push('"');
}

/// The serialisation the id is the hash of, under one escaping. See the module doc.
pub fn serialize_for_id(
    escaping: Escaping,
    pubkey: &str,
    created_at: u64,
    kind: u64,
    tags: &[Vec<String>],
    content: &str,
) -> String {
    let mut out = String::with_capacity(content.len() + 128);
    out.push_str("[0,");
    push_escaped(&mut out, pubkey, escaping);
    out.push_str(&format!(",{created_at},{kind},["));
    for (i, tag) in tags.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('[');
        for (j, part) in tag.iter().enumerate() {
            if j > 0 {
                out.push(',');
            }
            push_escaped(&mut out, part, escaping);
        }
        out.push(']');
    }
    out.push_str("],");
    push_escaped(&mut out, content, escaping);
    out.push(']');
    out
}

/// The canonical serialisation NetGet signs with: the JSON escaping.
pub fn canonical_serialization(
    pubkey: &str,
    created_at: u64,
    kind: u64,
    tags: &[Vec<String>],
    content: &str,
) -> String {
    serialize_for_id(Escaping::Json, pubkey, created_at, kind, tags, content)
}

/// Lowercase hex of the SHA-256 of the canonical serialisation.
pub fn compute_id(
    pubkey: &str,
    created_at: u64,
    kind: u64,
    tags: &[Vec<String>],
    content: &str,
) -> String {
    id_under(Escaping::Json, pubkey, created_at, kind, tags, content)
}

fn id_under(
    escaping: Escaping,
    pubkey: &str,
    created_at: u64,
    kind: u64,
    tags: &[Vec<String>],
    content: &str,
) -> String {
    let serialized = serialize_for_id(escaping, pubkey, created_at, kind, tags, content);
    hex::encode(Sha256::digest(serialized.as_bytes()))
}

/// A C0 control the escapings disagree about: anything below U+0020 but `\n \r \t`.
fn is_contested(ch: char) -> bool {
    (ch as u32) < 0x20 && !matches!(ch, '\n' | '\r' | '\t')
}

/// Whether an event's id depends on which escaping computed it.
fn has_contested(tags: &[Vec<String>], content: &str) -> bool {
    content.chars().any(is_contested) || tags.iter().flatten().any(|p| p.chars().any(is_contested))
}

fn is_lower_hex(s: &str, len: usize) -> bool {
    s.len() == len
        && s.bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Why an `EVENT` was refused before it could reach the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventRefusal {
    /// The event's `id` when it had a well-formed one, so the refusal can be an `OK`.
    pub id: Option<String>,
    /// The message, NIP-01 prefix included.
    pub message: String,
    /// The `decision=` token for the log.
    pub decision: &'static str,
}

/// Validate an event's shape, recompute its id and verify its signature.
pub fn verify_event(value: &Value) -> Result<Event, EventRefusal> {
    let object = value.as_object();
    let id = object
        .and_then(|o| o.get("id"))
        .and_then(Value::as_str)
        .filter(|s| is_lower_hex(s, 64))
        .map(str::to_string);
    let refuse = |message: String, decision: &'static str| EventRefusal {
        id: id.clone(),
        message,
        decision,
    };
    let Some(object) = object else {
        return Err(refuse(
            "invalid: an event must be a JSON object".to_string(),
            "fail_closed_invalid_event",
        ));
    };
    let Some(event_id) = id.clone() else {
        return Err(refuse(
            "invalid: id must be 64 lowercase hex characters".to_string(),
            "fail_closed_invalid_event",
        ));
    };
    let pubkey = match object.get("pubkey").and_then(Value::as_str) {
        Some(p) if is_lower_hex(p, 64) => p.to_string(),
        _ => {
            return Err(refuse(
                "invalid: pubkey must be 64 lowercase hex characters".to_string(),
                "fail_closed_invalid_event",
            ))
        }
    };
    let sig = match object.get("sig").and_then(Value::as_str) {
        Some(s) if is_lower_hex(s, 128) => s.to_string(),
        _ => {
            return Err(refuse(
                "invalid: sig must be 128 lowercase hex characters".to_string(),
                "fail_closed_invalid_event",
            ))
        }
    };
    let Some(created_at) = object.get("created_at").and_then(Value::as_u64) else {
        return Err(refuse(
            "invalid: created_at must be a non-negative integer".to_string(),
            "fail_closed_invalid_event",
        ));
    };
    let kind = match object.get("kind").and_then(Value::as_u64) {
        Some(k) if k <= MAX_KIND => k,
        _ => {
            return Err(refuse(
                format!("invalid: kind must be an integer from 0 to {MAX_KIND}"),
                "fail_closed_invalid_event",
            ))
        }
    };
    let tags = match object.get("tags") {
        Some(Value::Array(items)) => {
            if items.len() > MAX_TAGS {
                return Err(refuse(
                    format!("invalid: more than {MAX_TAGS} tags"),
                    "fail_closed_invalid_event",
                ));
            }
            let mut tags = Vec::with_capacity(items.len());
            for item in items {
                let Some(parts) = item.as_array() else {
                    return Err(refuse(
                        "invalid: every tag must be an array of strings".to_string(),
                        "fail_closed_invalid_event",
                    ));
                };
                let mut tag = Vec::with_capacity(parts.len());
                for part in parts {
                    match part.as_str() {
                        Some(s) => tag.push(s.to_string()),
                        None => {
                            return Err(refuse(
                                "invalid: every tag must be an array of strings".to_string(),
                                "fail_closed_invalid_event",
                            ))
                        }
                    }
                }
                tags.push(tag);
            }
            tags
        }
        _ => {
            return Err(refuse(
                "invalid: tags must be an array".to_string(),
                "fail_closed_invalid_event",
            ))
        }
    };
    let Some(content) = object.get("content").and_then(Value::as_str) else {
        return Err(refuse(
            "invalid: content must be a string".to_string(),
            "fail_closed_invalid_event",
        ));
    };

    let escapings: &[Escaping] = if has_contested(&tags, content) {
        &[Escaping::Json, Escaping::Nip01Literal, Escaping::GoNostr]
    } else {
        &[Escaping::Json]
    };
    let matches_an_escaping = escapings
        .iter()
        .any(|e| id_under(*e, &pubkey, created_at, kind, &tags, content) == event_id);
    if !matches_an_escaping {
        return Err(refuse(
            "invalid: event id does not match the hash of its content".to_string(),
            "fail_closed_invalid_id",
        ));
    }

    let bad_signature = || {
        refuse(
            "invalid: signature does not verify".to_string(),
            "fail_closed_invalid_signature",
        )
    };
    let key_bytes = hex::decode(&pubkey).map_err(|_| bad_signature())?;
    let Ok(key) = XOnlyPublicKey::from_slice(&key_bytes) else {
        return Err(refuse(
            "invalid: pubkey is not a point on secp256k1".to_string(),
            "fail_closed_invalid_signature",
        ));
    };
    let sig_bytes = hex::decode(&sig).map_err(|_| bad_signature())?;
    let signature = schnorr::Signature::from_slice(&sig_bytes).map_err(|_| bad_signature())?;
    let digest: [u8; 32] = hex::decode(&event_id)
        .ok()
        .and_then(|b| b.try_into().ok())
        .ok_or_else(bad_signature)?;
    SECP.verify_schnorr(&signature, &Message::from_digest(digest), &key)
        .map_err(|_| bad_signature())?;

    Ok(Event {
        id: event_id,
        pubkey,
        created_at,
        kind,
        tags,
        content: content.to_string(),
        sig,
    })
}

// ---------------------------------------------------------------------------
// The relay's own key
// ---------------------------------------------------------------------------

/// The key NetGet signs the model's events with.
///
/// A model cannot produce a BIP-340 signature, and an event without one is refused by every
/// client. So events the model supplies are signed by the relay, under the relay's pubkey —
/// not by their nominal author, whose secret key NetGet does not have.
pub struct RelayKey {
    keypair: Keypair,
    pubkey_hex: String,
}

impl std::fmt::Debug for RelayKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RelayKey")
            .field("pubkey", &self.pubkey_hex)
            .finish_non_exhaustive()
    }
}

impl RelayKey {
    /// A fresh key from the OS random source.
    pub fn generate() -> Self {
        loop {
            let bytes: [u8; 32] = rand::random();
            if let Ok(secret) = SecretKey::from_slice(&bytes) {
                return Self::from_secret(secret);
            }
        }
    }

    /// A key from 64 hex characters.
    pub fn from_hex(hex_key: &str) -> Result<Self, String> {
        let bytes = hex::decode(hex_key.trim())
            .map_err(|_| "relay_secret_key must be 64 hex characters".to_string())?;
        if bytes.len() != 32 {
            return Err("relay_secret_key must be 64 hex characters".to_string());
        }
        let secret = SecretKey::from_slice(&bytes)
            .map_err(|_| "relay_secret_key is not a valid secp256k1 secret key".to_string())?;
        Ok(Self::from_secret(secret))
    }

    fn from_secret(secret: SecretKey) -> Self {
        let keypair = Keypair::from_secret_key(&SECP, &secret);
        let (xonly, _) = keypair.x_only_public_key();
        Self {
            keypair,
            pubkey_hex: hex::encode(xonly.serialize()),
        }
    }

    pub fn pubkey_hex(&self) -> &str {
        &self.pubkey_hex
    }

    /// Build, hash and sign an event under this key.
    pub fn sign(
        &self,
        created_at: u64,
        kind: u64,
        tags: Vec<Vec<String>>,
        content: String,
    ) -> Event {
        let id = compute_id(&self.pubkey_hex, created_at, kind, &tags, &content);
        let digest: [u8; 32] = hex::decode(&id)
            .expect("compute_id returns hex")
            .try_into()
            .expect("sha256 is 32 bytes");
        let aux: [u8; 32] = rand::random();
        let signature =
            SECP.sign_schnorr_with_aux_rand(&Message::from_digest(digest), &self.keypair, &aux);
        Event {
            id,
            pubkey: self.pubkey_hex.clone(),
            created_at,
            kind,
            tags,
            content,
            sig: hex::encode(signature.serialize()),
        }
    }
}

// ---------------------------------------------------------------------------
// Filters
// ---------------------------------------------------------------------------

/// One NIP-01 filter. Keys NIP-01 does not define (`search`, …) are kept in `raw` for the model
/// and ignored when matching.
#[derive(Debug, Clone, PartialEq)]
pub struct Filter {
    pub ids: Option<Vec<String>>,
    pub authors: Option<Vec<String>>,
    pub kinds: Option<Vec<u64>>,
    pub since: Option<u64>,
    pub until: Option<u64>,
    pub limit: Option<u64>,
    /// `#e`, `#p`, `#t`…: the single letter and the values the tag's first value must be one of.
    pub tags: Vec<(String, Vec<String>)>,
    pub raw: Value,
}

fn hex_list(key: &str, value: &Value) -> Result<Vec<String>, String> {
    let items = value
        .as_array()
        .ok_or_else(|| format!("invalid: filter {key} must be an array"))?;
    items
        .iter()
        .map(|v| match v.as_str() {
            Some(s) if is_lower_hex(s, 64) => Ok(s.to_string()),
            _ => Err(format!(
                "invalid: filter {key} must hold 64-character lowercase hex strings"
            )),
        })
        .collect()
}

fn timestamp(key: &str, value: &Value) -> Result<u64, String> {
    value
        .as_u64()
        .ok_or_else(|| format!("invalid: filter {key} must be a non-negative integer"))
}

/// Parse one filter object.
pub fn parse_filter(value: &Value) -> Result<Filter, String> {
    let object: &Map<String, Value> = value
        .as_object()
        .ok_or_else(|| "invalid: a filter must be a JSON object".to_string())?;
    let mut filter = Filter {
        ids: None,
        authors: None,
        kinds: None,
        since: None,
        until: None,
        limit: None,
        tags: Vec::new(),
        raw: value.clone(),
    };
    for (key, v) in object {
        match key.as_str() {
            "ids" => filter.ids = Some(hex_list(key, v)?),
            "authors" => filter.authors = Some(hex_list(key, v)?),
            "kinds" => {
                let items = v
                    .as_array()
                    .ok_or_else(|| "invalid: filter kinds must be an array".to_string())?;
                let kinds = items
                    .iter()
                    .map(|k| match k.as_u64() {
                        Some(k) if k <= MAX_KIND => Ok(k),
                        _ => Err(format!(
                            "invalid: filter kinds must be integers from 0 to {MAX_KIND}"
                        )),
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                filter.kinds = Some(kinds);
            }
            "since" => filter.since = Some(timestamp(key, v)?),
            "until" => filter.until = Some(timestamp(key, v)?),
            "limit" => filter.limit = Some(timestamp(key, v)?),
            tag if tag.len() == 2
                && tag.starts_with('#')
                && tag.as_bytes()[1].is_ascii_alphabetic() =>
            {
                let items = v
                    .as_array()
                    .ok_or_else(|| format!("invalid: filter {tag} must be an array"))?;
                let values = items
                    .iter()
                    .map(|s| {
                        s.as_str()
                            .map(str::to_string)
                            .ok_or_else(|| format!("invalid: filter {tag} must hold strings"))
                    })
                    .collect::<Result<Vec<_>, _>>()?;
                filter.tags.push((tag[1..].to_string(), values));
            }
            // Extensions (NIP-50 `search`, …) are not matched on, but must still be flat: a
            // filter is shown to the model, and nothing in NIP-01 nests deeper than this.
            _ => {
                let flat = match v {
                    Value::Array(items) => items.iter().all(|i| !i.is_array() && !i.is_object()),
                    Value::Object(_) => false,
                    _ => true,
                };
                if !flat {
                    return Err(format!(
                        "invalid: filter field {key} must be a scalar or an array of scalars"
                    ));
                }
            }
        }
    }
    Ok(filter)
}

impl Filter {
    /// Whether `event` satisfies every condition of this filter (NIP-01: conditions are ANDed,
    /// values within one condition ORed). `limit` is not a condition; see [`select_events`].
    pub fn matches(&self, event: &Event) -> bool {
        if let Some(ids) = &self.ids {
            if !ids.contains(&event.id) {
                return false;
            }
        }
        if let Some(authors) = &self.authors {
            if !authors.contains(&event.pubkey) {
                return false;
            }
        }
        if let Some(kinds) = &self.kinds {
            if !kinds.contains(&event.kind) {
                return false;
            }
        }
        if let Some(since) = self.since {
            if event.created_at < since {
                return false;
            }
        }
        if let Some(until) = self.until {
            if event.created_at > until {
                return false;
            }
        }
        for (letter, values) in &self.tags {
            let hit = event.tags.iter().any(|tag| {
                tag.first().is_some_and(|name| name == letter)
                    && tag.get(1).is_some_and(|v| values.contains(v))
            });
            if !hit {
                return false;
            }
        }
        true
    }
}

/// Which of `events` a subscription with these `filters` receives, as indices in their
/// original order.
///
/// An event is sent when it matches at least one filter. With `apply_limit` (the answer to the
/// REQ itself, NIP-01's "initial query"), each filter contributes at most `limit` of its
/// matches, the newest first and, on equal `created_at`, the lowest id — NIP-01's own order.
/// Live events pushed after EOSE are not limited.
pub fn select_events(filters: &[Filter], events: &[Event], apply_limit: bool) -> Vec<usize> {
    let mut chosen = vec![false; events.len()];
    for filter in filters {
        let mut hits: Vec<usize> = (0..events.len())
            .filter(|&i| filter.matches(&events[i]))
            .collect();
        if apply_limit {
            if let Some(limit) = filter.limit {
                hits.sort_by(|&a, &b| {
                    events[b]
                        .created_at
                        .cmp(&events[a].created_at)
                        .then_with(|| events[a].id.cmp(&events[b].id))
                });
                hits.truncate(usize::try_from(limit).unwrap_or(usize::MAX));
            }
        }
        for i in hits {
            chosen[i] = true;
        }
    }
    (0..events.len()).filter(|&i| chosen[i]).collect()
}

// ---------------------------------------------------------------------------
// Client messages
// ---------------------------------------------------------------------------

/// A client message worth acting on. An `EVENT` here has already passed [`verify_event`].
#[derive(Debug, Clone)]
pub enum ClientMessage {
    Event(Event),
    Req {
        subscription_id: String,
        filters: Vec<Filter>,
    },
    Close {
        subscription_id: String,
    },
}

/// A message NetGet answers itself, without the model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Refusal {
    /// The relay message to send, already rendered.
    pub reply: String,
    /// The `decision=` token for the log.
    pub decision: &'static str,
}

impl Refusal {
    fn notice(message: impl Into<String>, decision: &'static str) -> Self {
        Self {
            reply: notice_message(&message.into()),
            decision,
        }
    }
    fn closed(subscription_id: &str, message: impl Into<String>, decision: &'static str) -> Self {
        Self {
            reply: closed_message(subscription_id, &message.into()),
            decision,
        }
    }
}

fn subscription_id(value: Option<&Value>, verb: &str) -> Result<String, Refusal> {
    match value.and_then(Value::as_str) {
        Some(id) if !id.is_empty() && id.chars().count() <= MAX_SUBSCRIPTION_ID_CHARS => {
            Ok(id.to_string())
        }
        Some(id) if !id.is_empty() => Err(Refusal::closed(
            id,
            format!("invalid: subscription id longer than {MAX_SUBSCRIPTION_ID_CHARS} characters"),
            "fail_closed_bad_subscription_id",
        )),
        _ => Err(Refusal::notice(
            format!("invalid: {verb} needs a non-empty subscription id string"),
            "fail_closed_bad_subscription_id",
        )),
    }
}

/// Parse one text frame from a client.
pub fn parse_client_message(text: &str) -> Result<ClientMessage, Refusal> {
    let value: Value = serde_json::from_str(text).map_err(|e| {
        let why = if e.to_string().contains("recursion limit") {
            "nested too deeply"
        } else {
            "not valid JSON"
        };
        Refusal::notice(
            format!("invalid: message is {why}"),
            "fail_closed_malformed",
        )
    })?;
    let Some(items) = value.as_array() else {
        return Err(Refusal::notice(
            "invalid: a message must be a JSON array",
            "fail_closed_malformed",
        ));
    };
    let Some(verb) = items.first().and_then(Value::as_str) else {
        return Err(Refusal::notice(
            "invalid: a message must start with its type, a string",
            "fail_closed_malformed",
        ));
    };
    match verb {
        "EVENT" => {
            let Some(event) = items.get(1) else {
                return Err(Refusal::notice(
                    "invalid: EVENT needs an event",
                    "fail_closed_malformed",
                ));
            };
            verify_event(event)
                .map(ClientMessage::Event)
                .map_err(|r| match r.id {
                    Some(id) => Refusal {
                        reply: ok_message(&id, false, &r.message),
                        decision: r.decision,
                    },
                    None => Refusal::notice(r.message, r.decision),
                })
        }
        "REQ" => {
            let subscription_id = subscription_id(items.get(1), "REQ")?;
            let raw_filters = &items[2..];
            if raw_filters.is_empty() {
                return Err(Refusal::closed(
                    &subscription_id,
                    "invalid: REQ needs at least one filter",
                    "fail_closed_bad_filter",
                ));
            }
            if raw_filters.len() > MAX_FILTERS {
                return Err(Refusal::closed(
                    &subscription_id,
                    format!("invalid: more than {MAX_FILTERS} filters in one REQ"),
                    "fail_closed_too_many_filters",
                ));
            }
            let filters = raw_filters
                .iter()
                .map(parse_filter)
                .collect::<Result<Vec<_>, _>>()
                .map_err(|why| Refusal::closed(&subscription_id, why, "fail_closed_bad_filter"))?;
            Ok(ClientMessage::Req {
                subscription_id,
                filters,
            })
        }
        "CLOSE" => Ok(ClientMessage::Close {
            subscription_id: subscription_id(items.get(1), "CLOSE")?,
        }),
        other => Err(Refusal::notice(
            format!(
                "error: unsupported message type {}; this relay speaks EVENT, REQ and CLOSE",
                crate::utils::truncate_for_log(other, 32)
            ),
            "fail_closed_unsupported",
        )),
    }
}

// ---------------------------------------------------------------------------
// Relay messages
// ---------------------------------------------------------------------------

fn render(value: Value) -> String {
    serde_json::to_string(&value).expect("relay messages are plain JSON")
}

/// `["EVENT", <subscription_id>, <event>]`
pub fn event_message(subscription_id: &str, event: &Event) -> String {
    render(json!(["EVENT", subscription_id, event.to_json()]))
}

/// `["OK", <event_id>, <accepted>, <message>]`
pub fn ok_message(event_id: &str, accepted: bool, message: &str) -> String {
    render(json!(["OK", event_id, accepted, message]))
}

/// `["EOSE", <subscription_id>]`
pub fn eose_message(subscription_id: &str) -> String {
    render(json!(["EOSE", subscription_id]))
}

/// `["CLOSED", <subscription_id>, <message>]`
pub fn closed_message(subscription_id: &str, message: &str) -> String {
    render(json!(["CLOSED", subscription_id, message]))
}

/// `["NOTICE", <message>]`
pub fn notice_message(message: &str) -> String {
    render(json!(["NOTICE", message]))
}

/// `reason` as an OK/CLOSED message: kept as written when it already starts with one of
/// NIP-01's prefixes (`blocked: …`), otherwise given `default_prefix`.
pub fn with_prefix(reason: &str, default_prefix: &str) -> String {
    let reason = reason.trim();
    let has_prefix = REASON_PREFIXES.iter().any(|p| {
        reason
            .strip_prefix(p)
            .is_some_and(|rest| rest.starts_with(':'))
    });
    if has_prefix {
        reason.to_string()
    } else if reason.is_empty() {
        format!("{default_prefix}:")
    } else {
        format!("{default_prefix}: {reason}")
    }
}

// ---------------------------------------------------------------------------
// Events the model supplies
// ---------------------------------------------------------------------------

/// What the model gives for one event: everything but `pubkey`, `id` and `sig`, which NetGet
/// fills in by signing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SuppliedEvent {
    pub kind: u64,
    pub content: String,
    pub tags: Vec<Vec<String>>,
    /// `None` means "now", resolved when NetGet signs.
    pub created_at: Option<u64>,
}

/// Text with the C0 controls the id escapings disagree about removed. See the module doc.
fn uncontested(s: &str) -> String {
    s.chars().filter(|c| !is_contested(*c)).collect()
}

fn scalar_text(value: &Value) -> Option<String> {
    match value {
        Value::String(s) => Some(uncontested(s)),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

/// Read one event the model supplied. C0 control characters other than `\n \r \t` are
/// dropped from its content and tags, so no client computes a different id for it.
pub fn parse_supplied_event(value: &Value) -> Result<SuppliedEvent, String> {
    let object = value.as_object().ok_or_else(|| {
        "each event must be an object {kind, content, tags?, created_at?}".to_string()
    })?;
    let kind = match object.get("kind") {
        Some(Value::Number(n)) => n.as_u64(),
        Some(Value::String(s)) => s.trim().parse().ok(),
        _ => None,
    }
    .filter(|k| *k <= MAX_KIND)
    .ok_or_else(|| format!("each event needs a kind, an integer from 0 to {MAX_KIND}"))?;
    let content = match object.get("content") {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => uncontested(s),
        // A kind-0 profile's content is a JSON object serialised as a string; a model that
        // writes the object itself means exactly that.
        Some(other) => serde_json::to_string(other).map_err(|e| e.to_string())?,
    };
    let tags = match object.get("tags") {
        None | Some(Value::Null) => Vec::new(),
        Some(Value::Array(items)) => {
            if items.len() > MAX_TAGS {
                return Err(format!("an event may carry at most {MAX_TAGS} tags"));
            }
            items
                .iter()
                .map(|tag| {
                    tag.as_array()
                        .ok_or_else(|| {
                            "tags must be an array of arrays of strings, e.g. [[\"t\", \"news\"]]"
                                .to_string()
                        })?
                        .iter()
                        .map(|part| {
                            scalar_text(part).ok_or_else(|| {
                                "tags must be an array of arrays of strings".to_string()
                            })
                        })
                        .collect::<Result<Vec<_>, _>>()
                })
                .collect::<Result<Vec<_>, _>>()?
        }
        Some(_) => return Err("tags must be an array of arrays of strings".to_string()),
    };
    let created_at = match object.get("created_at") {
        None | Some(Value::Null) => None,
        Some(Value::Number(n)) => Some(
            n.as_u64()
                .ok_or_else(|| "created_at must be a unix timestamp in seconds".to_string())?,
        ),
        Some(Value::String(s)) => Some(
            s.trim()
                .parse()
                .map_err(|_| "created_at must be a unix timestamp in seconds".to_string())?,
        ),
        Some(_) => return Err("created_at must be a unix timestamp in seconds".to_string()),
    };
    Ok(SuppliedEvent {
        kind,
        content,
        tags,
        created_at,
    })
}

/// Read the `events` array of `send_nostr_events`.
pub fn parse_supplied_events(value: Option<&Value>) -> Result<Vec<SuppliedEvent>, String> {
    let items =
        match value {
            None | Some(Value::Null) => return Ok(Vec::new()),
            Some(Value::Array(items)) => items,
            Some(_) => return Err(
                "send_nostr_events needs 'events', an array of {kind, content, tags?, created_at?}"
                    .to_string(),
            ),
        };
    if items.len() > MAX_EVENTS_PER_ANSWER {
        return Err(format!(
            "at most {MAX_EVENTS_PER_ANSWER} events per send_nostr_events"
        ));
    }
    items.iter().map(parse_supplied_event).collect()
}

/// Seconds since the Unix epoch, for an event the model gave no `created_at`.
pub fn now_secs() -> u64 {
    crate::utils::clock::SystemTime::now()
        .duration_since(crate::utils::clock::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}
