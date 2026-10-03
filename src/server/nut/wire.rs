//! RFC 9271/NUT framing shared by the server and client. No application storage.
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncBufReadExt};

pub const MAX_LINE_BYTES: usize = 8192;
pub const MAX_RESPONSE_BYTES: usize = 1024 * 1024;
pub const MAX_ENTRIES: usize = 4096;
pub const IO_TIMEOUT: Duration = Duration::from_secs(30);

pub fn quote(s: &str) -> Result<String> {
    ensure!(
        s.bytes().all(|b| (32..=126).contains(&b)),
        "NUT strings must contain printable ASCII only"
    );
    Ok(format!(
        "\"{}\"",
        s.replace('\\', "\\\\").replace('"', "\\\"")
    ))
}
pub fn token(s: &str) -> Result<&str> {
    ensure!(
        !s.is_empty()
            && s.bytes()
                .all(|b| (33..=126).contains(&b) && b != b'"' && b != b'\\'),
        "Invalid NUT identifier"
    );
    Ok(s)
}
/// Parse quoted strings without permitting newline injection or unfinished escapes.
pub fn split(line: &str) -> Result<Vec<String>> {
    ensure!(
        line.bytes().all(|b| (32..=126).contains(&b)),
        "Invalid NUT character"
    );
    let mut out = Vec::new();
    let mut chars = line.chars().peekable();
    while chars.peek().is_some() {
        while chars.peek() == Some(&' ') {
            chars.next();
        }
        if chars.peek().is_none() {
            break;
        }
        let quoted = chars.peek() == Some(&'"');
        if quoted {
            chars.next();
        }
        let mut field = String::new();
        let mut closed = !quoted;
        while let Some(c) = chars.next() {
            if c == '\\' {
                let c = chars.next().context("Incomplete escape")?;
                ensure!(c == '\\' || c == '"', "Invalid escape");
                field.push(c);
            } else if c == '"' {
                ensure!(quoted, "Unexpected quote");
                closed = true;
                ensure!(
                    chars.peek().is_none() || chars.peek() == Some(&' '),
                    "Text after quoted string"
                );
                break;
            } else if c == ' ' && !quoted {
                break;
            } else {
                field.push(c);
            }
        }
        ensure!(closed, "Unterminated quoted string");
        out.push(field);
        ensure!(out.len() <= 16, "Too many NUT fields");
    }
    ensure!(!out.is_empty(), "Empty NUT line");
    Ok(out)
}
/// A whole-line deadline and a hard allocation bound; EOF never completes a partial line.
pub async fn read_line<R: AsyncBufRead + Unpin>(
    r: &mut R,
    timeout: Duration,
) -> Result<Option<String>> {
    tokio::time::timeout(timeout, async {
        let mut line = Vec::new();
        loop {
            let bytes = r.fill_buf().await?;
            if bytes.is_empty() {
                ensure!(line.is_empty(), "Truncated NUT line");
                return Ok(None);
            }
            let count = bytes
                .iter()
                .position(|b| *b == b'\n')
                .map(|n| n + 1)
                .unwrap_or(bytes.len());
            ensure!(
                line.len() + count <= MAX_LINE_BYTES,
                "NUT line exceeds limit"
            );
            line.extend_from_slice(&bytes[..count]);
            r.consume(count);
            if line.last() == Some(&b'\n') {
                line.pop();
                // Tolerate CRLF peers; outbound framing is LF as RFC 9271 specifies.
                if line.last() == Some(&b'\r') {
                    line.pop();
                }
                return Ok(Some(String::from_utf8(line)?));
            }
        }
    })
    .await
    .context("NUT line deadline exceeded")?
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct Request {
    pub operation: String,
    pub ups: Option<String>,
    pub name: Option<String>,
    pub value: Option<String>,
}
impl Request {
    pub fn from_action(v: &Value) -> Result<Self> {
        let r: Self = serde_json::from_value(v.clone())?;
        r.encode()?;
        Ok(r)
    }
    pub fn encode(&self) -> Result<String> {
        let ups = || token(self.ups.as_deref().context("Missing ups")?).map(str::to_owned);
        let name = || token(self.name.as_deref().context("Missing name")?).map(str::to_owned);
        let value = || quote(self.value.as_deref().context("Missing value")?);
        let line = match self.operation.as_str() {
            "list_ups" => "LIST UPS".into(),
            "list_var" | "list_rw" | "list_cmd" => format!(
                "LIST {} {}",
                &self.operation[5..].to_ascii_uppercase(),
                ups()?
            ),
            "list_enum" => format!("LIST ENUM {} {}", ups()?, name()?),
            "get_var" | "get_desc" | "get_cmddesc" | "get_type" => format!(
                "GET {} {} {}",
                &self.operation[4..].to_ascii_uppercase(),
                ups()?,
                name()?
            ),
            "get_upsdesc" => format!("GET UPSDESC {}", ups()?),
            "set_var" => format!("SET VAR {} {} {}", ups()?, name()?, value()?),
            "instcmd" => format!("INSTCMD {} {}", ups()?, name()?),
            "username" | "password" => {
                format!("{} {}", self.operation.to_ascii_uppercase(), value()?)
            }
            "logout" => "LOGOUT".into(),
            _ => bail!("Unsupported NUT operation: {}", self.operation),
        };
        ensure!(line.len() + 1 <= MAX_LINE_BYTES, "NUT request too long");
        Ok(format!("{line}\n"))
    }
    pub fn parse(line: &str) -> Result<Self> {
        let p = split(line)?;
        let a: Vec<&str> = p.iter().map(String::as_str).collect();
        let (op, ups, name, value) = match a.as_slice() {
            ["LIST", "UPS"] => ("list_ups".into(), None, None, None),
            ["LIST", kind @ ("VAR" | "RW" | "CMD"), ups] => (
                format!("list_{}", kind.to_ascii_lowercase()),
                Some(*ups),
                None,
                None,
            ),
            ["LIST", "ENUM", ups, name] => ("list_enum".into(), Some(*ups), Some(*name), None),
            ["GET", kind @ ("VAR" | "DESC" | "CMDDESC" | "TYPE"), ups, name] => (
                format!("get_{}", kind.to_ascii_lowercase()),
                Some(*ups),
                Some(*name),
                None,
            ),
            ["GET", "UPSDESC", ups] => ("get_upsdesc".into(), Some(*ups), None, None),
            ["SET", "VAR", ups, name, value] => {
                ("set_var".into(), Some(*ups), Some(*name), Some(*value))
            }
            ["INSTCMD", ups, name] => ("instcmd".into(), Some(*ups), Some(*name), None),
            [kind @ ("USERNAME" | "PASSWORD"), value] => {
                (kind.to_ascii_lowercase(), None, None, Some(*value))
            }
            ["LOGOUT"] | ["DETACH"] => ("logout".into(), None, None, None),
            _ => bail!("Unsupported command or invalid arguments"),
        };
        let r = Self {
            operation: op,
            ups: ups.map(str::to_owned),
            name: name.map(str::to_owned),
            value: value.map(str::to_owned),
        };
        r.encode()?;
        Ok(r)
    }
    pub fn is_write(&self) -> bool {
        matches!(self.operation.as_str(), "set_var" | "instcmd")
    }
    pub fn list_suffix(&self) -> Result<String> {
        Ok(self
            .encode()?
            .trim_end()
            .strip_prefix("LIST ")
            .context("Not a list")?
            .to_owned())
    }
    pub fn public_data(&self) -> Value {
        let mut v = json!(self);
        if self.operation == "password" {
            v["value"] = json!("[redacted]");
        }
        v
    }
}

pub fn error_reply(error: &str) -> Result<String> {
    ensure!(
        matches!(
            error,
            "ACCESS-DENIED"
                | "UNKNOWN-UPS"
                | "VAR-NOT-SUPPORTED"
                | "CMD-NOT-SUPPORTED"
                | "INVALID-ARGUMENT"
                | "DATA-STALE"
                | "DRIVER-NOT-CONNECTED"
                | "READONLY"
                | "SET-FAILED"
                | "INSTCMD-FAILED"
                | "UNKNOWN-COMMAND"
                | "FEATURE-NOT-SUPPORTED"
                | "ALREADY-SET"
                | "USERNAME-REQUIRED"
                | "PASSWORD-REQUIRED"
        ),
        "Unsupported NUT error token"
    );
    Ok(format!("ERR {error}\n"))
}
fn field<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    v[key]
        .as_str()
        .with_context(|| format!("Missing string {key}"))
}
/// Render reply identities from the actual request, never from model-supplied framing.
pub fn render(r: &Request, v: &Value) -> Result<String> {
    if let Some(error) = v.get("error") {
        return error_reply(error.as_str().context("error must be a string")?);
    }
    let ups = r.ups.as_deref().unwrap_or("");
    let name = r.name.as_deref().unwrap_or("");
    let op = r.operation.as_str();
    let mut lines = Vec::new();
    if op.starts_with("list_") {
        let entries = v["entries"].as_array().context("Missing entries array")?;
        ensure!(entries.len() <= MAX_ENTRIES, "NUT list exceeds entry limit");
        let suffix = r.list_suffix()?;
        lines.push(format!("BEGIN LIST {suffix}"));
        for e in entries {
            lines.push(match op {
                "list_ups" => format!(
                    "UPS {} {}",
                    token(field(e, "name")?)?,
                    quote(field(e, "description")?)?
                ),
                "list_var" | "list_rw" => format!(
                    "{} {} {} {}",
                    &op[5..].to_ascii_uppercase(),
                    ups,
                    token(field(e, "name")?)?,
                    quote(field(e, "value")?)?
                ),
                "list_cmd" => format!("CMD {} {}", ups, token(field(e, "name")?)?),
                "list_enum" => format!("ENUM {} {} {}", ups, name, quote(field(e, "value")?)?),
                _ => bail!("Unsupported list"),
            });
        }
        lines.push(format!("END LIST {suffix}"));
    } else {
        lines.push(match op {
            "get_var" | "get_desc" | "get_cmddesc" => format!(
                "{} {} {} {}",
                &op[4..].to_ascii_uppercase(),
                ups,
                name,
                quote(field(v, "value")?)?
            ),
            "get_upsdesc" => format!("UPSDESC {} {}", ups, quote(field(v, "value")?)?),
            "get_type" => {
                let types = v["types"].as_array().context("Missing types")?;
                ensure!(!types.is_empty() && types.len() <= 8, "Invalid types count");
                let mut out = format!("TYPE {ups} {name}");
                for t in types {
                    let t = t.as_str().context("Type must be string")?;
                    ensure!(
                        matches!(t, "RW" | "ENUM" | "RANGE" | "NUMBER")
                            || t.strip_prefix("STRING:")
                                .and_then(|s| s.parse::<u32>().ok())
                                .is_some(),
                        "Invalid variable type"
                    );
                    out.push(' ');
                    out.push_str(t);
                }
                out
            }
            "set_var" | "instcmd" => {
                ensure!(v["ok"] == true, "Write needs explicit ok=true");
                "OK".into()
            }
            _ => bail!("Unsupported response operation"),
        });
    }
    ensure!(
        lines.iter().all(|s| s.len() < MAX_LINE_BYTES),
        "NUT response line too long"
    );
    let out = format!("{}\n", lines.join("\n"));
    ensure!(out.len() <= MAX_RESPONSE_BYTES, "NUT response too large");
    Ok(out)
}

/// Read and validate a complete response, matching every row and terminator to the request.
pub async fn read_response<R: AsyncBufRead + Unpin>(reader: &mut R, r: &Request) -> Result<Value> {
    read_response_with_timeout(reader, r, IO_TIMEOUT).await
}

/// Same parser with an explicit transaction deadline, including all list rows.
pub async fn read_response_with_timeout<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    r: &Request,
    timeout: Duration,
) -> Result<Value> {
    tokio::time::timeout(timeout, async {
        let first = read_line(reader, IO_TIMEOUT)
            .await?
            .context("EOF before NUT reply")?;
        let fields = split(&first)?;
        if fields.first().map(String::as_str) == Some("ERR") {
            ensure!(fields.len() == 2, "Malformed NUT error");
            return Ok(json!({"error": fields[1]}));
        }
        if r.operation.starts_with("list_") {
            let suffix = r.list_suffix()?;
            ensure!(
                first == format!("BEGIN LIST {suffix}"),
                "Mismatched list header"
            );
            let mut entries = Vec::new();
            let mut bytes = first.len();
            loop {
                let line = read_line(reader, IO_TIMEOUT)
                    .await?
                    .context("Truncated NUT list")?;
                bytes += line.len() + 1;
                ensure!(bytes <= MAX_RESPONSE_BYTES, "NUT response too large");
                if line == format!("END LIST {suffix}") {
                    break;
                }
                ensure!(entries.len() < MAX_ENTRIES, "Too many NUT entries");
                let p = split(&line)?;
                entries.push(parse_row(r, &p)?);
            }
            Ok(json!({"entries": entries}))
        } else if matches!(
            r.operation.as_str(),
            "username" | "password" | "set_var" | "instcmd" | "logout"
        ) {
            ensure!(
                first == "OK" || (r.operation == "logout" && first == "OK Goodbye"),
                "Unexpected NUT acknowledgement"
            );
            Ok(json!({"ok": true}))
        } else {
            parse_row(r, &fields)
        }
    })
    .await
    .context("NUT response deadline exceeded")?
}
fn parse_row(r: &Request, p: &[String]) -> Result<Value> {
    let op = r.operation.as_str();
    let kind = op
        .split_once('_')
        .context("Invalid operation")?
        .1
        .to_ascii_uppercase();
    ensure!(p.first() == Some(&kind), "Mismatched NUT response kind");
    if op == "list_ups" {
        ensure!(p.len() == 3, "Malformed UPS entry");
        token(&p[1])?;
        return Ok(json!({"name": p[1], "description": p[2]}));
    }
    ensure!(p.get(1) == r.ups.as_ref(), "Mismatched UPS");
    if op == "get_upsdesc" {
        ensure!(p.len() == 3, "Malformed UPSDESC");
        return Ok(json!({"value": p[2]}));
    }
    if op == "list_cmd" {
        ensure!(p.len() == 3, "Malformed CMD");
        token(&p[2])?;
        return Ok(json!({"name": p[2]}));
    }
    if !matches!(op, "list_var" | "list_rw") {
        ensure!(
            p.get(2) == r.name.as_ref(),
            "Mismatched variable or command"
        );
    }
    if op == "get_type" {
        ensure!(p.len() >= 4, "Missing TYPE value");
        return Ok(json!({"types": &p[3..]}));
    }
    ensure!(p.len() == 4, "Malformed NUT value");
    Ok(if matches!(op, "list_var" | "list_rw") {
        token(&p[2])?;
        json!({"name": p[2], "value": p[3]})
    } else {
        json!({"value": p[3]})
    })
}
