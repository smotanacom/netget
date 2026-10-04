//! WAMP URIs and identifiers (WAMP basic profile, "URIs" and "IDs").
use serde_json::Value;

pub const HELLO: u64 = 1;
pub const WELCOME: u64 = 2;
pub const ABORT: u64 = 3;
pub const GOODBYE: u64 = 6;
pub const ERROR: u64 = 8;
pub const PUBLISH: u64 = 16;
pub const PUBLISHED: u64 = 17;
pub const SUBSCRIBE: u64 = 32;
pub const SUBSCRIBED: u64 = 33;
pub const UNSUBSCRIBE: u64 = 34;
pub const UNSUBSCRIBED: u64 = 35;
pub const EVENT: u64 = 36;
pub const CALL: u64 = 48;
pub const CANCEL: u64 = 49;
pub const RESULT: u64 = 50;
pub const REGISTER: u64 = 64;
pub const REGISTERED: u64 = 65;
pub const UNREGISTER: u64 = 66;
pub const UNREGISTERED: u64 = 67;
pub const INVOCATION: u64 = 68;
pub const INTERRUPT: u64 = 69;
pub const YIELD: u64 = 70;

pub const SUBPROTOCOL: &str = "wamp.2.json";
/// IDs are drawn from [1, 2^53].
pub const MAX_ID: u64 = 1 << 53;

/// A loose URI (no whitespace, `#`, or empty components); with `allow_empty`, empty components
/// are allowed as in wildcard patterns.
pub fn uri_ok(s: &str, allow_empty: bool) -> bool {
    !s.is_empty()
        && s.len() <= 255
        && !s
            .chars()
            .any(|c| c.is_whitespace() || c == '#' || c.is_control())
        && s.split('.').all(|c| allow_empty || !c.is_empty())
}

pub fn id_of(v: &Value) -> Option<u64> {
    v.as_u64().filter(|n| (1..=MAX_ID).contains(n))
}

pub fn random_id() -> u64 {
    use ring::rand::SecureRandom;
    let mut b = [0u8; 8];
    let _ = ring::rand::SystemRandom::new().fill(&mut b);
    (u64::from_le_bytes(b) % MAX_ID) + 1
}

/// Whether a subscription `pattern` under `policy` matches `topic`.
pub fn matches(policy: &str, pattern: &str, topic: &str) -> bool {
    match policy {
        "prefix" => topic.starts_with(pattern),
        "wildcard" => {
            let (p, t): (Vec<&str>, Vec<&str>) =
                (pattern.split('.').collect(), topic.split('.').collect());
            p.len() == t.len() && p.iter().zip(&t).all(|(a, b)| a.is_empty() || a == b)
        }
        _ => pattern == topic,
    }
}

/// `[.., args?, kwargs?]` with empty tails dropped, as WAMP requires.
pub fn with_payload(mut msg: Vec<Value>, args: Option<&Value>, kwargs: Option<&Value>) -> Value {
    let args = args.filter(|a| a.as_array().is_some_and(|a| !a.is_empty()));
    let kwargs = kwargs.filter(|k| k.as_object().is_some_and(|k| !k.is_empty()));
    if args.is_some() || kwargs.is_some() {
        msg.push(args.cloned().unwrap_or_else(|| Value::Array(vec![])));
    }
    if let Some(k) = kwargs {
        msg.push(k.clone());
    }
    Value::Array(msg)
}
