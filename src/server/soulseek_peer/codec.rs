//! Soulseek P-connection browsing; compressed content is bounded after decompression.
use crate::server::{
    p2p_support::{DeviceSession, Framer, ReadStream, ScannerSession, Stream},
    soulseek::codec::{string, Reader},
};
use anyhow::{bail, ensure, Context, Result};
use async_trait::async_trait;
use serde_json::{json, Value};
use std::io::{Read, Write};
use tokio::io::AsyncWriteExt;
pub const MAX_COMMAND: usize = 1024 * 1024;
fn length(b: &[u8]) -> Result<usize> {
    let n = u32::from_le_bytes(b.try_into()?) as usize;
    ensure!((1..=MAX_COMMAND - 4).contains(&n), "peer frame length");
    Ok(n + 4)
}
fn packet(code: u32, data: Vec<u8>) -> Result<Vec<u8>> {
    ensure!(data.len() <= MAX_COMMAND - 8, "peer packet too large");
    let mut b = ((data.len() + 4) as u32).to_le_bytes().to_vec();
    b.extend(code.to_le_bytes());
    b.extend(data);
    Ok(b)
}
pub fn validate(v: &Value) -> Result<()> {
    ensure!(
        matches!(
            v["type"].as_str(),
            Some("soulseek_peer_shares" | "soulseek_peer_info")
        ),
        "unknown peer action"
    );
    Ok(())
}
fn directories(out: &mut Vec<u8>, v: &Value) -> Result<()> {
    let dirs = v.as_array().cloned().unwrap_or_default();
    ensure!(dirs.len() <= 128, "directory count");
    out.extend((dirs.len() as u32).to_le_bytes());
    for d in dirs {
        string(out, d["name"].as_str().context("directory name")?)?;
        let files = d["files"].as_array().context("files")?;
        ensure!(files.len() <= 128, "file count");
        out.extend((files.len() as u32).to_le_bytes());
        for f in files {
            out.push(1);
            string(out, f["name"].as_str().context("filename")?)?;
            out.extend(f["size"].as_u64().context("size")?.to_le_bytes());
            string(out, f["extension"].as_str().unwrap_or(""))?;
            out.extend(0u32.to_le_bytes());
            ensure!(out.len() <= MAX_COMMAND, "listing too large");
        }
    }
    Ok(())
}
fn read_dirs(r: &mut Reader<'_>) -> Result<Value> {
    let count = r.u32()?;
    ensure!(count <= 128, "directory count");
    let mut dirs = vec![];
    for _ in 0..count {
        let name = r.string()?;
        let n = r.u32()?;
        ensure!(n <= 128, "file count");
        let mut files = vec![];
        for _ in 0..n {
            ensure!(r.take(1)?[0] == 1, "file marker");
            let name = r.string()?;
            let size = r.u64()?;
            let extension = r.string()?;
            let n = r.u32()?;
            ensure!(n <= 32, "attribute count");
            let mut attributes = vec![];
            for _ in 0..n {
                attributes.push(json!({"id":r.u32()?,"value":r.u32()?}));
            }
            files.push(
                json!({"name":name,"size":size,"extension":extension,"attributes":attributes}),
            );
        }
        dirs.push(json!({"name":name,"files":files}));
    }
    Ok(json!(dirs))
}
#[derive(Default)]
pub struct Device {
    frame: Framer,
    initialized: bool,
}
#[async_trait]
impl DeviceSession for Device {
    async fn read(&mut self, s: &mut ReadStream) -> Result<Vec<u8>> {
        self.frame.sized(s, 4, MAX_COMMAND, length).await
    }
    fn receive(&mut self, b: &[u8]) -> Result<(Vec<u8>, Option<Value>)> {
        let mut r = Reader::new(&b[4..]);
        if !self.initialized {
            ensure!(r.take(1)?[0] == 1, "PeerInit required");
            let name = r.string()?;
            ensure!(!name.is_empty(), "peer username");
            ensure!(r.string()? == "P", "selected P connection only");
            r.u32()?;
            if r.pos < r.bytes.len() {
                r.u32()?;
            }
            r.end()?;
            self.initialized = true;
            return Ok((vec![], None));
        }
        let code = r.u32()?;
        match code {
            4 => {
                if r.pos < r.bytes.len() {
                    r.u32()?;
                }
            }
            15 => {}
            _ => return Ok((vec![], None)),
        }
        r.end()?;
        Ok((
            vec![],
            Some(json!({"operation":if code==4{"shares"}else{"info"},"code":code})),
        ))
    }
    fn answer(&mut self, r: &Value, a: Option<&Value>) -> Result<Vec<u8>> {
        let empty = json!({});
        let a = a.unwrap_or(&empty);
        let mut out = vec![];
        match r["code"].as_u64().context("code")? {
            4 => {
                directories(&mut out, &a["directories"])?;
                out.extend([0; 8]);
                ensure!(out.len() <= MAX_COMMAND, "decoded listing cap");
                let mut enc =
                    flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
                enc.write_all(&out)?;
                packet(5, enc.finish()?)
            }
            15 => {
                string(&mut out, a["description"].as_str().unwrap_or(""))?;
                out.push(0);
                for key in ["upload_slots", "queue_size"] {
                    let n = a[key].as_u64().unwrap_or(0);
                    ensure!(n <= u32::MAX as u64, "{key}");
                    out.extend((n as u32).to_le_bytes());
                }
                out.push(u8::from(a["has_slots_free"] == true));
                out.extend(0u32.to_le_bytes());
                packet(16, out)
            }
            _ => bail!("unknown peer reply"),
        }
    }
}
fn response(b: &[u8]) -> Result<Value> {
    let mut r = Reader::new(&b[4..]);
    let code = r.u32()?;
    match code {
        5 => {
            let mut out = vec![];
            flate2::read::ZlibDecoder::new(&r.bytes[r.pos..])
                .take((MAX_COMMAND + 1) as u64)
                .read_to_end(&mut out)?;
            ensure!(out.len() <= MAX_COMMAND, "decompression cap");
            let mut r = Reader::new(&out);
            let dirs = read_dirs(&mut r)?;
            if r.pos < r.bytes.len() {
                r.u32()?;
            }
            let locked = if r.pos < r.bytes.len() {
                read_dirs(&mut r)?
            } else {
                json!([])
            };
            r.end()?;
            Ok(json!({"code":code,"directories":dirs,"locked_directories":locked}))
        }
        16 => {
            let description = r.string()?;
            let picture = r.boolean()?;
            ensure!(!picture, "pictures outside selected scope");
            let slots = r.u32()?;
            let queue = r.u32()?;
            let free = r.boolean()?;
            let permissions = if r.pos < r.bytes.len() { r.u32()? } else { 0 };
            ensure!(permissions <= 3, "permissions");
            r.end()?;
            Ok(
                json!({"code":code,"description":description,"upload_slots":slots,"queue_size":queue,"has_slots_free":free,"upload_permissions":permissions}),
            )
        }
        _ => bail!("unsupported peer reply"),
    }
}
#[derive(Default)]
pub struct Scanner {
    frame: Framer,
}
#[async_trait]
impl ScannerSession for Scanner {
    async fn open(&mut self, s: &mut Stream) -> Result<()> {
        let mut init = vec![1];
        string(&mut init, "NetGet")?;
        string(&mut init, "P")?;
        init.extend(0u32.to_le_bytes());
        let mut b = (init.len() as u32).to_le_bytes().to_vec();
        b.extend(init);
        s.write_all(&b).await?;
        Ok(())
    }
    async fn exchange(&mut self, s: &mut Stream, a: &Value) -> Result<Value> {
        validate(a)?;
        let code = if a["type"] == "soulseek_peer_shares" {
            4
        } else {
            15
        };
        s.write_all(&packet(code, vec![])?).await?;
        let v = response(&self.frame.sized(s, 4, MAX_COMMAND, length).await?)?;
        ensure!(
            v["code"] == if code == 4 { 5 } else { 16 },
            "mismatched peer reply"
        );
        Ok(v)
    }
    async fn idle(&mut self, s: &mut Stream) -> Result<Option<Value>> {
        Ok(Some(response(
            &self.frame.sized(s, 4, MAX_COMMAND, length).await?,
        )?))
    }
}
