//! Bounded handler-supplied file content, compressed lists and Tiger Tree Hashes.
use anyhow::{ensure, Context, Result};
use base64::{engine::general_purpose::STANDARD, Engine};
use serde_json::{json, Value};
pub const MAX_FILE: usize = 1024 * 1024;
pub fn content(request: &Value, action: Option<&Value>) -> Result<Vec<u8>> {
    let a = action.context("no transfer decision")?;
    ensure!(a.get("error").is_none(), "transfer refused");
    let raw = if let Some(xml) = a["file_list_xml"].as_str() {
        ensure!(xml.len() <= MAX_FILE, "file list too large");
        if request["identifier"] == "files.xml.bz2" {
            use std::io::Write;
            let mut e = bzip2::write::BzEncoder::new(Vec::new(), bzip2::Compression::default());
            e.write_all(xml.as_bytes())?;
            e.finish()?
        } else {
            xml.as_bytes().to_vec()
        }
    } else {
        let data = a["data_base64"].as_str().context("data_base64 required")?;
        ensure!(
            data.len() <= MAX_FILE.div_ceil(3) * 4,
            "encoded transfer too large"
        );
        STANDARD.decode(data)?
    };
    ensure!(raw.len() <= MAX_FILE, "transfer too large");
    let start = usize::try_from(request["offset"].as_u64().context("offset")?)?;
    ensure!(start <= raw.len(), "offset outside source");
    let requested = request["length"].as_i64().context("length")?;
    let len = if requested == -1 {
        raw.len() - start
    } else {
        usize::try_from(requested)?
    };
    ensure!(len <= raw.len() - start, "range outside source");
    Ok(raw[start..start + len].to_vec())
}
pub fn tth(data: &[u8]) -> String {
    use tiger::{Digest, Tiger};
    let leaf = |chunk: &[u8]| {
        let mut h = Tiger::new();
        h.update([0]);
        h.update(chunk);
        h.finalize().to_vec()
    };
    let mut nodes: Vec<Vec<u8>> = if data.is_empty() {
        vec![leaf(&[])]
    } else {
        data.chunks(1024).map(leaf).collect()
    };
    while nodes.len() > 1 {
        nodes = nodes
            .chunks(2)
            .map(|pair| {
                if pair.len() == 1 {
                    pair[0].clone()
                } else {
                    let mut h = Tiger::new();
                    h.update([1]);
                    h.update(&pair[0]);
                    h.update(&pair[1]);
                    h.finalize().to_vec()
                }
            })
            .collect();
    }
    crate::server::p2p_support::base32(&nodes[0])
}
pub fn decoded_result(identifier: &str, data: Vec<u8>, expected: Option<&str>) -> Result<Value> {
    if let Some(hash) = expected {
        ensure!(tth(&data) == hash, "Tiger tree hash mismatch");
    }
    let mut result = json!({"identifier":identifier,"length":data.len(),"data_base64":STANDARD.encode(&data),"tth":tth(&data)});
    if identifier == "files.xml.bz2" {
        use std::io::Read;
        let mut text = String::new();
        bzip2::read::BzDecoder::new(&data[..])
            .take((MAX_FILE + 1) as u64)
            .read_to_string(&mut text)?;
        ensure!(text.len() <= MAX_FILE, "decompressed file list too large");
        result["file_list_xml"] = json!(text);
    } else if identifier == "files.xml" {
        result["file_list_xml"] = json!(String::from_utf8(data)?);
    }
    Ok(result)
}
