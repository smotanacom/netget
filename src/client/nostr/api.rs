//! Selected NIP-01 schemas. The existing pure wire module owns event crypto.
use crate::server::nostr::wire::{self, Event, Filter};
use anyhow::{ensure, Context, Result};
use serde_json::{json, Map, Value};
pub const MAX_DEPTH: usize = 8;
pub const MAX_NODES: usize = 25000;
pub const MAX_RETAINED_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_LIST: usize = 500;
pub const MAX_REASON: usize = 4096;
pub const MAX_TAG_ITEMS: usize = 32;
pub const MAX_TEXT: usize = 64 * 1024;

pub fn within_budget(value: &Value) -> bool {
    crate::utils::json_budget::within_budget(value, MAX_RETAINED_BYTES, MAX_NODES, MAX_DEPTH)
}
fn fields(value: &Value, names: &[&str]) -> Result<()> {
    ensure!(
        value
            .as_object()
            .context("Nostr action must be object")?
            .keys()
            .all(|key| names.contains(&key.as_str())),
        "unsupported Nostr action field"
    );
    Ok(())
}
pub fn subid(value: &Value) -> Result<String> {
    let id = value
        .as_str()
        .context("Nostr subscription id must be string")?;
    ensure!(
        !id.is_empty() && id.chars().count() <= wire::MAX_SUBSCRIPTION_ID_CHARS,
        "Nostr subscription id must be1..64 characters"
    );
    Ok(id.into())
}
fn lower_hex(value: &str, length: usize) -> bool {
    value.len() == length
        && value
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}
pub enum Action {
    Publish {
        kind: u64,
        content: String,
        tags: Vec<Vec<String>>,
        created_at: u64,
    },
    Subscribe {
        id: String,
        filters: Vec<Filter>,
    },
    Close(String),
    RelayInfo,
    Disconnect,
}
pub fn action(value: &Value) -> Result<Action> {
    ensure!(
        within_budget(value),
        "Nostr action depth/node/retained-content limit"
    );
    match value["type"]
        .as_str()
        .context("Nostr action type required")?
    {
        "nostr_publish" => {
            fields(value, &["type", "kind", "content", "tags", "created_at"])?;
            let kind = value["kind"]
                .as_u64()
                .context("Nostr kind must be integer")?;
            ensure!(kind <= wire::MAX_KIND, "Nostr kind must be0..65535");
            let content = value["content"]
                .as_str()
                .context("Nostr content must be string")?;
            ensure!(content.len() <= MAX_TEXT, "Nostr content limit");
            let created_at = value
                .get("created_at")
                .map(|v| {
                    v.as_u64()
                        .context("Nostr created_at must be nonnegative integer")
                })
                .transpose()?
                .unwrap_or_else(wire::now_secs);
            let mut tags = Vec::new();
            if let Some(value) = value.get("tags") {
                let array = value.as_array().context("Nostr tags must be array")?;
                ensure!(array.len() <= wire::MAX_TAGS, "Nostr tag count limit");
                for tag in array {
                    let tag = tag.as_array().context("Nostr tag must be string array")?;
                    ensure!(
                        !tag.is_empty() && tag.len() <= MAX_TAG_ITEMS,
                        "Nostr tag item limit"
                    );
                    tags.push(
                        tag.iter()
                            .map(|item| {
                                let text =
                                    item.as_str().context("Nostr tag item must be string")?;
                                ensure!(text.len() <= MAX_REASON, "Nostr tag text limit");
                                Ok(text.into())
                            })
                            .collect::<Result<Vec<String>>>()?,
                    );
                }
            }
            Ok(Action::Publish {
                kind,
                content: content.into(),
                tags,
                created_at,
            })
        }
        "nostr_subscribe" => {
            fields(value, &["type", "subscription_id", "filters"])?;
            let id = subid(&value["subscription_id"])?;
            let values = value["filters"]
                .as_array()
                .context("Nostr filters must be array")?;
            ensure!(
                !values.is_empty() && values.len() <= wire::MAX_FILTERS,
                "Nostr filter count must be1..10"
            );
            let filters = values.iter().map(filter).collect::<Result<Vec<_>>>()?;
            let mut frame = vec![json!("REQ"), json!(id)];
            frame.extend(filters.iter().map(|f| f.raw.clone()));
            frame_text(Value::Array(frame))?;
            Ok(Action::Subscribe { id, filters })
        }
        "nostr_close" => {
            fields(value, &["type", "subscription_id"])?;
            Ok(Action::Close(subid(&value["subscription_id"])?))
        }
        "nostr_relay_info" => {
            fields(value, &["type"])?;
            Ok(Action::RelayInfo)
        }
        "disconnect" => {
            fields(value, &["type"])?;
            Ok(Action::Disconnect)
        }
        _ => anyhow::bail!("unsupported selected Nostr action"),
    }
}
pub fn filter(value: &Value) -> Result<Filter> {
    ensure!(within_budget(value), "Nostr filter budget");
    let fields = value.as_object().context("Nostr filter must be object")?;
    ensure!(fields.len() <= 64, "Nostr filter field limit");
    for (name, value) in fields {
        if matches!(name.as_str(), "since" | "until" | "limit") {
            let n = value
                .as_u64()
                .context("Nostr filter scalar must be nonnegative integer")?;
            ensure!(
                name != "limit" || n <= MAX_LIST as u64,
                "Nostr initial limit must be0..500"
            );
        } else {
            ensure!(
                matches!(name.as_str(), "ids" | "authors" | "kinds")
                    || name.len() == 2
                        && name.starts_with('#')
                        && name.as_bytes()[1].is_ascii_alphabetic(),
                "unsupported Nostr filter extension"
            );
            let list = value
                .as_array()
                .context("Nostr filter list must be array")?;
            ensure!(
                !list.is_empty() && list.len() <= MAX_LIST,
                "Nostr filter list must have1..500 values"
            );
            if name != "kinds" {
                for v in list {
                    let text = v.as_str().context("Nostr filter value must be string")?;
                    ensure!(text.len() <= MAX_REASON, "Nostr filter text limit");
                    if matches!(name.as_str(), "#e" | "#p") {
                        ensure!(
                            lower_hex(text, 64),
                            "Nostr reference filter requires full lowercase hex"
                        );
                    }
                }
            }
        }
    }
    wire::parse_filter(value).map_err(|_| anyhow::anyhow!("invalid NIP-01 filter"))
}
pub fn frame_text(frame: Value) -> Result<String> {
    if !within_budget(&frame) {
        crate::utils::json_budget::drop_iteratively(frame);
        anyhow::bail!("Nostr frame depth/node/retained-content limit");
    }
    let text = serde_json::to_string(&frame)?;
    ensure!(
        text.len() <= wire::MAX_MESSAGE_BYTES,
        "Nostr message byte limit"
    );
    Ok(text)
}
pub fn json(text: &[u8]) -> Result<Value> {
    ensure!(
        text.len() <= wire::MAX_MESSAGE_BYTES,
        "Nostr message byte limit"
    );
    let value: Value = serde_json::from_slice(text).context("invalid Nostr JSON")?;
    if !within_budget(&value) {
        crate::utils::json_budget::drop_iteratively(value);
        anyhow::bail!("Nostr JSON depth/node/retained-content limit");
    }
    Ok(value)
}
pub enum RelayMessage {
    Event {
        id: String,
        event: Event,
    },
    Ok {
        id: String,
        accepted: bool,
        message: String,
    },
    Eose(String),
    Closed {
        id: String,
        message: String,
    },
    Notice(String),
    Unsupported(&'static str),
}
fn reason(value: &Value) -> Result<String> {
    let text = value
        .as_str()
        .context("Nostr relay reason must be string")?;
    ensure!(text.len() <= MAX_REASON, "Nostr relay reason limit");
    Ok(text.into())
}
pub fn relay(text: &str) -> Result<RelayMessage> {
    let value = json(text.as_bytes())?;
    let array = value
        .as_array()
        .context("Nostr relay message must be array")?;
    match array
        .first()
        .and_then(Value::as_str)
        .context("Nostr relay verb required")?
    {
        "EVENT" => {
            ensure!(array.len() == 3, "Nostr EVENT arity");
            let id = subid(&array[1])?;
            let event = wire::verify_event(&array[2]).map_err(|_| {
                anyhow::anyhow!("Nostr event id/signature/schema validation failed")
            })?;
            ensure!(
                event.content.len() <= MAX_TEXT
                    && event.tags.iter().all(|t| !t.is_empty()
                        && t.len() <= MAX_TAG_ITEMS
                        && t.iter().all(|s| s.len() <= MAX_REASON)),
                "Nostr incoming event content/tag limit"
            );
            Ok(RelayMessage::Event { id, event })
        }
        "OK" => {
            ensure!(array.len() == 4, "Nostr OK arity");
            let id = array[1].as_str().context("Nostr OK event id required")?;
            ensure!(
                lower_hex(id, 64),
                "Nostr OK event id must be full lowercase hex"
            );
            let accepted = array[2]
                .as_bool()
                .context("Nostr OK acceptance must be boolean")?;
            let message = reason(&array[3])?;
            if !accepted {
                ensure!(
                    reason_prefix(&message).is_some(),
                    "Nostr rejection requires machine-readable prefix"
                );
            }
            Ok(RelayMessage::Ok {
                id: id.into(),
                accepted,
                message,
            })
        }
        "EOSE" => {
            ensure!(array.len() == 2, "Nostr EOSE arity");
            Ok(RelayMessage::Eose(subid(&array[1])?))
        }
        "CLOSED" => {
            ensure!(array.len() == 3, "Nostr CLOSED arity");
            let message = reason(&array[2])?;
            ensure!(
                reason_prefix(&message).is_some(),
                "Nostr CLOSED requires machine-readable prefix"
            );
            Ok(RelayMessage::Closed {
                id: subid(&array[1])?,
                message,
            })
        }
        "NOTICE" => {
            ensure!(array.len() == 2, "Nostr NOTICE arity");
            Ok(RelayMessage::Notice(reason(&array[1])?))
        }
        // Unsupported extensions never trigger signing or expose challenges.
        "AUTH" => Ok(RelayMessage::Unsupported("AUTH")),
        "COUNT" => Ok(RelayMessage::Unsupported("COUNT")),
        _ => Ok(RelayMessage::Unsupported("unknown")),
    }
}
pub fn reason_prefix(text: &str) -> Option<&str> {
    let (prefix, _) = text.split_once(':')?;
    (!prefix.is_empty()
        && prefix.len() <= 64
        && prefix.bytes().all(|b| b.is_ascii_lowercase() || b == b'-'))
    .then_some(prefix)
}
pub fn relay_info(value: &Value) -> Result<Value> {
    ensure!(within_budget(value), "Nostr information budget");
    let input = value
        .as_object()
        .context("NIP-11 document must be object")?;
    let mut output = Map::new();
    for name in [
        "name",
        "description",
        "banner",
        "icon",
        "contact",
        "software",
        "version",
        "terms_of_service",
        "pubkey",
        "self",
    ] {
        if let Some(value) = input.get(name) {
            if value.is_null() {
                continue;
            }
            let text = value
                .as_str()
                .context("NIP-11 selected text field must be string")?;
            ensure!(text.len() <= MAX_TEXT, "NIP-11 text limit");
            if matches!(name, "pubkey" | "self") {
                ensure!(
                    lower_hex(text, 64),
                    "NIP-11 public key must be full lowercase hex"
                );
            }
            output.insert(name.into(), value.clone());
        }
    }
    if let Some(value) = input.get("supported_nips").filter(|v| !v.is_null()) {
        let list = value
            .as_array()
            .context("NIP-11 supported_nips must be array")?;
        ensure!(
            list.len() <= MAX_LIST && list.iter().all(|v| v.as_u64().is_some()),
            "NIP-11 supported_nips integer list limit"
        );
        output.insert("supported_nips".into(), value.clone());
    }
    if let Some(value) = input.get("limitation").filter(|v| !v.is_null()) {
        let input = value
            .as_object()
            .context("NIP-11 limitation must be object")?;
        let mut selected = Map::new();
        for name in [
            "max_message_length",
            "max_subscriptions",
            "max_filters",
            "max_limit",
            "max_subid_length",
            "max_event_tags",
            "max_content_length",
            "min_pow_difficulty",
            "created_at_lower_limit",
            "created_at_upper_limit",
            "default_limit",
        ] {
            if let Some(value) = input.get(name).filter(|v| !v.is_null()) {
                ensure!(
                    value.as_u64().is_some(),
                    "NIP-11 limitation integer required"
                );
                selected.insert(name.into(), value.clone());
            }
        }
        for name in ["auth_required", "payment_required", "restricted_writes"] {
            if let Some(value) = input.get(name).filter(|v| !v.is_null()) {
                ensure!(
                    value.as_bool().is_some(),
                    "NIP-11 limitation boolean required"
                );
                selected.insert(name.into(), value.clone());
            }
        }
        output.insert("limitation".into(), Value::Object(selected));
    }
    Ok(Value::Object(output))
}
