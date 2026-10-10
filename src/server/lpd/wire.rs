//! LPD (RFC 1179) framing shared by the server and the client: one command per connection,
//! LF-terminated command lines, receive-job subcommands carrying a byte count, the file's
//! bytes and a trailing NUL, and the control-file format.
//!
//! Every count is checked against its bound before a byte of the file is read.
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncRead, AsyncReadExt};

/// A daemon command or receive-job subcommand line, including its LF.
pub const MAX_COMMAND_LINE: usize = 1024;
/// A control file is a few short lines; LPRng and BSD lpd keep them well under this.
pub const MAX_CONTROL_FILE: u64 = 64 * 1024;
/// Default total size of the data files in one job.
pub const DEFAULT_MAX_JOB_BYTES: u64 = 16 * 1024 * 1024;
/// Upper bound on the configurable job size.
pub const MAX_JOB_BYTES_LIMIT: u64 = 256 * 1024 * 1024;
/// Data files one control file may reference (dfA..dfZ, dfa..dfz).
pub const MAX_FILES_PER_JOB: usize = 52;
/// Text of a data file shown to a handler.
pub const MAX_PREVIEW: usize = 64 * 1024;
/// Bytes of a queue listing or removal reply the client accepts.
pub const MAX_REPLY_BYTES: usize = 1024 * 1024;
/// Connect, write, acknowledgement and per-read deadline.
pub const IO_TIMEOUT: Duration = Duration::from_secs(30);

/// Daemon command codes (RFC 1179 section 5).
pub const CMD_PRINT_WAITING: u8 = 0x01;
pub const CMD_RECEIVE_JOB: u8 = 0x02;
pub const CMD_QUEUE_SHORT: u8 = 0x03;
pub const CMD_QUEUE_LONG: u8 = 0x04;
pub const CMD_REMOVE_JOBS: u8 = 0x05;
/// Receive-job subcommand codes (RFC 1179 section 6).
pub const SUB_ABORT: u8 = 0x01;
pub const SUB_CONTROL_FILE: u8 = 0x02;
pub const SUB_DATA_FILE: u8 = 0x03;

/// Read one LF-terminated line, refusing past `MAX_COMMAND_LINE`. `None` at a clean EOF.
pub async fn read_line<R: AsyncBufRead + Unpin>(
    reader: &mut R,
    deadline: Duration,
) -> Result<Option<Vec<u8>>> {
    tokio::time::timeout(deadline, async {
        let mut line = Vec::new();
        loop {
            let available = reader.fill_buf().await?;
            if available.is_empty() {
                if line.is_empty() {
                    return Ok(None);
                }
                bail!("LPD peer closed mid-command");
            }
            let (chunk, found) = match available.iter().position(|b| *b == b'\n') {
                Some(i) => (&available[..=i], true),
                None => (available, false),
            };
            ensure!(
                line.len() + chunk.len() <= MAX_COMMAND_LINE,
                "LPD command line exceeds {MAX_COMMAND_LINE} bytes"
            );
            line.extend_from_slice(chunk);
            let taken = chunk.len();
            reader.consume(taken);
            if found {
                line.pop();
                return Ok(Some(line));
            }
        }
    })
    .await
    .context("LPD read deadline")?
}

/// Read a file body of exactly `count` bytes and its terminating NUL.
pub async fn read_file<R: AsyncRead + Unpin>(
    reader: &mut R,
    count: u64,
    deadline: Duration,
) -> Result<Vec<u8>> {
    let mut body = vec![0u8; usize::try_from(count)?];
    tokio::time::timeout(deadline, async {
        reader.read_exact(&mut body).await?;
        let mut nul = [0u8; 1];
        reader.read_exact(&mut nul).await?;
        ensure!(nul[0] == 0, "LPD file not terminated by a NUL byte");
        Ok(())
    })
    .await
    .context("LPD file transfer deadline")??;
    Ok(body)
}

/// A queue name or job-list token: printable, no whitespace, short.
pub fn valid_token(token: &str) -> bool {
    !token.is_empty() && token.len() <= 128 && token.bytes().all(|b| b.is_ascii_graphic())
}

/// Split a command line's operands on spaces and tabs.
pub fn operands(line: &[u8]) -> Vec<String> {
    String::from_utf8_lossy(line)
        .split([' ', '\t'])
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .collect()
}

/// `cfA123host` / `dfA123host`: the three-digit job number and the originating host.
pub fn job_number(name: &str) -> Option<(String, String)> {
    let rest = name.get(3..)?;
    let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
    if digits.is_empty() {
        return None;
    }
    Some((digits.clone(), rest[digits.len()..].to_string()))
}

/// A parsed control file.
#[derive(Debug, Clone, Default)]
pub struct ControlFile {
    pub host: Option<String>,
    pub user: Option<String>,
    pub job_name: Option<String>,
    pub title: Option<String>,
    pub class: Option<String>,
    pub banner_user: Option<String>,
    pub mail: Option<String>,
    /// (format letter, data file name, source file name if an N line preceded it)
    pub prints: Vec<(char, String, Option<String>)>,
    pub unlink: Vec<String>,
}

/// Print-file format letters RFC 1179 defines.
const FORMATS: &str = "cdfglnoprtvz";

impl ControlFile {
    pub fn parse(bytes: &[u8]) -> Result<Self> {
        let text = std::str::from_utf8(bytes).context("LPD control file is not UTF-8")?;
        let mut control = ControlFile::default();
        let mut source: Option<String> = None;
        for line in text.split('\n') {
            let line = line.strip_suffix('\r').unwrap_or(line);
            let mut chars = line.chars();
            let Some(code) = chars.next() else { continue };
            let value = chars.as_str().to_string();
            ensure!(
                !crate::utils::sanitize::has_controls(&value),
                "LPD control line contains control characters"
            );
            match code {
                'H' => control.host = Some(value),
                'P' => control.user = Some(value),
                'J' => control.job_name = Some(value),
                'T' => control.title = Some(value),
                'C' => control.class = Some(value),
                'L' => control.banner_user = Some(value),
                'M' => control.mail = Some(value),
                'N' => source = Some(value),
                'U' => control.unlink.push(value),
                c if FORMATS.contains(c) => {
                    ensure!(
                        value.starts_with("df"),
                        "LPD print line must name a data file"
                    );
                    if !control.prints.iter().any(|(_, name, _)| *name == value) {
                        ensure!(
                            control.prints.len() < MAX_FILES_PER_JOB,
                            "LPD job names too many data files"
                        );
                    }
                    control.prints.push((c, value, source.take()));
                }
                // Fonts, indent, width, symlink data and anything newer: kept out of the event.
                _ => {}
            }
        }
        Ok(control)
    }

    /// Distinct data files the job needs before it is complete.
    pub fn data_files(&self) -> Vec<String> {
        let mut names: Vec<String> = Vec::new();
        for (_, name, _) in &self.prints {
            if !names.contains(name) {
                names.push(name.clone());
            }
        }
        names
    }
}

/// Text of a data file for a handler: the UTF-8 prefix, or null for binary content.
pub fn preview(bytes: &[u8]) -> (Value, bool) {
    let shown = &bytes[..bytes.len().min(MAX_PREVIEW)];
    let text = match std::str::from_utf8(shown) {
        Ok(text) => text,
        Err(e) if e.error_len().is_none() => {
            std::str::from_utf8(&shown[..e.valid_up_to()]).unwrap_or("")
        }
        Err(_) => return (Value::Null, true),
    };
    // Binary when it holds a control character other than line breaks, tabs and form feeds.
    let binary =
        crate::utils::sanitize::has_controls(&text.replace(['\n', '\r', '\t', '\x0c'], ""));
    if binary {
        (Value::Null, true)
    } else {
        (json!(text), false)
    }
}

/// One line of a queue listing, kept free of control characters.
pub fn clean(text: &str, limit: usize) -> String {
    let mut out = String::new();
    for c in crate::utils::sanitize::line_field(text).chars() {
        if out.len() + c.len_utf8() > limit {
            break;
        }
        out.push(c);
    }
    out
}

/// Render a queue listing in the BSD/LPRng shape clients expect.
pub fn render_queue(queue: &str, long: bool, status: Option<&str>, jobs: &[Value]) -> String {
    let mut out = String::new();
    out.push_str(&clean(status.unwrap_or(&format!("{queue} is ready")), 200));
    out.push('\n');
    if jobs.is_empty() {
        out.push_str("no entries\n");
        return out;
    }
    if !long {
        out.push_str("Rank   Owner      Job  Files                                 Total Size\n");
    }
    for (i, job) in jobs.iter().enumerate() {
        let rank = job["rank"].as_str().map(str::to_string).unwrap_or_else(|| {
            if i == 0 {
                "active".into()
            } else {
                format!("{}", i + 1)
            }
        });
        let owner = clean(job["owner"].as_str().unwrap_or("unknown"), 32);
        let id = job["job_id"].as_u64().unwrap_or(i as u64 + 1);
        let files = clean(job["files"].as_str().unwrap_or("(stdin)"), 120);
        let size = job["size"].as_u64().unwrap_or(0);
        if long {
            out.push_str(&format!(
                "\n{owner}: {rank:<28}[job {id:03}]\n        {files:<32}{size} bytes\n"
            ));
        } else {
            out.push_str(&format!(
                "{rank:<7}{owner:<11}{id:<5}{files:<38}{size} bytes\n"
            ));
        }
    }
    out
}
