//! Bolt's handshake and messages: what a client may send, and the four things a server answers
//! with (SUCCESS, RECORD, IGNORED, FAILURE).
//!
//! Message shapes are those `cypher-shell` 2026.09 (neo4j-java driver 6.2) was recorded sending
//! against a raw TCP recorder before this was written; see `CLAUDE.md` for the capture.

use super::packstream::Value;

/// The four bytes every Bolt connection opens with.
pub const MAGIC: [u8; 4] = [0x60, 0x60, 0xB0, 0x17];

/// Bolt major version this server speaks.
pub const MAJOR: u8 = 5;
/// Lowest and highest 5.x minor this server negotiates.
pub const MIN_MINOR: u8 = 0;
pub const MAX_MINOR: u8 = 8;

/// Request tags.
pub const HELLO: u8 = 0x01;
pub const GOODBYE: u8 = 0x02;
pub const RESET: u8 = 0x0F;
pub const RUN: u8 = 0x10;
pub const BEGIN: u8 = 0x11;
pub const COMMIT: u8 = 0x12;
pub const ROLLBACK: u8 = 0x13;
pub const DISCARD: u8 = 0x2F;
pub const PULL: u8 = 0x3F;
pub const TELEMETRY: u8 = 0x54;
pub const ROUTE: u8 = 0x66;
pub const LOGON: u8 = 0x6A;
pub const LOGOFF: u8 = 0x6B;

/// Response tags.
pub const SUCCESS: u8 = 0x70;
pub const RECORD: u8 = 0x71;
pub const IGNORED: u8 = 0x7E;
pub const FAILURE: u8 = 0x7F;

/// Choose a version from the client's four proposals.
///
/// Each proposal is four bytes, `[reserved, range, minor, major]`, meaning "any of
/// `major.(minor - range) ..= major.minor`", in the client's order of preference. The first
/// proposal overlapping 5.0..=5.8 wins, at the highest minor both sides support. The Bolt 5.7+
/// handshake-manifest proposal (`00 00 01 FF`) names major 255 and is passed over, so a client
/// offering it falls back to its plain proposals.
pub fn negotiate(proposals: &[u8; 16]) -> Option<(u8, u8)> {
    for [_, range, minor, major] in proposals.as_chunks::<4>().0 {
        if *major != MAJOR {
            continue;
        }
        // Every minor from MIN_MINOR (0) up is spoken, so the proposal overlaps exactly when
        // its range reaches down to MAX_MINOR or below.
        let lowest = minor.saturating_sub(*range);
        let chosen = (*minor).min(MAX_MINOR);
        if chosen >= lowest {
            return Some((MAJOR, chosen));
        }
    }
    None
}

/// One client request, decoded from its structure.
#[derive(Debug, Clone)]
pub enum Request {
    Hello {
        extra: Value,
    },
    Logon {
        auth: Value,
    },
    Logoff,
    Goodbye,
    Reset,
    Run {
        query: String,
        params: Value,
        extra: Value,
    },
    Begin {
        extra: Value,
    },
    Commit,
    Rollback,
    Pull {
        n: i64,
        qid: i64,
    },
    Discard {
        n: i64,
        qid: i64,
    },
    Route {
        routing: Value,
        extra: Value,
    },
    Telemetry,
}

impl Request {
    pub fn name(&self) -> &'static str {
        match self {
            Request::Hello { .. } => "HELLO",
            Request::Logon { .. } => "LOGON",
            Request::Logoff => "LOGOFF",
            Request::Goodbye => "GOODBYE",
            Request::Reset => "RESET",
            Request::Run { .. } => "RUN",
            Request::Begin { .. } => "BEGIN",
            Request::Commit => "COMMIT",
            Request::Rollback => "ROLLBACK",
            Request::Pull { .. } => "PULL",
            Request::Discard { .. } => "DISCARD",
            Request::Route { .. } => "ROUTE",
            Request::Telemetry => "TELEMETRY",
        }
    }
}

fn empty_map() -> Value {
    Value::Map(Vec::new())
}

fn map_field(fields: &[Value], i: usize) -> Result<Value, String> {
    match fields.get(i) {
        None | Some(Value::Null) => Ok(empty_map()),
        Some(v @ Value::Map(_)) => Ok(v.clone()),
        Some(_) => Err(format!("field {i} must be a map")),
    }
}

/// Decode a request structure. `Err` names what is wrong, for the log and for a
/// `Neo.ClientError.Request.Invalid` FAILURE.
pub fn parse_request(value: Value) -> Result<Request, String> {
    let Value::Struct { tag, fields } = value else {
        return Err("a Bolt message must be a structure".to_string());
    };
    Ok(match tag {
        HELLO => Request::Hello {
            extra: map_field(&fields, 0)?,
        },
        LOGON => Request::Logon {
            auth: map_field(&fields, 0)?,
        },
        LOGOFF => Request::Logoff,
        GOODBYE => Request::Goodbye,
        RESET => Request::Reset,
        RUN => {
            let query = fields
                .first()
                .and_then(Value::as_str)
                .ok_or("RUN needs a query string")?
                .to_string();
            Request::Run {
                query,
                params: map_field(&fields, 1)?,
                extra: map_field(&fields, 2)?,
            }
        }
        BEGIN => Request::Begin {
            extra: map_field(&fields, 0)?,
        },
        COMMIT => Request::Commit,
        ROLLBACK => Request::Rollback,
        PULL | DISCARD => {
            let extra = map_field(&fields, 0)?;
            let n = extra.get("n").and_then(Value::as_int).unwrap_or(-1);
            let qid = extra.get("qid").and_then(Value::as_int).unwrap_or(-1);
            if n == 0 || n < -1 {
                return Err(format!("'n' must be positive or -1, got {n}"));
            }
            if tag == PULL {
                Request::Pull { n, qid }
            } else {
                Request::Discard { n, qid }
            }
        }
        ROUTE => Request::Route {
            routing: map_field(&fields, 0)?,
            // 5.x: [routing, bookmarks, extra{db, imp_user}]; 4.3 sent the db as a string.
            extra: match fields.get(2) {
                Some(Value::String(db)) => Value::map([("db", Value::String(db.clone()))]),
                _ => map_field(&fields, 2)?,
            },
        },
        TELEMETRY => Request::Telemetry,
        other => return Err(format!("unknown message tag 0x{other:02X}")),
    })
}

pub fn success(metadata: Vec<(&str, Value)>) -> Value {
    Value::Struct {
        tag: SUCCESS,
        fields: vec![Value::map(metadata)],
    }
}

pub fn record(values: Vec<Value>) -> Value {
    Value::Struct {
        tag: RECORD,
        fields: vec![Value::List(values)],
    }
}

pub fn ignored() -> Value {
    Value::Struct {
        tag: IGNORED,
        fields: Vec::new(),
    }
}

/// GQL status sent with every FAILURE on Bolt 5.7+: Neo4j's own status for an error that has no
/// more specific GQL mapping ("general processing exception - unexpected error"). The Neo4j
/// status code the model or NetGet chose travels beside it as `neo4j_code`, which is what drivers
/// classify on.
pub const GQL_STATUS: &str = "50N42";
const GQL_DESCRIPTION: &str = "error: general processing exception - unexpected error";

/// A FAILURE in the shape the negotiated version defines: `{code, message}` before 5.7, the GQL
/// error object (`gql_status`, `description`, `message`, `neo4j_code`, `diagnostic_record`) from
/// 5.7 on.
pub fn failure(minor: u8, code: &str, message: &str) -> Value {
    let metadata = if minor >= 7 {
        vec![
            ("gql_status", Value::string(GQL_STATUS)),
            (
                "description",
                Value::String(format!("{GQL_DESCRIPTION}. {message}")),
            ),
            ("message", Value::string(message)),
            ("neo4j_code", Value::string(code)),
            (
                "diagnostic_record",
                Value::map([
                    ("OPERATION", Value::string("")),
                    ("OPERATION_CODE", Value::string("0")),
                    ("CURRENT_SCHEMA", Value::string("/")),
                ]),
            ),
        ]
    } else {
        vec![
            ("code", Value::string(code)),
            ("message", Value::string(message)),
        ]
    };
    Value::Struct {
        tag: FAILURE,
        fields: vec![Value::map(metadata)],
    }
}
