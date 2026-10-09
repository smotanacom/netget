//! Resource and lifetime guards for trusted interpreter processes.

use std::io;
use tokio::io::{AsyncRead, AsyncReadExt};

pub const MAX_STDOUT_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_STDERR_BYTES: usize = 1024 * 1024;

/// Abort the whole Unix process group on completion, failure or cancellation.
/// Scripts remain trusted: a child that deliberately creates a new session can
/// escape this cleanup. Windows uses a kill-on-close Job Object.
pub struct ProcessGroup {
    #[cfg(unix)]
    pid: Option<u32>,
    #[cfg(windows)]
    job: std::os::windows::io::OwnedHandle,
}

/// Environment variables an interpreter child never inherits.
///
/// Handler scripts run as the operator and are not sandboxed — `AGENTS.md`'s trust
/// boundary — but a script's job is to answer a network event, and the one secret this
/// process holds for its own use is the model backend's API key. Nothing a handler does
/// needs it, while a handler supplied over MCP, loaded from a `.netget` file or written by
/// the model in operator chat could `print(os.environ["NETGET_API_KEY"])` into its stderr,
/// which is logged. The same goes for every other credential-shaped name: the names below
/// are removed exactly, and any variable whose name contains one of [`SECRET_NAME_PARTS`]
/// is removed too. `PATH`, `HOME`, `LANG` and the rest pass through, so interpreters still
/// find their modules.
pub const STRIPPED_ENV: &[&str] = &[
    "NETGET_API_KEY",
    "OPENAI_API_KEY",
    "ANTHROPIC_API_KEY",
    "NETGET_MCP_TOKEN",
    "AWS_SECRET_ACCESS_KEY",
    "AWS_SESSION_TOKEN",
    "GITHUB_TOKEN",
    "GH_TOKEN",
    "NPM_TOKEN",
];

/// Substrings (upper case) of an environment variable name that mark it as a credential.
pub const SECRET_NAME_PARTS: &[&str] = &[
    "SECRET",
    "PASSWORD",
    "PASSWD",
    "API_KEY",
    "APIKEY",
    "ACCESS_TOKEN",
    "AUTH_TOKEN",
    "PRIVATE_KEY",
    "CREDENTIAL",
];

/// Whether an environment variable named `name` is withheld from interpreter children.
pub fn env_is_stripped(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    STRIPPED_ENV.contains(&upper.as_str())
        || SECRET_NAME_PARTS.iter().any(|part| upper.contains(part))
}

impl ProcessGroup {
    /// Put the child in its own process group (so a timeout can kill its descendants) and
    /// withhold credential-bearing environment variables from it; see [`STRIPPED_ENV`].
    /// Every interpreter spawn in this module's callers goes through here.
    pub fn configure(command: &mut tokio::process::Command) {
        #[cfg(unix)]
        command.process_group(0);
        for (name, _) in std::env::vars_os() {
            if name.to_str().is_some_and(env_is_stripped) {
                command.env_remove(&name);
            }
        }
    }

    pub fn new(child: &tokio::process::Child) -> io::Result<Self> {
        #[cfg(windows)]
        {
            use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
            use windows_sys::Win32::System::JobObjects::*;
            let raw = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
            if raw.is_null() {
                return Err(io::Error::last_os_error());
            }
            let job = unsafe { OwnedHandle::from_raw_handle(raw) };
            let mut limits: JOBOBJECT_EXTENDED_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
            limits.BasicLimitInformation.LimitFlags = JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
            let configured = unsafe {
                SetInformationJobObject(
                    raw,
                    JobObjectExtendedLimitInformation,
                    (&limits as *const JOBOBJECT_EXTENDED_LIMIT_INFORMATION).cast(),
                    std::mem::size_of_val(&limits) as u32,
                )
            };
            if configured == 0 {
                return Err(io::Error::last_os_error());
            }
            let process = child
                .raw_handle()
                .ok_or_else(|| io::Error::other("script process already exited"))?;
            if unsafe { AssignProcessToJobObject(job.as_raw_handle(), process) } == 0 {
                return Err(io::Error::last_os_error());
            }
            Ok(Self { job })
        }
        #[cfg(not(windows))]
        {
            let _ = child;
            Ok(Self {
                #[cfg(unix)]
                pid: child.id(),
            })
        }
    }

    pub fn kill(&self) {
        #[cfg(windows)]
        {
            use std::os::windows::io::AsRawHandle;
            unsafe {
                windows_sys::Win32::System::JobObjects::TerminateJobObject(
                    self.job.as_raw_handle(),
                    1,
                );
            }
        }
        #[cfg(unix)]
        if let Some(pid) = self.pid.and_then(|pid| i32::try_from(pid).ok()) {
            // The child was spawned into its own process group, never ours.
            unsafe {
                libc::kill(-pid, libc::SIGKILL);
            }
        }
    }
}

impl Drop for ProcessGroup {
    fn drop(&mut self) {
        self.kill();
    }
}

/// Stop on the first byte beyond the limit, without retaining that excess.
pub async fn read_bounded<R: AsyncRead + Unpin>(
    mut reader: R,
    limit: usize,
) -> io::Result<Vec<u8>> {
    let mut output = Vec::new();
    let mut chunk = vec![0; 8192];
    loop {
        let n = reader.read(&mut chunk).await?;
        if n == 0 {
            return Ok(output);
        }
        if n > limit.saturating_sub(output.len()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("script output exceeds the {limit}-byte cap"),
            ));
        }
        output.extend_from_slice(&chunk[..n]);
    }
}

pub(crate) async fn read_line_bounded<R: tokio::io::AsyncBufRead + Unpin>(
    reader: &mut R,
    limit: usize,
) -> io::Result<String> {
    use tokio::io::AsyncBufReadExt;
    let mut line = Vec::new();
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Err(io::Error::new(
                io::ErrorKind::UnexpectedEof,
                "resident script exited before a complete response",
            ));
        }
        let newline = available.iter().position(|&byte| byte == b'\n');
        let take = newline.map_or(available.len(), |n| n + 1);
        if take > limit.saturating_sub(line.len()) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "resident script response exceeds the 8 MiB cap",
            ));
        }
        line.extend_from_slice(&available[..take]);
        reader.consume(take);
        if newline.is_some() {
            return String::from_utf8(line)
                .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error));
        }
    }
}

/// A private directory with exclusive creation and cancellation-safe removal.
pub(crate) struct ScriptDirectory(std::path::PathBuf);

impl ScriptDirectory {
    pub(crate) fn new() -> io::Result<Self> {
        let path = std::env::temp_dir().join(format!("netget-script-{}", uuid::Uuid::new_v4()));
        let mut builder = std::fs::DirBuilder::new();
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&path)?;
        Ok(Self(path))
    }

    pub(crate) fn script_path(&self) -> std::path::PathBuf {
        self.0.join("main.go")
    }
}

impl Drop for ScriptDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}
