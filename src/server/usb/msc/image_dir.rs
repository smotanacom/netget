//! The one directory a disk image may live in.
//!
//! `mount_disk` is offered to the model on every event a USB/IP peer provokes, and its
//! `disk_image` named any path on the host: the file was opened read/write (created if
//! absent), memory-mapped, and from then on the peer read it as SCSI sectors and, with
//! `write_protect: false`, wrote it in place. A peer that talked the model into
//! `{"disk_image": "/home/op/.ssh/authorized_keys"}` read the file; with write protection
//! off it rewrote it. The startup `disk_image` parameter, model-settable through
//! `open_server`, was the same path with the same open.
//!
//! An [`ImageDir`] is the boundary: the `image_dir` startup parameter, defaulting to
//! NetGet's own `usb-images` directory under the platform's local-data dir — neither
//! `$HOME` nor the working directory, which are what is being protected. A path's parent is
//! canonicalised and must sit under the root; the final component is a plain file name;
//! an existing target must be a regular file (a symlink is refused, and the open itself
//! uses `O_NOFOLLOW`).

use std::path::{Component, Path, PathBuf};

use anyhow::{bail, Context, Result};

/// Startup parameter naming the directory.
pub const IMAGE_DIR_PARAM: &str = "image_dir";

/// Where images live when no `image_dir` is configured.
pub fn default_image_dir() -> PathBuf {
    if let Some(data) = dirs::data_local_dir() {
        return data.join("netget").join("usb-images");
    }
    if let Some(home) = dirs::home_dir() {
        return home.join(".netget-usb-images");
    }
    std::env::temp_dir().join("netget-usb-images")
}

/// One server's image boundary: a canonical directory that exists.
#[derive(Debug, Clone)]
pub struct ImageDir {
    root: PathBuf,
}

impl ImageDir {
    /// Build the boundary from the `image_dir` startup parameter, creating the directory so
    /// the default is usable without preparation.
    pub fn new(configured: Option<&str>) -> Result<Self> {
        let requested = match configured {
            Some(raw) if !raw.trim().is_empty() => PathBuf::from(raw.trim()),
            _ => default_image_dir(),
        };
        std::fs::create_dir_all(&requested).with_context(|| {
            format!(
                "could not create the {IMAGE_DIR_PARAM} directory {}",
                requested.display()
            )
        })?;
        let root = requested.canonicalize().with_context(|| {
            format!(
                "could not canonicalise {IMAGE_DIR_PARAM} {}",
                requested.display()
            )
        })?;
        if !root.is_dir() {
            bail!("{IMAGE_DIR_PARAM} ({}) is not a directory", root.display());
        }
        Ok(Self { root })
    }

    /// The canonical root.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// The image file `raw` names, which may not exist yet: its directory must exist under
    /// the root, its name must be a plain file name, and if it exists it must be a regular
    /// file. A relative path is resolved against the root.
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
        let name = match absolute.components().next_back() {
            Some(Component::Normal(name)) => name.to_os_string(),
            _ => bail!("{context} ({trimmed}) does not end in a file name"),
        };
        let parent = absolute
            .parent()
            .with_context(|| format!("{context} ({trimmed}) has no directory"))?;
        let parent = parent
            .canonicalize()
            .with_context(|| format!("{context} ({trimmed}): its directory does not exist"))?;
        if !parent.starts_with(&self.root) {
            bail!(
                "{context} ({trimmed}) is in {} which is outside this server's \
                 {IMAGE_DIR_PARAM} ({}). Only images under that directory are served; put the \
                 image there, or set the `{IMAGE_DIR_PARAM}` startup parameter to a directory \
                 that contains it.",
                parent.display(),
                self.root.display()
            );
        }
        let resolved = parent.join(name);
        if let Ok(meta) = std::fs::symlink_metadata(&resolved) {
            if meta.file_type().is_symlink() {
                bail!("{context} ({trimmed}) is a symlink; an image must be a regular file",);
            }
            if !meta.is_file() {
                bail!("{context} ({trimmed}) exists and is not a regular file");
            }
        }
        Ok(resolved)
    }
}
