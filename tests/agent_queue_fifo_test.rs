//! FIFO notifications never contact a model and must never overwrite regular files.
#![cfg(unix)]

use netget::llm::agent_queue::LlmRequestQueue;
use std::io::Read;
use std::os::unix::fs::{symlink, OpenOptionsExt};

fn make_fifo(path: &std::path::Path) {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(path.as_os_str().as_bytes()).unwrap();
    assert_eq!(unsafe { libc::mkfifo(path.as_ptr(), 0o600) }, 0);
}

#[test]
fn notification_does_not_overwrite_a_replacement_file_or_symlink_target() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("notifications");
    make_fifo(&path);
    let queue = LlmRequestQueue::new(Some(path.clone()));
    std::fs::remove_file(&path).unwrap();
    std::fs::write(&path, "unchanged regular file").unwrap();
    let _ = queue.submit("fixture".into(), vec![], vec![]);
    assert_eq!(
        std::fs::read_to_string(&path).unwrap(),
        "unchanged regular file"
    );
    std::fs::remove_file(&path).unwrap();
    let target = temp.path().join("target");
    std::fs::write(&target, "unchanged symlink target").unwrap();
    symlink(&target, &path).unwrap();
    let _ = queue.submit("fixture".into(), vec![], vec![]);
    assert_eq!(
        std::fs::read_to_string(&target).unwrap(),
        "unchanged symlink target"
    );
    assert_eq!(
        queue.list().len(),
        2,
        "failed push notifications must not lose queued requests"
    );
}

#[test]
fn a_real_fifo_still_receives_the_queued_request_id() {
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("notifications");
    make_fifo(&path);
    let mut reader = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NONBLOCK)
        .open(&path)
        .unwrap();
    let queue = LlmRequestQueue::new(Some(path));
    let (id, _answer) = queue.submit("fixture".into(), vec![], vec![]);
    let mut buffer = [0; 64];
    let n = reader.read(&mut buffer).unwrap();
    assert_eq!(&buffer[..n], format!("{id}\n").as_bytes());
}
