//! The one directory a media client may read a local file from.
//!
//! `srt_send_file` and `rtmp_publish` take the path of a file on this machine and stream its
//! bytes to the peer — which is the same peer whose responses the model reads, so a
//! prompt-injected `{"path": "/home/op/.ssh/id_ed25519"}` was a file-exfiltration primitive.
//! The content gates (SRT checked one byte, `0x47`; RTMP an `FLV\x01` magic) are format
//! sniffing, not confinement: a 64 MiB read of any file the process can open, shipped raw.
//!
//! A [`MediaRoot`] is the client's boundary, set by the `media_root` startup parameter and
//! defaulting to a NetGet-owned directory under the platform's local-data dir — the same
//! reasoning as the Git client's `sandbox::default_root`: neither `$HOME` nor the working
//! directory, which are exactly what is being protected. A path is canonicalised (the file
//! has to exist to be read) and must sit under the root and be a regular file.

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

/// Startup parameter naming the directory.
pub const MEDIA_ROOT_PARAM: &str = "media_root";

/// Where files come from when no `media_root` is configured.
pub fn default_root() -> PathBuf {
    if let Some(data) = dirs::data_local_dir() {
        return data.join("netget").join("media");
    }
    if let Some(home) = dirs::home_dir() {
        return home.join(".netget-media");
    }
    std::env::temp_dir().join("netget-media")
}

/// One client's media boundary: a canonical directory that exists.
#[derive(Debug, Clone)]
pub struct MediaRoot {
    root: PathBuf,
}

impl MediaRoot {
    /// Build the boundary from the `media_root` startup parameter, creating the directory
    /// so the default is usable without preparation.
    pub fn new(configured: Option<&str>) -> Result<Self> {
        let requested = match configured {
            Some(raw) if !raw.trim().is_empty() => PathBuf::from(raw.trim()),
            _ => default_root(),
        };
        std::fs::create_dir_all(&requested).with_context(|| {
            format!(
                "could not create the {MEDIA_ROOT_PARAM} directory {}",
                requested.display()
            )
        })?;
        let root = requested.canonicalize().with_context(|| {
            format!(
                "could not canonicalise {MEDIA_ROOT_PARAM} {}",
                requested.display()
            )
        })?;
        if !root.is_dir() {
            bail!("{MEDIA_ROOT_PARAM} ({}) is not a directory", root.display());
        }
        Ok(Self { root })
    }

    /// The canonical root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The file `raw` names, if it is a regular file under the root.
    ///
    /// A relative path is resolved against the root, not the working directory. The
    /// canonical form is checked, so a symlink under the root pointing outside it is
    /// refused too. Refused rather than relocated, naming the parameter that widens it.
    pub fn resolve(&self, raw: &str, context: &str) -> Result<PathBuf> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            bail!("{context} is empty");
        }
        let requested = Path::new(trimmed);
        let absolute = if requested.is_absolute() {
            requested.to_path_buf()
        } else {
            self.root.join(requested)
        };
        let resolved = absolute
            .canonicalize()
            .with_context(|| format!("{context} ({trimmed}) does not exist"))?;
        if !resolved.starts_with(&self.root) {
            bail!(
                "{context} ({trimmed}) resolves to {} which is outside this client's \
                 {MEDIA_ROOT_PARAM} ({}). Only files under that directory are read; put the \
                 file there, or set the `{MEDIA_ROOT_PARAM}` startup parameter to a directory \
                 that contains it.",
                resolved.display(),
                self.root.display()
            );
        }
        if !resolved.is_file() {
            bail!("{context} ({trimmed}) is not a regular file");
        }
        Ok(resolved)
    }
}
