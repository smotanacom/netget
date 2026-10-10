//! `mount_disk`'s `disk_image` (and the startup `disk_image`) may only name a file under the
//! server's `image_dir`.
//!
//! The action is offered to the model on every event a USB/IP peer provokes; the peer then
//! reads the mounted image as SCSI sectors and, with `write_protect: false`, writes it in
//! place. Any host path was therefore an arbitrary file read, and write, one prompt
//! injection away. No peer and no model here: the boundary is checked at the executor,
//! before any file is opened.

use netget::llm::actions::protocol_trait::Server;
use netget::server::usb::msc::image_dir::{ImageDir, IMAGE_DIR_PARAM};
use netget::server::UsbMscProtocol;

fn scratch(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "netget-usb-image-dir-{}-{name}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn an_image_under_the_root_resolves_whether_or_not_it_exists_yet() {
    let dir = scratch("inside");
    let root = dir.join("images");
    std::fs::create_dir_all(root.join("sub")).unwrap();
    std::fs::write(root.join("sub/existing.img"), b"x").unwrap();
    let images = ImageDir::new(Some(root.to_str().unwrap())).unwrap();
    let canon = root.canonicalize().unwrap();
    assert_eq!(
        images.resolve("new.img", "p").unwrap(),
        canon.join("new.img")
    );
    assert_eq!(
        images.resolve("sub/existing.img", "p").unwrap(),
        canon.join("sub/existing.img")
    );
    assert_eq!(
        images
            .resolve(root.join("sub/../sub/existing.img").to_str().unwrap(), "p")
            .unwrap(),
        canon.join("sub/existing.img")
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn anything_outside_the_root_is_refused_by_name_before_it_is_touched() {
    let dir = scratch("outside");
    let root = dir.join("images");
    std::fs::create_dir_all(&root).unwrap();
    let victim = dir.join("authorized_keys");
    std::fs::write(&victim, b"ssh-ed25519 AAAA").unwrap();
    let images = ImageDir::new(Some(root.to_str().unwrap())).unwrap();
    for raw in [
        victim.to_str().unwrap().to_string(),
        "../authorized_keys".to_string(),
        "/etc/hostname".to_string(),
        format!("{}/", root.display()),
        ".".to_string(),
        "".to_string(),
    ] {
        let err = images
            .resolve(&raw, "the mount_disk 'disk_image'")
            .expect_err(&raw);
        let text = err.to_string();
        assert!(
            text.contains(IMAGE_DIR_PARAM) || text.contains("file name") || text.contains("empty"),
            "{raw}: {text}"
        );
    }
    assert_eq!(std::fs::read(&victim).unwrap(), b"ssh-ed25519 AAAA");
    let _ = std::fs::remove_dir_all(&dir);
}

#[cfg(unix)]
#[test]
fn a_symlink_at_the_image_path_is_refused() {
    let dir = scratch("symlink");
    let root = dir.join("images");
    std::fs::create_dir_all(&root).unwrap();
    let victim = dir.join("secret");
    std::fs::write(&victim, b"s").unwrap();
    std::os::unix::fs::symlink(&victim, root.join("disk.img")).unwrap();
    let images = ImageDir::new(Some(root.to_str().unwrap())).unwrap();
    assert!(images
        .resolve("disk.img", "p")
        .unwrap_err()
        .to_string()
        .contains("symlink"));
    let _ = std::fs::remove_dir_all(&dir);
}

/// The executor refuses before any handler lookup or file open: a server whose image_dir is
/// a scratch directory, asked to mount a file elsewhere, must neither open nor create it.
#[test]
fn mount_disk_outside_image_dir_is_refused_at_the_executor_and_creates_nothing() {
    let dir = scratch("executor");
    let root = dir.join("images");
    std::fs::create_dir_all(&root).unwrap();
    let outside = dir.join("planted.img");
    let protocol =
        UsbMscProtocol::with_image_dir(ImageDir::new(Some(root.to_str().unwrap())).unwrap());
    let err = protocol
        .execute_action(serde_json::json!({
            "type": "mount_disk",
            "connection_id": "conn-1",
            "disk_image": outside.to_str().unwrap(),
            "write_protect": false,
            "size_mb": 1
        }))
        .expect_err("outside image_dir");
    assert!(format!("{err:#}").contains(IMAGE_DIR_PARAM), "{err:#}");
    assert!(
        !outside.exists(),
        "the refusal must not have created {}",
        outside.display()
    );

    // Inside the root the path is accepted; the failure that follows is the missing device
    // handler, which is this test's proof that the path check came first and passed.
    let err = protocol
        .execute_action(serde_json::json!({
            "type": "mount_disk",
            "connection_id": "conn-1",
            "disk_image": "fine.img",
            "size_mb": 1
        }))
        .expect_err("no device is attached in this test");
    let text = format!("{err:#}");
    assert!(
        !text.contains(IMAGE_DIR_PARAM),
        "a path inside the root must not be refused: {text}"
    );
    let _ = std::fs::remove_dir_all(&dir);
}
