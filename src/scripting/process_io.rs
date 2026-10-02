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

impl ProcessGroup {
    pub fn configure(command: &mut tokio::process::Command) {
        #[cfg(unix)]
        command.process_group(0);
        let _ = command;
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
