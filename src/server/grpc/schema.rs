//! Bounded startup descriptors and a cancellable, deadline-owned protoc child.
use anyhow::{ensure, Context, Result};
use base64::Engine;
use prost::Message;
use prost_reflect::DescriptorPool;
use std::{path::Path, process::Stdio, time::Duration};
use tokio::io::AsyncReadExt;

const MAX_BYTES: usize = 4 * 1024 * 1024;
const MAX_BASE64_BYTES: usize = MAX_BYTES.div_ceil(3) * 4;
const MAX_DIAGNOSTIC: usize = 16 * 1024;
const COMPILE_TIMEOUT: Duration = Duration::from_secs(10);

fn decode(bytes: &[u8]) -> Result<DescriptorPool> {
    ensure!(bytes.len() <= MAX_BYTES, "schema exceeds 4 MiB");
    let set = prost_types::FileDescriptorSet::decode(bytes)?;
    pool_from_set(set)
}
/// Check qualified-name expansion before prost-reflect materializes symbols.
pub(crate) fn pool_from_set(set: prost_types::FileDescriptorSet) -> Result<DescriptorPool> {
    ensure!(
        set.encoded_len() <= MAX_BYTES && set.file.len() <= 128,
        "schema exceeds 4 MiB/128 files"
    );
    let mut symbols = 0usize;
    let mut bytes = 0usize;
    let mut name = |parent: &str, short: &str| -> Result<std::sync::Arc<str>> {
        ensure!(
            !short.is_empty() && short.len() <= 256,
            "schema identifier exceeds 256 bytes"
        );
        ensure!(
            parent.len() + short.len() + 1 <= 1024,
            "qualified schema name exceeds 1024 bytes"
        );
        symbols = symbols.checked_add(1).context("symbol count overflow")?;
        bytes = bytes
            .checked_add(parent.len() + short.len() + 1)
            .context("symbol size overflow")?;
        ensure!(
            symbols <= 100_000 && bytes <= 8 * 1024 * 1024,
            "schema symbol expansion exceeds 100000 nodes/8 MiB"
        );
        Ok(if parent.is_empty() {
            short.into()
        } else {
            format!("{parent}.{short}").into()
        })
    };
    for file in &set.file {
        ensure!(
            file.name().len() <= 4096
                && file.package().len() <= 256
                && file.dependency.iter().all(|path| path.len() <= 4096),
            "schema filename/package too long"
        );
        let prefix: std::sync::Arc<str> = file.package().into();
        let mut pending: Vec<_> = file
            .message_type
            .iter()
            .map(|message| (message, prefix.clone(), 0usize))
            .collect();
        while let Some((message, parent, depth)) = pending.pop() {
            ensure!(depth <= 32, "schema nesting exceeds 32 levels");
            let qualified = name(&parent, message.name())?;
            for field in message.field.iter().chain(&message.extension) {
                name(&qualified, field.name())?;
                ensure!(
                    field.type_name().len() <= 1024 && field.extendee().len() <= 1024,
                    "schema field type name too long"
                );
            }
            for oneof in &message.oneof_decl {
                name(&qualified, oneof.name())?;
            }
            for enumeration in &message.enum_type {
                let enumeration_name = name(&qualified, enumeration.name())?;
                for value in &enumeration.value {
                    name(&enumeration_name, value.name())?;
                }
            }
            pending.extend(
                message
                    .nested_type
                    .iter()
                    .map(|message| (message, qualified.clone(), depth + 1)),
            );
        }
        for enumeration in &file.enum_type {
            let qualified = name(&prefix, enumeration.name())?;
            for value in &enumeration.value {
                name(&qualified, value.name())?;
            }
        }
        for extension in &file.extension {
            name(&prefix, extension.name())?;
            ensure!(
                extension.type_name().len() <= 1024 && extension.extendee().len() <= 1024,
                "schema field type name too long"
            );
        }
        for service in &file.service {
            let qualified = name(&prefix, service.name())?;
            for method in &service.method {
                name(&qualified, method.name())?;
                ensure!(
                    method.input_type().len() <= 1024 && method.output_type().len() <= 1024,
                    "method type name too long"
                );
            }
        }
    }
    let pool = DescriptorPool::from_file_descriptor_set(set)?;
    ensure!(pool.services().len() <= 128, "schema exceeds 128 services");
    ensure!(
        pool.services()
            .all(|service| service.full_name().len() <= 1024),
        "service name exceeds 1024 bytes"
    );
    Ok(pool)
}
async fn read(path: &Path) -> Result<Vec<u8>> {
    let metadata = tokio::fs::metadata(path).await?;
    ensure!(
        metadata.is_file() && metadata.len() <= u64::try_from(MAX_BYTES)?,
        "schema file must be regular and at most 4 MiB"
    );
    let mut options = tokio::fs::OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    options.custom_flags(libc::O_NONBLOCK);
    let file = options.open(path).await?;
    let metadata = file.metadata().await?;
    ensure!(
        metadata.is_file() && metadata.len() <= u64::try_from(MAX_BYTES)?,
        "schema file must be regular and at most 4 MiB"
    );
    let mut bytes = Vec::new();
    file.take(u64::try_from(MAX_BYTES + 1)?)
        .read_to_end(&mut bytes)
        .await?;
    ensure!(bytes.len() <= MAX_BYTES, "schema file exceeds 4 MiB");
    Ok(bytes)
}
async fn compile(path: &Path) -> Result<DescriptorPool> {
    let output = tempfile::tempdir()?;
    let destination = output.path().join("descriptor.pb");
    let path = tokio::fs::canonicalize(path).await?;
    let parent = path.parent().context("schema has no parent directory")?;
    let filename = path.file_name().context("schema has no filename")?;
    tokio::time::timeout(COMPILE_TIMEOUT, async {
        let mut child = tokio::process::Command::new("protoc")
            .current_dir(parent)
            .arg("--include_imports")
            .arg("--include_source_info")
            .arg("--descriptor_set_out")
            .arg(&destination)
            .arg("--proto_path=.")
            .arg(filename)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .context("protoc must be installed to compile .proto schemas")?;
        let stderr = child.stderr.take().context("protoc stderr unavailable")?;
        let mut diagnostic = Vec::new();
        let mut stderr = stderr.take(u64::try_from(MAX_DIAGNOSTIC + 1)?);
        let status = {
            let drain = stderr.read_to_end(&mut diagnostic);
            tokio::pin!(drain);
            tokio::select! {
            status = child.wait() => { let status = status?; drain.await?; status }
            result = &mut drain => {
                let bytes = result?;
                if bytes > MAX_DIAGNOSTIC {
                    child.start_kill()?;
                    let _ = child.wait().await;
                    anyhow::bail!("protoc exceeded 16 KiB diagnostics");
                }
                child.wait().await?
            }
            }
        };
        ensure!(
            diagnostic.len() <= MAX_DIAGNOSTIC,
            "protoc exceeded 16 KiB diagnostics"
        );
        ensure!(
            status.success(),
            "protoc rejected schema: {}",
            String::from_utf8_lossy(&diagnostic)
        );
        decode(&read(&destination).await?)
    })
    .await
    .context("protoc compilation exceeded 10 seconds")?
}
pub(crate) async fn load(input: &str) -> Result<DescriptorPool> {
    let input = input.trim();
    ensure!(input.len() <= MAX_BASE64_BYTES, "schema input too large");
    if let Ok(bytes) = base64::engine::general_purpose::STANDARD.decode(input) {
        return decode(&bytes).context("base64 schema is not a valid FileDescriptorSet");
    }
    if input.ends_with(".pb") || input.ends_with(".proto") {
        ensure!(input.len() <= 4096, "schema path exceeds 4096 bytes");
        let path = Path::new(input);
        if input.ends_with(".pb") {
            return decode(&read(path).await?);
        }
        // Validate the actual source before starting the compiler; imports are
        // admitted only through a descriptor output bounded before allocation.
        let _ = read(path).await?;
        return compile(path).await;
    }
    ensure!(input.len() <= MAX_BYTES, "inline proto exceeds 4 MiB");
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("schema.proto");
    tokio::fs::write(&path, input).await?;
    compile(&path).await
}
