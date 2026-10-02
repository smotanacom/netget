//! Gemini request encoding, bounded responses and structured gemtext.
use anyhow::{bail, ensure, Context, Result};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::io::{AsyncBufRead, AsyncBufReadExt, AsyncReadExt};
pub const DEADLINE: Duration = Duration::from_secs(30);
pub const MAX_BODY_BYTES: usize = 1024 * 1024;
pub const MAX_LINES: usize = 8192;
pub const MAX_HEADER_BYTES: usize = 1029;
#[derive(Debug)]
pub struct Request {
    pub url: url::Url,
    pub bytes: Vec<u8>,
}
impl Request {
    pub fn from_action(v: &Value) -> Result<Self> {
        let raw = v["url"].as_str().context("Missing Gemini URL")?;
        ensure!(
            !raw.chars().any(char::is_control),
            "URL contains control characters"
        );
        crate::server::gemini::wire::parse_request(raw)
            .map_err(|e| anyhow::anyhow!("Invalid Gemini URL: {e:?}"))?;
        let mut url = url::Url::parse(raw)?;
        if let Some(input) = v.get("input") {
            let input = input.as_str().context("input must be text")?;
            ensure!(input.len() <= 1024, "Input too long");
            let mut query = String::new();
            for b in input.bytes() {
                if b.is_ascii_alphanumeric() || b"-._~".contains(&b) {
                    query.push(b as char)
                } else {
                    query.push_str(&format!("%{b:02X}"));
                }
            }
            url.set_query(Some(&query));
        }
        ensure!(url.as_str().len() <= 1024, "Gemini URL exceeds 1024 bytes");
        ensure!(url.port().unwrap_or(1965) > 0, "Invalid Gemini port");
        Ok(Self {
            bytes: format!("{}\r\n", url.as_str()).into_bytes(),
            url,
        })
    }
}
pub async fn response<R: AsyncBufRead + Unpin>(r: &mut R, request: &Request) -> Result<Value> {
    let mut header = Vec::new();
    loop {
        let buf = r.fill_buf().await?;
        ensure!(!buf.is_empty(), "Truncated Gemini header");
        let n = buf
            .iter()
            .position(|b| *b == b'\n')
            .map(|n| n + 1)
            .unwrap_or(buf.len());
        ensure!(
            header.len() + n <= MAX_HEADER_BYTES,
            "Gemini header exceeds1029 bytes"
        );
        header.extend_from_slice(&buf[..n]);
        r.consume(n);
        if header.ends_with(b"\n") {
            break;
        }
    }
    ensure!(header.ends_with(b"\r\n"), "Gemini requires CRLF");
    header.truncate(header.len() - 2);
    ensure!(
        header.len() >= 3
            && (b'1'..=b'6').contains(&header[0])
            && header[1].is_ascii_digit()
            && header[2] == b' ',
        "Malformed Gemini status"
    );
    let status = (header[0] - b'0') * 10 + header[1] - b'0';
    let meta = std::str::from_utf8(&header[3..])?;
    ensure!(
        !meta.chars().any(char::is_control),
        "Control character in Gemini meta"
    );
    let mut out = json!({"status":status,"meta":meta});
    match status / 10 {
        1 => {
            out["kind"] = json!("input");
            out["sensitive"] = json!(status == 11);
        }
        2 => {
            let media = if meta.is_empty() {
                "text/gemini; charset=utf-8"
            } else {
                meta
            };
            let mime = media
                .split(';')
                .next()
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            ensure!(
                mime.starts_with("text/"),
                "Unsupported non-text Gemini MIME type: {mime}"
            );
            let mut ascii = false;
            for field in media.split(';').skip(1) {
                if let Some((name, value)) = field.split_once('=') {
                    if name.trim().eq_ignore_ascii_case("charset") {
                        let value = value.trim().trim_matches('"');
                        ensure!(
                            value.eq_ignore_ascii_case("utf-8")
                                || value.eq_ignore_ascii_case("us-ascii"),
                            "Unsupported text charset"
                        );
                        ascii = value.eq_ignore_ascii_case("us-ascii");
                    }
                }
            }
            let mut body = Vec::new();
            r.take((MAX_BODY_BYTES + 1) as u64)
                .read_to_end(&mut body)
                .await?;
            ensure!(body.len() <= MAX_BODY_BYTES, "Gemini body exceeds1MiB");
            ensure!(
                !ascii || body.is_ascii(),
                "Non-ASCII body declares us-ascii"
            );
            let text = String::from_utf8(body).context("Gemini text is not UTF-8")?;
            out["kind"] = json!("success");
            out["mime_type"] = json!(mime);
            if mime == "text/gemini" {
                out["lines"] = gemtext(&text, &request.url)?;
            }
            out["text"] = json!(text);
        }
        3 => {
            ensure!(!meta.is_empty(), "Redirect has no target");
            let target = request.url.join(meta).context("Invalid redirect target")?;
            out["kind"] = json!("redirect");
            out["url"] = json!(target.as_str());
            out["permanent"] = json!(status == 31);
        }
        4 => {
            out["kind"] = json!("temporary_failure");
            if status == 44 {
                out["retry_after_secs"] =
                    json!(meta.parse::<u64>().context("Invalid slow-down delay")?);
            }
        }
        5 => out["kind"] = json!("permanent_failure"),
        6 => out["kind"] = json!("certificate_required"),
        _ => bail!("Invalid status class"),
    }
    Ok(out)
}
pub fn gemtext(text: &str, base: &url::Url) -> Result<Value> {
    let mut out = Vec::new();
    let mut pre: Option<(String, Vec<&str>)> = None;
    for (index, line) in text.lines().enumerate() {
        ensure!(index < MAX_LINES, "Too many gemtext lines");
        if let Some(alt) = line.strip_prefix("```") {
            if let Some((alt, lines)) = pre.take() {
                out.push(json!({"type":"preformatted","alt":alt,"text":lines.join("\n")}));
            } else {
                pre = Some((alt.into(), Vec::new()));
            }
            continue;
        }
        if let Some((_, lines)) = &mut pre {
            lines.push(line);
            continue;
        }
        if let Some(link) = line.strip_prefix("=>") {
            let link = link.trim_start();
            let end = link.find(char::is_whitespace).unwrap_or(link.len());
            let raw = &link[..end];
            ensure!(!raw.is_empty(), "Empty gemtext link");
            let target = base.join(raw).context("Invalid gemtext link")?;
            out.push(json!({"type":"link","url":target.as_str(),"text":link[end..].trim()}));
        } else if let Some(text) = line.strip_prefix("###") {
            out.push(json!({"type":"heading3","text":text.trim_start()}));
        } else if let Some(text) = line.strip_prefix("##") {
            out.push(json!({"type":"heading2","text":text.trim_start()}));
        } else if let Some(text) = line.strip_prefix('#') {
            out.push(json!({"type":"heading1","text":text.trim_start()}));
        } else if let Some(text) = line.strip_prefix("* ") {
            out.push(json!({"type":"list","text":text}));
        } else if let Some(text) = line.strip_prefix('>') {
            out.push(json!({"type":"quote","text":text.trim_start()}));
        } else {
            out.push(json!({"type":"text","text":line}));
        }
    }
    if let Some((alt, lines)) = pre {
        out.push(json!({"type":"preformatted","alt":alt,"text":lines.join("\n")}));
    }
    Ok(json!(out))
}
