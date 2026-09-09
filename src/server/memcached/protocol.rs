//! Memcached **text** protocol parsing and reply framing.
//!
//! Pure functions over bytes: no sockets, no state, and — importantly — **no cache**.
//! There is no map, no table, no store anywhere in this protocol. The model answers every
//! `get`. That is the project rule (protocols must not implement storage) and it is also the
//! entire point of running Memcached under NetGet: the interesting behaviour is a model
//! deciding what a key holds, not a hash map doing what hash maps do.
//!
//! Scope is the text protocol, as advised: the binary protocol was deprecated in memcached
//! 1.6 (2020) and is no longer documented in upstream `protocol.txt`.

/// Longest command line accepted before the connection is failed.
///
/// Real memcached caps the key at 250 bytes and the whole command line well below this; a
/// line this long is a client bug or an attack, not a request.
pub const MAX_COMMAND_LINE: usize = 8 * 1024;

/// Upstream memcached's key limit (`KEY_MAX_LENGTH`).
pub const MAX_KEY_LEN: usize = 250;

/// Upstream memcached's default item size limit (1 MiB).
pub const MAX_VALUE_LEN: usize = 1024 * 1024;

/// A parsed client command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// `get <key>*` / `gets <key>*`
    Retrieval {
        /// `"get"` or `"gets"`; `gets` additionally returns the CAS unique
        command: &'static str,
        keys: Vec<String>,
    },
    /// `set|add|replace|append|prepend <key> <flags> <exptime> <bytes> [noreply]`
    /// followed by exactly `bytes` octets and CRLF.
    Storage {
        command: &'static str,
        key: String,
        flags: u32,
        exptime: i64,
        bytes: usize,
        /// Present only for `cas`
        cas_unique: Option<u64>,
        noreply: bool,
        data: Vec<u8>,
    },
    /// `delete <key> [noreply]`
    Delete { key: String, noreply: bool },
    /// `incr|decr <key> <value> [noreply]`
    Arithmetic {
        command: &'static str,
        key: String,
        delta: u64,
        noreply: bool,
    },
    /// `touch <key> <exptime> [noreply]`
    Touch {
        key: String,
        exptime: i64,
        noreply: bool,
    },
    /// `stats [<argument>]`
    Stats { argument: Option<String> },
    /// `version`
    Version,
    /// `flush_all [<delay>] [noreply]`
    FlushAll { delay: i64, noreply: bool },
    /// `quit` — close without replying
    Quit,
    /// A line this server does not recognise. Reported to the model rather than answered
    /// with a canned `ERROR`, so it can decide what an unknown verb should do.
    Unknown { line: String },
}

/// Why a line could not be turned into a `Command`.
///
/// These map onto memcached's own `ERROR` / `CLIENT_ERROR` replies, which are part of the
/// protocol rather than transport failures.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParseError {
    /// Structurally wrong: `CLIENT_ERROR <message>`
    ClientError(String),
    /// The storage command's data block has not fully arrived yet. Not an error — the caller
    /// must read more bytes and retry. Carries the total frame length it is waiting for.
    Incomplete,
}

/// Result of trying to take one command off the front of a buffer.
#[derive(Debug)]
pub enum Parsed {
    /// A complete command, and how many bytes of the buffer it consumed.
    Complete { command: Command, consumed: usize },
    /// Not enough bytes yet; leave the buffer alone and read more.
    Incomplete,
    /// The command line was well-framed but malformed. `consumed` bytes should still be
    /// dropped so the connection can continue.
    ///
    /// For a storage command this **includes its data block**: rejecting the header while
    /// leaving `<bytes>` octets of attacker-chosen payload in the stream means those octets
    /// are then parsed as commands. See `parse_storage`.
    Invalid { message: String, consumed: usize },
    /// The command line was malformed in a way that makes resynchronising impossible: the
    /// caller must reply and then **close the connection**.
    ///
    /// This is only reachable from a storage command whose `<bytes>` count is missing,
    /// unparseable, or larger than `MAX_VALUE_LEN`. Every other malformed command is
    /// self-delimiting (one CRLF-terminated line), so the parser can skip it and carry on;
    /// a storage command whose length is unknown or unbufferable has no such boundary, and
    /// guessing one would put the peer's payload back on the command path.
    Fatal { message: String },
}

/// Try to take one command from the front of `buffer`.
///
/// Framing rules that matter, and that are easy to get wrong:
///
/// - Every command line ends with `\r\n`. A bare `\n` is **not** accepted; upstream is
///   lenient about this in places but a strict server keeps clients honest and keeps byte
///   counting unambiguous.
/// - A storage command is followed by *exactly* `<bytes>` octets and then `\r\n`. The count
///   is authoritative: the data block may itself contain `\r\n`, so scanning for a delimiter
///   instead of counting is the classic memcached implementation bug.
pub fn parse_command(buffer: &[u8]) -> Parsed {
    let Some(line_end) = find_crlf(buffer) else {
        if buffer.len() > MAX_COMMAND_LINE {
            return Parsed::Invalid {
                message: format!("command line exceeds {} bytes", MAX_COMMAND_LINE),
                consumed: buffer.len(),
            };
        }
        return Parsed::Incomplete;
    };

    let line_bytes = &buffer[..line_end];
    let header_len = line_end + 2;

    let line = match std::str::from_utf8(line_bytes) {
        Ok(s) => s,
        Err(_) => {
            return Parsed::Invalid {
                message: "command line is not valid UTF-8".to_string(),
                consumed: header_len,
            }
        }
    };

    let parts: Vec<&str> = line.split_ascii_whitespace().collect();
    if parts.is_empty() {
        return Parsed::Invalid {
            message: "empty command".to_string(),
            consumed: header_len,
        };
    }

    match parts[0] {
        "get" | "gets" => {
            let command = if parts[0] == "get" { "get" } else { "gets" };
            if parts.len() < 2 {
                return Parsed::Invalid {
                    message: format!("{} requires at least one key", command),
                    consumed: header_len,
                };
            }
            let keys: Vec<String> = parts[1..].iter().map(|s| s.to_string()).collect();
            if let Some(bad) = keys.iter().find(|k| k.len() > MAX_KEY_LEN) {
                return Parsed::Invalid {
                    message: format!("key is {} bytes, maximum is {}", bad.len(), MAX_KEY_LEN),
                    consumed: header_len,
                };
            }
            Parsed::Complete {
                command: Command::Retrieval { command, keys },
                consumed: header_len,
            }
        }

        cmd @ ("set" | "add" | "replace" | "append" | "prepend" | "cas") => {
            parse_storage(cmd, &parts, buffer, header_len)
        }

        "delete" => {
            if parts.len() < 2 {
                return Parsed::Invalid {
                    message: "delete requires a key".to_string(),
                    consumed: header_len,
                };
            }
            Parsed::Complete {
                command: Command::Delete {
                    key: parts[1].to_string(),
                    noreply: has_noreply(&parts),
                },
                consumed: header_len,
            }
        }

        cmd @ ("incr" | "decr") => {
            let command = if cmd == "incr" { "incr" } else { "decr" };
            if parts.len() < 3 {
                return Parsed::Invalid {
                    message: format!("{} requires a key and a value", command),
                    consumed: header_len,
                };
            }
            let delta = match parts[2].parse::<u64>() {
                Ok(d) => d,
                Err(_) => {
                    return Parsed::Invalid {
                        message: "invalid numeric delta argument".to_string(),
                        consumed: header_len,
                    }
                }
            };
            Parsed::Complete {
                command: Command::Arithmetic {
                    command,
                    key: parts[1].to_string(),
                    delta,
                    noreply: has_noreply(&parts),
                },
                consumed: header_len,
            }
        }

        "touch" => {
            if parts.len() < 3 {
                return Parsed::Invalid {
                    message: "touch requires a key and an expiry".to_string(),
                    consumed: header_len,
                };
            }
            let exptime = match parts[2].parse::<i64>() {
                Ok(v) => v,
                Err(_) => {
                    return Parsed::Invalid {
                        message: "invalid exptime argument".to_string(),
                        consumed: header_len,
                    }
                }
            };
            Parsed::Complete {
                command: Command::Touch {
                    key: parts[1].to_string(),
                    exptime,
                    noreply: has_noreply(&parts),
                },
                consumed: header_len,
            }
        }

        "stats" => Parsed::Complete {
            command: Command::Stats {
                argument: parts.get(1).map(|s| s.to_string()),
            },
            consumed: header_len,
        },

        "version" => Parsed::Complete {
            command: Command::Version,
            consumed: header_len,
        },

        "flush_all" => {
            let delay = parts
                .get(1)
                .filter(|s| **s != "noreply")
                .and_then(|s| s.parse::<i64>().ok())
                .unwrap_or(0);
            Parsed::Complete {
                command: Command::FlushAll {
                    delay,
                    noreply: has_noreply(&parts),
                },
                consumed: header_len,
            }
        }

        "quit" => Parsed::Complete {
            command: Command::Quit,
            consumed: header_len,
        },

        _ => Parsed::Complete {
            command: Command::Unknown {
                line: line.to_string(),
            },
            consumed: header_len,
        },
    }
}

fn has_noreply(parts: &[&str]) -> bool {
    parts.last().map(|s| *s == "noreply").unwrap_or(false)
}

/// `<command> <key> <flags> <exptime> <bytes> [<cas unique>] [noreply]\r\n<data>\r\n`
///
/// **`<bytes>` is parsed before anything else, and every rejection below it consumes the whole
/// frame.** A storage command is not self-delimiting: its data block is `<bytes>` octets that
/// may contain anything, CRLF included. Rejecting the header and consuming only the header
/// leaves that payload at the front of the buffer, where the next `parse_command` reads it as
/// commands — so
///
/// ```text
/// set k bad 0 7\r\nversion\r\n
/// ```
///
/// answered `CLIENT_ERROR` for the header and then *executed* `version`. Anything the peer can
/// put in a value it could put on the command path. Upstream memcached avoids this by entering
/// `conn_swallow` for exactly `<bytes> + 2` octets, which is what consuming `frame_len` does
/// here.
///
/// Where `<bytes>` itself is unusable there is no boundary to skip to, and the only safe exit
/// is `Fatal` — reply and close. Guessing a boundary is the bug above.
fn parse_storage(cmd: &str, parts: &[&str], buffer: &[u8], header_len: usize) -> Parsed {
    let is_cas = cmd == "cas";
    let min_parts = if is_cas { 6 } else { 5 };

    let Some(bytes_field) = parts.get(4) else {
        return Parsed::Fatal {
            message: format!("bad data chunk: {} needs {} arguments", cmd, min_parts - 1),
        };
    };
    let bytes = match bytes_field.parse::<usize>() {
        Ok(v) => v,
        Err(_) => {
            return Parsed::Fatal {
                message: "bad command line format: bytes must be a non-negative integer"
                    .to_string(),
            }
        }
    };
    if bytes > MAX_VALUE_LEN {
        // Skipping this would mean buffering an arbitrary number of octets purely to throw
        // them away, which is the allocation the cap exists to prevent.
        return Parsed::Fatal {
            message: format!("object too large for cache ({} bytes)", bytes),
        };
    }

    // From here the frame's extent is known. Wait for all of it first, so that any rejection
    // below drops the data block along with the header rather than leaving it to be parsed.
    // The count is authoritative — the payload may itself contain CRLF.
    let frame_len = header_len + bytes + 2;
    if buffer.len() < frame_len {
        return Parsed::Incomplete;
    }
    let reject = |message: String| Parsed::Invalid {
        message,
        consumed: frame_len,
    };

    // Only reachable for `cas` with five parts: fewer than five was caught above, because
    // `parts.get(4)` is where `<bytes>` lives.
    if parts.len() < min_parts {
        return reject(format!(
            "bad data chunk: {} needs {} arguments",
            cmd,
            min_parts - 1
        ));
    }

    let key = parts[1].to_string();
    if key.len() > MAX_KEY_LEN {
        return reject(format!(
            "key is {} bytes, maximum is {}",
            key.len(),
            MAX_KEY_LEN
        ));
    }

    let flags = match parts[2].parse::<u32>() {
        Ok(v) => v,
        Err(_) => {
            return reject(
                "bad command line format: flags must be a 32-bit unsigned integer".to_string(),
            )
        }
    };
    let exptime = match parts[3].parse::<i64>() {
        Ok(v) => v,
        Err(_) => return reject("bad command line format: exptime must be an integer".to_string()),
    };
    let cas_unique = if is_cas {
        match parts[5].parse::<u64>() {
            Ok(v) => Some(v),
            Err(_) => {
                return reject(
                    "bad command line format: cas unique must be a 64-bit integer".to_string(),
                )
            }
        }
    } else {
        None
    };

    let data = buffer[header_len..header_len + bytes].to_vec();
    if &buffer[header_len + bytes..frame_len] != b"\r\n" {
        return reject("bad data chunk".to_string());
    }

    let command = match cmd {
        "set" => "set",
        "add" => "add",
        "replace" => "replace",
        "append" => "append",
        "prepend" => "prepend",
        _ => "cas",
    };

    Parsed::Complete {
        command: Command::Storage {
            command,
            key,
            flags,
            exptime,
            bytes,
            cas_unique,
            noreply: has_noreply(parts),
            data,
        },
        consumed: frame_len,
    }
}

fn find_crlf(buffer: &[u8]) -> Option<usize> {
    buffer.windows(2).position(|w| w == b"\r\n")
}

/// Does this command suppress its reply when the client asked for `noreply`?
pub fn is_noreply(command: &Command) -> bool {
    match command {
        Command::Storage { noreply, .. }
        | Command::Delete { noreply, .. }
        | Command::Arithmetic { noreply, .. }
        | Command::Touch { noreply, .. }
        | Command::FlushAll { noreply, .. } => *noreply,
        _ => false,
    }
}

/// One value in a `get`/`gets` reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ValueItem {
    pub key: String,
    pub flags: u32,
    pub data: Vec<u8>,
    /// Only emitted for `gets`
    pub cas_unique: Option<u64>,
}

/// Frame a retrieval reply.
///
/// ```text
/// VALUE <key> <flags> <bytes> [<cas unique>]\r\n
/// <data block>\r\n
/// ...
/// END\r\n
/// ```
///
/// `<bytes>` is computed here from the actual payload length. It is never taken from the
/// model: a byte count that disagrees with the payload desynchronises the client's parser
/// for the rest of the connection, and a model asked to count bytes will eventually
/// miscount. An empty item list is a cache miss, which is `END\r\n` alone.
pub fn encode_values(items: &[ValueItem], include_cas: bool) -> Vec<u8> {
    let mut out = Vec::new();
    for item in items {
        let header = match (include_cas, item.cas_unique) {
            (true, Some(cas)) => format!(
                "VALUE {} {} {} {}\r\n",
                item.key,
                item.flags,
                item.data.len(),
                cas
            ),
            _ => format!("VALUE {} {} {}\r\n", item.key, item.flags, item.data.len()),
        };
        out.extend_from_slice(header.as_bytes());
        out.extend_from_slice(&item.data);
        out.extend_from_slice(b"\r\n");
    }
    out.extend_from_slice(b"END\r\n");
    out
}

/// Frame a `stats` reply: `STAT <name> <value>\r\n` per entry, then `END\r\n`.
pub fn encode_stats(entries: &[(String, String)]) -> Vec<u8> {
    let mut out = Vec::new();
    for (name, value) in entries {
        out.extend_from_slice(format!("STAT {} {}\r\n", name, value).as_bytes());
    }
    out.extend_from_slice(b"END\r\n");
    out
}

/// Fixed status lines the text protocol defines.
pub fn status_line(status: &str) -> Option<&'static str> {
    match status {
        "STORED" => Some("STORED\r\n"),
        "NOT_STORED" => Some("NOT_STORED\r\n"),
        "EXISTS" => Some("EXISTS\r\n"),
        "NOT_FOUND" => Some("NOT_FOUND\r\n"),
        "DELETED" => Some("DELETED\r\n"),
        "TOUCHED" => Some("TOUCHED\r\n"),
        "OK" => Some("OK\r\n"),
        "ERROR" => Some("ERROR\r\n"),
        _ => None,
    }
}
