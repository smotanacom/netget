//! Typed Gearman binary requests and a continuous bounded response reader.
use crate::server::gearman::wire as gear;
use anyhow::{bail, Context, Result};
use serde_json::Value;
use std::time::Duration;
use tokio::io::{AsyncRead, AsyncReadExt};
pub const DEADLINE: Duration = Duration::from_secs(15);
pub const MAX_FOLLOWUPS: u8 = 4;
pub const MAX_JOBS: usize = 64;
pub const MAX_ABILITIES: usize = 64;
pub const NOOP: u32 = 6;
pub const NO_JOB: u32 = 10;
pub const JOB_ASSIGN: u32 = 11;
pub const JOB_ASSIGN_UNIQ: u32 = 31;
pub const DEFAULT_ROLE: &str = "submitter";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Role {
    Submitter,
    Worker,
}
impl Role {
    pub fn parse(role: &str) -> Result<Self> {
        match role {
            "submitter" => Ok(Self::Submitter),
            "worker" => Ok(Self::Worker),
            _ => bail!("Gearman role must be submitter or worker"),
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Self::Submitter => "submitter",
            Self::Worker => "worker",
        }
    }
}
#[derive(Debug)]
pub struct Request {
    pub packet_type: u32,
    pub args: Vec<Vec<u8>>,
}
impl Request {
    pub fn bytes(&self) -> Vec<u8> {
        gear::encode(
            true,
            self.packet_type,
            &self.args.iter().map(Vec::as_slice).collect::<Vec<_>>(),
        )
    }
    pub fn expects_reply(&self) -> bool {
        matches!(
            self.packet_type,
            gear::SUBMIT_JOB
                | gear::SUBMIT_JOB_HIGH
                | gear::SUBMIT_JOB_LOW
                | gear::SUBMIT_JOB_BG
                | gear::SUBMIT_JOB_HIGH_BG
                | gear::SUBMIT_JOB_LOW_BG
                | gear::GET_STATUS
                | gear::ECHO_REQ
                | gear::OPTION_REQ
                | gear::GRAB_JOB
                | gear::GRAB_JOB_UNIQ
        )
    }
    pub fn worker_only(&self) -> bool {
        gear::is_worker_packet(self.packet_type) || self.packet_type == gear::SET_CLIENT_ID
    }
    pub fn submit(&self) -> bool {
        matches!(
            self.packet_type,
            gear::SUBMIT_JOB
                | gear::SUBMIT_JOB_HIGH
                | gear::SUBMIT_JOB_LOW
                | gear::SUBMIT_JOB_BG
                | gear::SUBMIT_JOB_HIGH_BG
                | gear::SUBMIT_JOB_LOW_BG
        )
    }
    pub fn background(&self) -> bool {
        matches!(
            self.packet_type,
            gear::SUBMIT_JOB_BG | gear::SUBMIT_JOB_HIGH_BG | gear::SUBMIT_JOB_LOW_BG
        )
    }
}
fn text<'a>(v: &'a Value, key: &str) -> Result<&'a str> {
    v[key]
        .as_str()
        .with_context(|| format!("Gearman {key} must be a string"))
}
fn field(s: &str, key: &str, min: usize, max: usize) -> Result<Vec<u8>> {
    anyhow::ensure!(
        (min..=max).contains(&s.len()) && !s.contains('\0'),
        "Gearman {key} must have {min}..={max} UTF-8 bytes without NUL"
    );
    Ok(s.as_bytes().to_vec())
}
pub fn handle(bytes: &[u8]) -> Result<String> {
    anyhow::ensure!(
        !bytes.is_empty() && bytes.len() < 64 && bytes.iter().all(|b| b.is_ascii_graphic()),
        "Gearman job handle must have 1..63 printable ASCII bytes"
    );
    Ok(std::str::from_utf8(bytes)?.into())
}
fn job(v: &Value) -> Result<Vec<u8>> {
    let h = text(v, "job_handle")?;
    handle(h.as_bytes())?;
    Ok(h.as_bytes().to_vec())
}
fn function(v: &Value) -> Result<Vec<u8>> {
    field(
        text(v, "function_name")?,
        "function",
        1,
        gear::MAX_FUNCTION_NAME,
    )
}
pub fn request(v: &Value) -> Result<Request> {
    let operation = text(v, "operation")?;
    let (packet_type, args) = match v["type"].as_str() {
        Some("gearman_request") => match operation {
            "submit" => {
                let priority = match v.get("priority") {
                    None => "normal",
                    Some(p) => p.as_str().context("Gearman priority must be a string")?,
                };
                let background = match v.get("background") {
                    None => false,
                    Some(b) => b.as_bool().context("Gearman background must be boolean")?,
                };
                let t = match (priority, background) {
                    ("normal", false) => gear::SUBMIT_JOB,
                    ("normal", true) => gear::SUBMIT_JOB_BG,
                    ("high", false) => gear::SUBMIT_JOB_HIGH,
                    ("high", true) => gear::SUBMIT_JOB_HIGH_BG,
                    ("low", false) => gear::SUBMIT_JOB_LOW,
                    ("low", true) => gear::SUBMIT_JOB_LOW_BG,
                    _ => bail!("Gearman priority must be normal, high or low"),
                };
                let unique = match v.get("unique_id") {
                    None => "",
                    Some(s) => s.as_str().context("Gearman unique_id must be a string")?,
                };
                (
                    t,
                    vec![
                        function(v)?,
                        field(unique, "unique_id", 0, gear::MAX_UNIQUE)?,
                        text(v, "workload")?.as_bytes().to_vec(),
                    ],
                )
            }
            "status" => (gear::GET_STATUS, vec![job(v)?]),
            "echo" => (gear::ECHO_REQ, vec![text(v, "data")?.as_bytes().to_vec()]),
            "enable_exceptions" => (gear::OPTION_REQ, vec![b"exceptions".to_vec()]),
            _ => bail!("Unknown Gearman submitter operation"),
        },
        Some("gearman_worker") => match operation {
            "register" => (gear::CAN_DO, vec![function(v)?]),
            "unregister" => (gear::CANT_DO, vec![function(v)?]),
            "reset" => (gear::RESET_ABILITIES, vec![]),
            "grab" => (gear::GRAB_JOB, vec![]),
            "grab_unique" => (gear::GRAB_JOB_UNIQ, vec![]),
            "sleep" => (gear::PRE_SLEEP, vec![]),
            "set_id" => (
                gear::SET_CLIENT_ID,
                vec![field(text(v, "client_id")?, "client_id", 1, 64)?],
            ),
            "progress" => {
                let n = v["numerator"]
                    .as_u64()
                    .context("Gearman numerator must be a nonnegative integer")?;
                let d = v["denominator"]
                    .as_u64()
                    .context("Gearman denominator must be a positive integer")?;
                anyhow::ensure!(d>0&&n<=d&&d<=u32::MAX as u64,"Gearman progress must satisfy 0 <= numerator <= denominator <= u32::MAX and denominator > 0");
                (
                    gear::WORK_STATUS,
                    vec![
                        job(v)?,
                        n.to_string().into_bytes(),
                        d.to_string().into_bytes(),
                    ],
                )
            }
            "data" => (
                gear::WORK_DATA,
                vec![job(v)?, text(v, "data")?.as_bytes().to_vec()],
            ),
            "warning" => (
                gear::WORK_WARNING,
                vec![job(v)?, text(v, "data")?.as_bytes().to_vec()],
            ),
            "complete" => (
                gear::WORK_COMPLETE,
                vec![job(v)?, text(v, "result")?.as_bytes().to_vec()],
            ),
            "fail" => (gear::WORK_FAIL, vec![job(v)?]),
            "exception" => (
                gear::WORK_EXCEPTION,
                vec![job(v)?, text(v, "text")?.as_bytes().to_vec()],
            ),
            _ => bail!("Unknown Gearman worker operation"),
        },
        _ => bail!("Unknown Gearman client action"),
    };
    let size = args.iter().map(Vec::len).sum::<usize>() + args.len().saturating_sub(1);
    anyhow::ensure!(
        size <= gear::MAX_PACKET_BYTES,
        "Gearman packet body exceeds 1 MiB"
    );
    Ok(Request { packet_type, args })
}
#[derive(Debug)]
pub struct Frame {
    pub packet_type: u32,
    pub data: Vec<u8>,
}
/// A dedicated reader owns each complete frame. Idle workers may wait indefinitely;
/// partial headers/bodies get a deadline and the size is checked before allocation.
pub async fn frame<R: AsyncRead + Unpin>(r: &mut R) -> Result<Option<Frame>> {
    let mut header = [0; gear::HEADER_LEN];
    if r.read(&mut header[..1]).await? == 0 {
        return Ok(None);
    }
    tokio::time::timeout(DEADLINE, async {
        r.read_exact(&mut header[1..]).await?;
        let h = gear::parse_header(&header)
            .map_err(|e| anyhow::anyhow!("Invalid Gearman response header: {e:?}"))?;
        anyhow::ensure!(!h.request, "Gearman peer sent request magic");
        let mut data = vec![0; h.size as usize];
        r.read_exact(&mut data).await?;
        Ok(Some(Frame {
            packet_type: h.packet_type,
            data,
        }))
    })
    .await
    .context("Gearman partial frame deadline")?
}
pub fn args(data: &[u8], n: usize) -> Result<Vec<&[u8]>> {
    gear::split_args(data, n).context("Malformed Gearman response arguments")
}
pub fn number(data: &[u8]) -> Result<u64> {
    anyhow::ensure!(
        !data.is_empty() && data.len() <= 10 && data.iter().all(u8::is_ascii_digit),
        "Invalid Gearman response integer"
    );
    let n = std::str::from_utf8(data)?.parse::<u64>()?;
    anyhow::ensure!(n <= u32::MAX as u64, "Gearman response integer exceeds u32");
    Ok(n)
}
pub fn body(data: &[u8]) -> Value {
    serde_json::json!({"text":std::str::from_utf8(data).ok(),"bytes":data.len(),"utf8":std::str::from_utf8(data).is_ok()})
}
