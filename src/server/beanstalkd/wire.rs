//! The beanstalkd wire format: command parsing, reply rendering, and the check that a reply
//! answers the command it is sent for.
//!
//! Everything here is a pure function of its arguments, shared by the session loop, the action
//! executor and the tests. **The model never writes a byte of framing**: it supplies job ids,
//! job bodies, a status word from a fixed list and the key/value pairs of a stats report, and
//! this module decides the reply line, the byte count and the YAML. A job body that happens to
//! contain `\r\nINSERTED 9\r\n` is counted into `<bytes>` and read by the client as body, not as
//! a second reply.
//!
//! The protocol is upstream's `doc/protocol.txt` (beanstalkd 1.13): lines end in CRLF,
//! commands are lower-case and case-sensitive, tube names are at most 200 bytes of
//! `[A-Za-z0-9+/;.$_()-]` and do not begin with `-`, and a job body is `<bytes>` octets followed
//! by CRLF.

/// The longest command line a peer may send, **including** the trailing CRLF.
///
/// Upstream's `LINE_BUF_SIZE` is 224 (a 200-byte tube name plus the longest verb and its
/// numbers). Upstream answers an over-long line with `BAD_FORMAT`; NetGet does too, and then
/// closes, because there is no way to find the start of the next command in a stream that
/// ignored the limit.
pub const MAX_LINE_BYTES: usize = 224;

/// The largest job body `put` may declare: upstream's default `max-job-size` (`-z`, 65535).
///
/// Checked against the **declared** `<bytes>` before a single body byte is read, so a `put`
/// declaring four gigabytes is answered `JOB_TOO_BIG` at once and nothing is buffered for it.
/// The same limit applies to the bodies the model hands out in `RESERVED`/`FOUND`, because a
/// real beanstalkd never holds a job larger than it would have accepted.
pub const MAX_JOB_BYTES: usize = 65_535;

/// The longest tube name (upstream's `MAX_TUBE_NAME_LEN` - 1).
pub const MAX_TUBE_NAME: usize = 200;

/// How many bytes of an oversize body NetGet reads and discards after `JOB_TOO_BIG` to keep the
/// connection in step, as upstream does. A `put` declaring more than this is answered and then
/// closed instead: skipping a gigabyte to stay in sync is not worth holding the connection for.
pub const MAX_DISCARD_BYTES: u64 = 1024 * 1024;

/// The most entries a stats report or tube list the model writes may carry.
pub const MAX_YAML_ENTRIES: usize = 512;

/// The longest scalar value in a stats report.
pub const MAX_YAML_VALUE: usize = 200;

/// Is `name` a tube name upstream would accept?
pub fn valid_tube_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= MAX_TUBE_NAME
        && !name.starts_with('-')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"-+/;.$_()".contains(&b))
}

/// One parsed command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Put {
        priority: u32,
        delay: u32,
        ttr: u32,
        /// The declared body length. `None` when the number does not fit a `u64`, which is
        /// larger than any bound and is refused the same way.
        bytes: Option<u64>,
    },
    Use(String),
    Reserve,
    ReserveWithTimeout(u32),
    ReserveJob(u64),
    Delete(u64),
    Release {
        id: u64,
        priority: u32,
        delay: u32,
    },
    Bury {
        id: u64,
        priority: u32,
    },
    Touch(u64),
    Watch(String),
    Ignore(String),
    Peek(u64),
    PeekReady,
    PeekDelayed,
    PeekBuried,
    Kick(u64),
    KickJob(u64),
    StatsJob(u64),
    StatsTube(String),
    Stats,
    ListTubes,
    ListTubeUsed,
    ListTubesWatched,
    PauseTube {
        tube: String,
        delay: u32,
    },
    Quit,
    /// A known command with missing, extra or malformed arguments.
    BadFormat,
    /// Not a beanstalkd command.
    Unknown,
}

fn num<T: std::str::FromStr>(s: &str) -> Option<T> {
    // Digits only: upstream's strtoul would also take a sign or leading space, but no client
    // sends either and accepting `-1` as a huge unsigned number is not a courtesy.
    if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    s.parse().ok()
}

/// Parse one command line, without its CRLF. Commands are case-sensitive, as upstream's are.
pub fn parse_command(line: &str) -> Command {
    // Upstream separates arguments with single spaces; splitting on runs of spaces accepts the
    // same lines and a few more, and never splits a tube name (which cannot contain a space).
    let parts: Vec<&str> = line.split(' ').filter(|p| !p.is_empty()).collect();
    let Some((verb, args)) = parts.split_first() else {
        return Command::Unknown;
    };
    let tube = |s: &str| -> Option<String> { valid_tube_name(s).then(|| s.to_string()) };
    let parsed = match (*verb, args) {
        ("put", [p, d, t, b]) => (|| {
            Some(Command::Put {
                priority: num(p)?,
                delay: num(d)?,
                ttr: num(t)?,
                bytes: if b.is_empty() || !b.bytes().all(|c| c.is_ascii_digit()) {
                    return None;
                } else {
                    b.parse().ok()
                },
            })
        })(),
        ("use", [t]) => tube(t).map(Command::Use),
        ("reserve", []) => Some(Command::Reserve),
        ("reserve-with-timeout", [s]) => num(s).map(Command::ReserveWithTimeout),
        ("reserve-job", [id]) => num(id).map(Command::ReserveJob),
        ("delete", [id]) => num(id).map(Command::Delete),
        ("release", [id, p, d]) => (|| {
            Some(Command::Release {
                id: num(id)?,
                priority: num(p)?,
                delay: num(d)?,
            })
        })(),
        ("bury", [id, p]) => (|| {
            Some(Command::Bury {
                id: num(id)?,
                priority: num(p)?,
            })
        })(),
        ("touch", [id]) => num(id).map(Command::Touch),
        ("watch", [t]) => tube(t).map(Command::Watch),
        ("ignore", [t]) => tube(t).map(Command::Ignore),
        ("peek", [id]) => num(id).map(Command::Peek),
        ("peek-ready", []) => Some(Command::PeekReady),
        ("peek-delayed", []) => Some(Command::PeekDelayed),
        ("peek-buried", []) => Some(Command::PeekBuried),
        ("kick", [n]) => num(n).map(Command::Kick),
        ("kick-job", [id]) => num(id).map(Command::KickJob),
        ("stats-job", [id]) => num(id).map(Command::StatsJob),
        ("stats-tube", [t]) => tube(t).map(Command::StatsTube),
        ("stats", []) => Some(Command::Stats),
        ("list-tubes", []) => Some(Command::ListTubes),
        ("list-tube-used", []) => Some(Command::ListTubeUsed),
        ("list-tubes-watched", []) => Some(Command::ListTubesWatched),
        ("pause-tube", [t, d]) => (|| {
            Some(Command::PauseTube {
                tube: tube(t)?,
                delay: num(d)?,
            })
        })(),
        ("quit", []) => Some(Command::Quit),
        (
            "put"
            | "use"
            | "reserve"
            | "reserve-with-timeout"
            | "reserve-job"
            | "delete"
            | "release"
            | "bury"
            | "touch"
            | "watch"
            | "ignore"
            | "peek"
            | "peek-ready"
            | "peek-delayed"
            | "peek-buried"
            | "kick"
            | "kick-job"
            | "stats-job"
            | "stats-tube"
            | "stats"
            | "list-tubes"
            | "list-tube-used"
            | "list-tubes-watched"
            | "pause-tube"
            | "quit",
            _,
        ) => None,
        _ => return Command::Unknown,
    };
    parsed.unwrap_or(Command::BadFormat)
}

/// The status words `send_beanstalkd_status` may send. Everything else on the wire — `USING`,
/// `WATCHING`, `NOT_IGNORED`, `BAD_FORMAT`, `UNKNOWN_COMMAND`, `JOB_TOO_BIG`, `EXPECTED_CRLF`
/// — is NetGet's own answer about the connection, and `INSERTED`/`RESERVED`/`FOUND`/`OK` have
/// actions of their own because they carry data.
pub const STATUS_WORDS: &[&str] = &[
    "DELETED",
    "RELEASED",
    "BURIED",
    "TOUCHED",
    "KICKED",
    "PAUSED",
    "NOT_FOUND",
    "TIMED_OUT",
    "DEADLINE_SOON",
    "DRAINING",
    "OUT_OF_MEMORY",
    "INTERNAL_ERROR",
];

/// `STATUS` or `KICKED <count>`.
pub fn render_status(status: &str, count: Option<u64>) -> Result<String, String> {
    let word = status.trim().to_ascii_uppercase();
    if !STATUS_WORDS.contains(&word.as_str()) {
        return Err(format!(
            "status must be one of {}, got {status:?}",
            STATUS_WORDS.join(", ")
        ));
    }
    match (word.as_str(), count) {
        ("KICKED", Some(n)) => Ok(format!("KICKED {n}\r\n")),
        (_, Some(_)) => Err(format!(
            "'count' is only meaningful with KICKED, not {word}"
        )),
        (_, None) => Ok(format!("{word}\r\n")),
    }
}

/// `INSERTED <id>`, or `BURIED <id>` when the job went straight to the buried list.
pub fn render_inserted(id: u64, buried: bool) -> Result<String, String> {
    check_id(id)?;
    Ok(format!(
        "{} {id}\r\n",
        if buried { "BURIED" } else { "INSERTED" }
    ))
}

/// Which job-carrying reply to write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobReply {
    Reserved,
    Found,
}

/// `RESERVED <id> <bytes>\r\n<body>\r\n` or `FOUND …`. The count is the body's byte length,
/// computed here, so whatever the body contains is read as body.
pub fn render_job(kind: JobReply, id: u64, body: &str) -> Result<String, String> {
    check_id(id)?;
    if body.len() > MAX_JOB_BYTES {
        return Err(format!(
            "job body is {} bytes; beanstalkd's max-job-size is {MAX_JOB_BYTES}",
            body.len()
        ));
    }
    let word = match kind {
        JobReply::Reserved => "RESERVED",
        JobReply::Found => "FOUND",
    };
    Ok(format!("{word} {id} {}\r\n{body}\r\n", body.len()))
}

fn check_id(id: u64) -> Result<(), String> {
    if id == 0 {
        return Err("job ids start at 1".to_string());
    }
    Ok(())
}

/// A scalar in a stats report, rendered the way beanstalkd renders it.
fn yaml_scalar(key: &str, value: &serde_json::Value) -> Result<String, String> {
    use serde_json::Value;
    let text = match value {
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::String(s) => s.clone(),
        Value::Null => return Err(format!("stat '{key}' is null; give a number or a string")),
        _ => {
            return Err(format!(
                "stat '{key}' is a list or an object; a beanstalkd stats report is flat \
                 key: value lines"
            ))
        }
    };
    // Clients decode the report as ASCII (greenstalk calls `.decode("ascii")`), and a newline
    // would start a line of its own. Refused rather than rewritten: the model wrote the value
    // and can be told, and a rewritten value would disagree with the decision the log records.
    if text.is_empty()
        || text.len() > MAX_YAML_VALUE
        || !text.bytes().all(|b| (0x20..0x7f).contains(&b))
    {
        return Err(format!(
            "stat '{key}' must be 1..={MAX_YAML_VALUE} printable ASCII characters, got {text:?}"
        ));
    }
    Ok(text)
}

fn valid_stat_key(key: &str) -> bool {
    !key.is_empty()
        && key.len() <= 64
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// `OK <bytes>\r\n---\nkey: value\n…\r\n` — the YAML dictionary `stats`, `stats-tube` and
/// `stats-job` answer with.
pub fn render_stats(stats: &serde_json::Map<String, serde_json::Value>) -> Result<String, String> {
    if stats.len() > MAX_YAML_ENTRIES {
        return Err(format!("at most {MAX_YAML_ENTRIES} stats"));
    }
    let mut yaml = String::from("---\n");
    for (key, value) in stats {
        if !valid_stat_key(key) {
            return Err(format!(
                "stat name {key:?} must be letters, digits, '-' or '_' (like current-jobs-ready)"
            ));
        }
        yaml.push_str(&format!("{key}: {}\n", yaml_scalar(key, value)?));
    }
    Ok(ok_block(&yaml))
}

/// `OK <bytes>\r\n---\n- name\n…\r\n` — the YAML list `list-tubes` and `list-tubes-watched`
/// answer with. Every entry must be a tube name, so no entry can break the list.
pub fn render_tube_list<S: AsRef<str>>(tubes: &[S]) -> Result<String, String> {
    if tubes.len() > MAX_YAML_ENTRIES {
        return Err(format!("at most {MAX_YAML_ENTRIES} tubes"));
    }
    let mut yaml = String::from("---\n");
    for tube in tubes {
        let tube = tube.as_ref();
        if !valid_tube_name(tube) {
            return Err(format!(
                "{tube:?} is not a tube name: 1-{MAX_TUBE_NAME} of letters, digits and \
                 -+/;.$_() not starting with '-'"
            ));
        }
        yaml.push_str(&format!("- {tube}\n"));
    }
    Ok(ok_block(&yaml))
}

fn ok_block(yaml: &str) -> String {
    format!("OK {}\r\n{yaml}\r\n", yaml.len())
}

/// The first line of a rendered reply, split into its word and arguments.
pub fn reply_head(reply: &[u8]) -> (String, Vec<String>) {
    let end = reply
        .windows(2)
        .position(|w| w == b"\r\n")
        .unwrap_or(reply.len());
    let line = String::from_utf8_lossy(&reply[..end]);
    let mut parts = line.split(' ').map(str::to_string);
    let word = parts.next().unwrap_or_default();
    (word, parts.collect())
}

/// Does `reply` — something the executor rendered — answer `command`?
///
/// The executor is stateless and cannot know which command it is answering, so a model can
/// produce a well-formed reply to the wrong command: `RESERVED` to a `delete`, a stats
/// dictionary to `list-tubes`, `KICKED` without a count to `kick`. Each would be read by the
/// client as something else. `OUT_OF_MEMORY` and `INTERNAL_ERROR` answer anything.
pub fn reply_fits(command: &Command, reply: &[u8]) -> bool {
    let (word, args) = reply_head(reply);
    let n = args.len();
    if n == 0 && (word == "OUT_OF_MEMORY" || word == "INTERNAL_ERROR") {
        return true;
    }
    // For `OK <bytes>`, the YAML payload: a list has only `- ` lines, a dictionary none.
    let yaml_lines = || -> Option<Vec<String>> {
        let size: usize = args.first()?.parse().ok()?;
        let start = reply.windows(2).position(|w| w == b"\r\n")? + 2;
        let payload = reply.get(start..start.checked_add(size)?)?;
        let text = std::str::from_utf8(payload).ok()?;
        let body = text.strip_prefix("---\n")?;
        Some(body.lines().map(str::to_string).collect())
    };
    let is_list = || yaml_lines().is_some_and(|l| l.iter().all(|l| l.starts_with("- ")));
    let is_dict = || yaml_lines().is_some_and(|l| l.iter().all(|l| !l.starts_with("- ")));
    use Command::*;
    match command {
        Put { .. } => matches!(
            (word.as_str(), n),
            ("INSERTED", 1) | ("BURIED", 1) | ("DRAINING", 0)
        ),
        Reserve => matches!((word.as_str(), n), ("RESERVED", 2) | ("DEADLINE_SOON", 0)),
        ReserveWithTimeout(_) => matches!(
            (word.as_str(), n),
            ("RESERVED", 2) | ("DEADLINE_SOON", 0) | ("TIMED_OUT", 0)
        ),
        ReserveJob(_) => matches!((word.as_str(), n), ("RESERVED", 2) | ("NOT_FOUND", 0)),
        Delete(_) => matches!((word.as_str(), n), ("DELETED", 0) | ("NOT_FOUND", 0)),
        Release { .. } => matches!(
            (word.as_str(), n),
            ("RELEASED", 0) | ("BURIED", 0) | ("NOT_FOUND", 0)
        ),
        Bury { .. } => matches!((word.as_str(), n), ("BURIED", 0) | ("NOT_FOUND", 0)),
        Touch(_) => matches!((word.as_str(), n), ("TOUCHED", 0) | ("NOT_FOUND", 0)),
        Peek(_) | PeekReady | PeekDelayed | PeekBuried => {
            matches!((word.as_str(), n), ("FOUND", 2) | ("NOT_FOUND", 0))
        }
        Kick(_) => word == "KICKED" && n == 1,
        KickJob(_) => matches!((word.as_str(), n), ("KICKED", 0) | ("NOT_FOUND", 0)),
        Stats => word == "OK" && n == 1 && is_dict(),
        StatsJob(_) | StatsTube(_) => {
            (word == "OK" && n == 1 && is_dict()) || (word == "NOT_FOUND" && n == 0)
        }
        ListTubes => word == "OK" && n == 1 && is_list(),
        PauseTube { .. } => matches!((word.as_str(), n), ("PAUSED", 0) | ("NOT_FOUND", 0)),
        _ => false,
    }
}

/// Is this reply a refusal (the model saying no) rather than an answer?
pub fn is_refusal(reply: &[u8]) -> bool {
    let (word, _) = reply_head(reply);
    matches!(
        word.as_str(),
        "NOT_FOUND"
            | "TIMED_OUT"
            | "DEADLINE_SOON"
            | "DRAINING"
            | "OUT_OF_MEMORY"
            | "INTERNAL_ERROR"
    )
}
