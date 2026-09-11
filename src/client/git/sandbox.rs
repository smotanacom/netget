//! Filesystem confinement for the Git client.
//!
//! Every path this client touches comes from the model: `git_clone`'s `path` and `url`, and
//! the `remote_addr` / `local_path` a caller hands `connect()`. Without a boundary, libgit2
//! is pointed at whatever the model names — the operator's own repositories included, where
//! `git_checkout` and `git_delete_branch --force` are destructive and `git_push` publishes.
//!
//! So there is a boundary: one root directory, declared as the `allowed_root` startup
//! parameter, defaulting to a NetGet-owned workspace. Every model-supplied path is resolved
//! against it and **refused** if it lands outside.
//!
//! ## Refuse, never relocate
//!
//! A path outside the root is an error naming `allowed_root`. It is deliberately not
//! rewritten to sit inside: a model that asked for `/tmp/x` and silently got
//! `<root>/tmp/x` has a confusing bug, where a refusal is a clear one, and a silent
//! rewrite would also make `git_clone` report success for a repository that is not where
//! the model believes it is.
//!
//! A *relative* path is a different thing and is resolved against the root rather than the
//! process's working directory. That is not a relocation — a relative path names no
//! location until something supplies a base — and resolving against the cwd would mean
//! `./repo` was refused for being outside the root, which is useless. `local_path: "./my-repo"`,
//! which is what every startup example in `actions.rs` shows, therefore means
//! `<allowed_root>/my-repo`.
//!
//! ## How escape is prevented
//!
//! `..` and symlinks both have to be dealt with, and neither yields to string matching:
//! `<root>/a/../../etc` is textually inside the root, and `<root>/link -> /etc` is textually
//! inside it too. [`GitSandbox::resolve`] therefore canonicalises rather than compares text.
//! Since a clone destination does not exist yet, it canonicalises the **longest existing
//! ancestor** — which is where every symlink and every `..` that can be resolved lives — and
//! re-appends the non-existent tail, refusing any `..` in that tail because there is nothing
//! for it to mean. The comparison is then between two canonical absolute paths.
//!
//! A TOCTOU window remains: a symlink created between the check and libgit2's own `open()`
//! would not be seen. Closing it needs `openat2(RESOLVE_BENEATH)` or a per-operation chroot,
//! neither of which libgit2 exposes. The window requires an attacker who can already write
//! inside the workspace, which is a strictly larger capability than anything this guard is
//! defending against.

use anyhow::{bail, Context, Result};
use std::path::{Path, PathBuf};

/// Startup parameter naming the directory the client may touch. Named in every refusal, so
/// the operator is told which knob to turn rather than just that something was denied.
pub const ALLOWED_ROOT_PARAM: &str = "allowed_root";

/// Startup parameter opting in to operations that reach *outside* the sandbox entirely.
pub const ALLOW_REMOTE_WRITES_PARAM: &str = "allow_remote_writes";

/// The workspace NetGet owns when `allowed_root` is not given.
///
/// Deliberately neither `$HOME` nor the process's working directory: the second would be
/// whatever repository the operator happened to launch NetGet from, which is precisely the
/// thing being protected. The platform's local-data directory gives NetGet a subtree nothing
/// else writes to, stable across runs, so a repository cloned by one client is still there
/// for the next one — `~/Library/Application Support/netget/git-workspace` on macOS,
/// `~/.local/share/netget/git-workspace` on Linux, `%LOCALAPPDATA%\netget\git-workspace` on
/// Windows.
///
/// **Not `~/.netget/git-workspace`**, which was the obvious first choice and is impossible:
/// `Settings::settings_path` (`src/settings.rs`) makes `~/.netget` NetGet's settings *file*,
/// so `create_dir_all` on a path beneath it fails with `ENOTDIR`. It was the existing Git
/// client tests that caught this, which is a good argument for having them.
pub fn default_root() -> PathBuf {
    if let Some(data) = dirs::data_local_dir() {
        return data.join("netget").join("git-workspace");
    }
    if let Some(home) = dirs::home_dir() {
        return home.join(".netget-git-workspace");
    }
    // No home and no data directory (a daemon with no passwd entry, some containers). The
    // temp directory is the only writable location that can be assumed, and a confined
    // scratch space there is still far better than no confinement.
    std::env::temp_dir().join("netget-git-workspace")
}

/// One Git client's filesystem boundary.
///
/// Cheap to clone — it is a canonical `PathBuf` and a flag — which matters because every
/// git2 call runs on `spawn_blocking` and needs an owned copy.
#[derive(Debug, Clone)]
pub struct GitSandbox {
    /// Canonicalised, and the directory is known to exist: both are established in
    /// [`GitSandbox::new`] so that `resolve` never has to reason about a root that does not.
    root: PathBuf,
    allow_remote_writes: bool,
}

impl GitSandbox {
    /// Build the boundary from the client's startup parameters.
    ///
    /// Creates the root if it does not exist — `canonicalize` needs a real directory, and a
    /// client whose workspace has to be pre-made by hand would fail on first use for a
    /// reason nobody would guess.
    pub fn new(configured_root: Option<&str>, allow_remote_writes: bool) -> Result<Self> {
        let requested = match configured_root {
            Some(raw) if !raw.trim().is_empty() => PathBuf::from(raw.trim()),
            _ => default_root(),
        };

        std::fs::create_dir_all(&requested).with_context(|| {
            format!(
                "could not create the Git client's {ALLOWED_ROOT_PARAM} directory {}",
                requested.display()
            )
        })?;

        let root = requested.canonicalize().with_context(|| {
            format!(
                "could not canonicalise the Git client's {ALLOWED_ROOT_PARAM} {}",
                requested.display()
            )
        })?;

        if !root.is_dir() {
            bail!(
                "the Git client's {ALLOWED_ROOT_PARAM} ({}) is not a directory",
                root.display()
            );
        }

        Ok(Self {
            root,
            allow_remote_writes,
        })
    }

    /// The canonical root. Every accepted path is under this.
    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Whether operations that publish outside the sandbox were opted into.
    pub fn remote_writes_allowed(&self) -> bool {
        self.allow_remote_writes
    }

    /// Refuse a verb that reaches outside the sandbox unless it was opted into.
    ///
    /// Confinement bounds what can be *read and written on this disk*; it says nothing about
    /// what gets pushed to a remote. `git_push` and the remote half of `git_delete_branch`
    /// both send to a URL that lives in the cloned repository's own config, using whatever
    /// credentials the client was given — so they can publish to, or delete a branch on, a
    /// real forge, and no allow-list of local paths can prevent that. Those two are gated.
    ///
    /// Verbs whose damage is *inside* the sandbox are deliberately **not** gated:
    /// `git_checkout` and a `force` local branch delete destroy history in a scratch clone,
    /// which is what a scratch clone is for. Gating them would train the operator to turn
    /// the flag on for routine work, which is how an opt-in stops meaning anything.
    pub fn require_remote_writes(&self, verb: &str) -> Result<()> {
        if self.allow_remote_writes {
            return Ok(());
        }
        bail!(
            "{verb} is refused: it writes to a remote repository, which is outside anything \
             {ALLOWED_ROOT_PARAM} can confine — the remote URL comes from the cloned \
             repository's own config and the push uses this client's credentials. Set the \
             `{ALLOW_REMOTE_WRITES_PARAM}` startup parameter to true to permit it. It \
             defaults to false because a model that can push can publish or delete work on a \
             real forge, and that is not recoverable by deleting the workspace."
        );
    }

    /// Resolve a model-supplied path and refuse it if it escapes the root.
    ///
    /// The path need not exist — this is what a clone destination goes through. `context`
    /// names the thing being resolved (`"git_clone path"`, `"local_path"`) and appears in
    /// the error, because "path refused" without saying *which* path is unactionable when
    /// three of them are in play.
    pub fn resolve(&self, raw: &str, context: &str) -> Result<PathBuf> {
        let trimmed = raw.trim();
        if trimmed.is_empty() {
            bail!("{context} is empty");
        }

        let requested = Path::new(trimmed);
        // A relative path is resolved against the root, not the process's working
        // directory — see the module docs. An absolute path is taken at face value and
        // then checked.
        let absolute = if requested.is_absolute() {
            requested.to_path_buf()
        } else {
            self.root.join(requested)
        };

        // Canonicalise the longest ancestor that exists, so symlinks and resolvable `..`
        // are dealt with by the OS rather than by string surgery, then re-append the tail.
        let mut tail: Vec<std::ffi::OsString> = Vec::new();
        let mut cursor = absolute.clone();
        let canonical_prefix = loop {
            if let Ok(canonical) = cursor.canonicalize() {
                break canonical;
            }
            let name = cursor.file_name().map(|n| n.to_os_string());
            let parent = cursor.parent().map(|p| p.to_path_buf());
            match (name, parent) {
                (Some(name), Some(parent)) => {
                    if name == ".." {
                        // A `..` below a component that does not exist cannot be resolved
                        // against anything real, so there is no safe interpretation. Refuse.
                        bail!(
                            "{context} ({trimmed}) contains '..' inside a path that does not \
                             exist, so it cannot be resolved safely"
                        );
                    }
                    tail.push(name);
                    cursor = parent;
                }
                _ => bail!(
                    "{context} ({trimmed}) has no resolvable existing ancestor, so it cannot \
                     be checked against {ALLOWED_ROOT_PARAM}"
                ),
            }
        };

        let mut resolved = canonical_prefix;
        for name in tail.into_iter().rev() {
            resolved.push(name);
        }

        if !resolved.starts_with(&self.root) {
            bail!(
                "{context} ({trimmed}) resolves to {} which is outside this Git client's \
                 {ALLOWED_ROOT_PARAM} ({}). Refused rather than relocated: silently moving it \
                 inside the root would leave the repository somewhere other than where it was \
                 asked for. Point it inside the root, or set the `{ALLOWED_ROOT_PARAM}` \
                 startup parameter to a directory that contains it.",
                resolved.display(),
                self.root.display()
            );
        }

        Ok(resolved)
    }
}

/// Where a `git_clone` reads from: a network URL, or a path on this disk.
///
/// The distinction matters because the source is model-supplied too. A local clone source
/// is a **read** of an arbitrary repository — point it at the operator's private work and
/// the contents land in the sandbox, from where a permitted `git_push` could publish them.
/// So a local source is confined; a network URL is not a filesystem concern at all.
#[derive(Debug, PartialEq, Eq)]
pub enum CloneSource {
    /// Fetched over the network. Out of scope for path confinement.
    Remote,
    /// Read from this filesystem. The payload is the path portion, which for a `file://`
    /// URL is what follows the scheme.
    Local(String),
}

/// Classify a clone URL as network or local.
///
/// `file://` is a local path wearing a URL, so it is unwrapped rather than waved through on
/// the strength of containing `://` — that would have been a hole big enough to drive the
/// whole guard through.
pub fn classify_clone_source(url: &str) -> CloneSource {
    let trimmed = url.trim();

    if let Some(rest) = trimmed.strip_prefix("file://") {
        // `file:///abs/path` (three slashes, empty authority) is the common form; strip the
        // empty authority so what is left is the absolute path.
        let path = rest.strip_prefix("localhost/").unwrap_or(rest);
        return CloneSource::Local(if path.starts_with('/') {
            path.to_string()
        } else {
            format!("/{path}")
        });
    }

    if trimmed.contains("://") {
        return CloneSource::Remote;
    }

    // scp-style `git@host:owner/repo.git`. A Windows drive letter (`C:\repos\x`) also
    // contains a colon but has no `@`, so requiring one keeps the two apart.
    if let Some((before_colon, _)) = trimmed.split_once(':') {
        if before_colon.contains('@') && !before_colon.contains('/') {
            return CloneSource::Remote;
        }
    }

    CloneSource::Local(trimmed.to_string())
}
