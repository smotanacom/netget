//! Bounded file I/O on native hosts; browser builds return explicit errors.
#[cfg(not(target_arch = "wasm32"))]
mod native {
    use anyhow::{bail, Context, Result};
    use std::fs::{File, OpenOptions};
    use std::io::{Read, Write};
    use std::path::{Path, PathBuf};

    pub fn read_regular_file(path: &Path, limit: usize) -> Result<Vec<u8>> {
        let mut options = OpenOptions::new();
        options.read(true);
        // Do not block opening a FIFO if the path is replaced after a caller's
        // preliminary metadata check. Validate the opened descriptor itself.
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.custom_flags(libc::O_NONBLOCK);
        }
        let file = options
            .open(path)
            .with_context(|| format!("open {}", path.display()))?;
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            bail!("{} is not a regular file", path.display());
        }
        if metadata.len() > limit as u64 {
            bail!("{} exceeds {limit} bytes", path.display());
        }
        let mut bytes = Vec::new();
        file.take((limit as u64).saturating_add(1))
            .read_to_end(&mut bytes)?;
        if bytes.len() > limit {
            bail!("{} grew beyond {limit} bytes", path.display());
        }
        Ok(bytes)
    }

    pub fn read_text(path: &Path, limit: usize) -> Result<String> {
        String::from_utf8(read_regular_file(path, limit)?).context("file is not UTF-8")
    }

    struct RemoveOnDrop(PathBuf);
    impl Drop for RemoveOnDrop {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    /// Complete the new contents before replacing the destination. A failure before
    /// rename leaves the old file intact, and temporary files have private permissions.
    pub fn write_atomic(path: &Path, contents: &[u8]) -> Result<()> {
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let temporary = parent.join(format!(".netget-write-{}", uuid::Uuid::new_v4()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = options
            .open(&temporary)
            .context("create atomic-write staging file")?;
        let _cleanup = RemoveOnDrop(temporary.clone());
        file.write_all(contents)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temporary, path).with_context(|| format!("replace {}", path.display()))?;
        #[cfg(unix)]
        File::open(parent)?.sync_all()?;
        Ok(())
    }

    /// Convert the legacy ~/.netget JSON file into ~/.netget/settings.json before
    /// writing either settings or config.toml. Keep an exact backup; serialize with
    /// an OS file lock and roll the old path back if installing the directory fails.
    pub fn ensure_config_directory(home: &Path) -> Result<PathBuf> {
        use fs2::FileExt;
        let path = home.join(".netget");
        if path.is_dir() {
            return Ok(path);
        }
        let mut lock_options = OpenOptions::new();
        lock_options
            .read(true)
            .write(true)
            .create(true)
            .truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            lock_options.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        let lock = lock_options.open(home.join(".netget-migration.lock"))?;
        if !lock.metadata()?.is_file() {
            bail!("configuration migration lock is not a regular file");
        }
        FileExt::lock_exclusive(&lock)?;
        if path.is_dir() {
            return Ok(path);
        }
        let recovery_backup = home.join(".netget-legacy.json");
        match std::fs::symlink_metadata(&path) {
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                let stage = home.join(format!(".netget-migration-{}", uuid::Uuid::new_v4()));
                std::fs::create_dir(&stage)?;
                #[cfg(unix)]
                {
                    use std::os::unix::fs::PermissionsExt;
                    std::fs::set_permissions(&stage, std::fs::Permissions::from_mode(0o700))?;
                }
                let install = (|| -> Result<()> {
                    if recovery_backup.is_file() {
                        let bytes = read_regular_file(&recovery_backup, 1024 * 1024)?;
                        let value: serde_json::Value = serde_json::from_slice(&bytes)?;
                        if !value.is_object() {
                            bail!("legacy recovery settings are not an object");
                        }
                        write_atomic(&stage.join("settings.json"), &bytes)?;
                    }
                    std::fs::rename(&stage, &path)?;
                    #[cfg(unix)]
                    File::open(home)?.sync_all()?;
                    Ok(())
                })();
                if stage.exists() {
                    let _ = std::fs::remove_dir_all(&stage);
                }
                install?;
                return Ok(path);
            }
            Ok(meta) if meta.file_type().is_file() => {}
            Ok(_) => bail!(
                "legacy configuration path is not a regular file: {}",
                path.display()
            ),
            Err(e) => return Err(e.into()),
        }
        let original = read_regular_file(&path, 1024 * 1024)?;
        // Refuse to reinterpret or move unrelated/malformed data.
        let value: serde_json::Value =
            serde_json::from_slice(&original).context("legacy settings are not valid JSON")?;
        if !value.is_object() {
            bail!("legacy settings must be a JSON object");
        }
        let unique = uuid::Uuid::new_v4();
        let stage = home.join(format!(".netget-migration-{unique}"));
        let backup = recovery_backup;
        std::fs::create_dir(&stage)?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(&stage, std::fs::Permissions::from_mode(0o700))?;
        }
        let result = (|| -> Result<()> {
            write_atomic(&stage.join("settings.json"), &original)?;
            // A hard link fails rather than overwriting an existing backup.
            if backup.exists() {
                if read_regular_file(&backup, 1024 * 1024)? != original {
                    bail!(
                        "legacy backup already exists with different contents: {}",
                        backup.display()
                    );
                }
            } else {
                std::fs::hard_link(stage.join("settings.json"), &backup)
                    .context("preserve legacy settings backup")?;
            }
            #[cfg(unix)]
            File::open(home)?.sync_all()?;
            std::fs::remove_file(&path)?;
            if let Err(error) = std::fs::rename(&stage, &path) {
                // create_new behavior via hard_link refuses to overwrite a competing
                // path; the backup remains available even if rollback cannot install.
                let rollback = std::fs::hard_link(&backup, &path);
                bail!("install configuration directory failed: {error}; rollback: {rollback:?}; backup: {}", backup.display());
            }
            #[cfg(unix)]
            File::open(home)?.sync_all()?;
            Ok(())
        })();
        if stage.exists() {
            let _ = std::fs::remove_dir_all(&stage);
        }
        result?;
        Ok(path)
    }
}
#[cfg(not(target_arch = "wasm32"))]
pub use native::*;

#[cfg(target_arch = "wasm32")]
pub fn read_regular_file(_path: &std::path::Path, _limit: usize) -> anyhow::Result<Vec<u8>> {
    anyhow::bail!("local filesystem is unavailable in the browser")
}
#[cfg(target_arch = "wasm32")]
pub fn read_text(_path: &std::path::Path, _limit: usize) -> anyhow::Result<String> {
    anyhow::bail!("local filesystem is unavailable in the browser")
}
#[cfg(target_arch = "wasm32")]
pub fn write_atomic(_path: &std::path::Path, _contents: &[u8]) -> anyhow::Result<()> {
    anyhow::bail!("local filesystem is unavailable in the browser")
}
#[cfg(target_arch = "wasm32")]
pub fn ensure_config_directory(_home: &std::path::Path) -> anyhow::Result<std::path::PathBuf> {
    anyhow::bail!("local filesystem is unavailable in the browser")
}

pub async fn read_text_async(path: &std::path::Path, limit: usize) -> anyhow::Result<String> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let path = path.to_owned();
        tokio::time::timeout(
            std::time::Duration::from_secs(30),
            tokio::task::spawn_blocking(move || read_text(&path, limit)),
        )
        .await
        .map_err(|_| anyhow::anyhow!("file read timed out after 30 seconds"))??
    }
    #[cfg(target_arch = "wasm32")]
    {
        read_text(path, limit)
    }
}

pub async fn write_atomic_async(path: &std::path::Path, bytes: Vec<u8>) -> anyhow::Result<()> {
    #[cfg(not(target_arch = "wasm32"))]
    {
        let path = path.to_owned();
        // Once started, finish the atomic replace even if the caller is cancelled.
        tokio::task::spawn_blocking(move || write_atomic(&path, &bytes)).await?
    }
    #[cfg(target_arch = "wasm32")]
    {
        write_atomic(path, &bytes)
    }
}
