//! libnbd 1.24.3 (C, independent, unchanged) against NetGet's NBD server: nbdinfo lists the
//! exports, describes one (size, read-only, base:allocation, block sizes, description) and maps
//! its allocation; nbdcopy copies the whole export byte for byte and fails on the error region;
//! refused and unknown exports are refused. Fails, never skips.
use crate::helpers::nbd::*;
use netget::state::AccessLogOwner;

#[tokio::test(flavor = "multi_thread")]
async fn libnbd_against_netget() {
    let state = state();
    let (sid, addr) = server_in(&state).await;
    let uri = |export: &str| format!("nbd://{addr}/{export}");

    let (ok, out, err) = libnbd("NETGET_NBD_NBDINFO", &["--list", &format!("nbd://{addr}")]).await;
    assert!(ok, "{out}{err}");
    assert!(
        out.contains("export=\"disk0\"")
            && out.contains("description: boot disk")
            && out.contains("export=\"flaky\""),
        "{out}"
    );

    let (ok, out, err) = libnbd("NETGET_NBD_NBDINFO", &[&uri("disk0")]).await;
    assert!(ok, "{out}{err}");
    for line in [
        "export-size: 1048576",
        "is_read_only: true",
        "base:allocation",
        "block_size_preferred: 4096",
        "description: boot disk",
        "structured",
    ] {
        assert!(out.contains(line), "missing {line:?}:\n{out}");
    }

    let (ok, out, err) = libnbd("NETGET_NBD_NBDINFO", &["--map", &uri("disk0")]).await;
    assert!(ok, "{out}{err}");
    let map: Vec<(u64, u64, String)> = out
        .lines()
        .filter_map(|l| {
            let f: Vec<&str> = l.split_whitespace().collect();
            Some((
                f.first()?.parse().ok()?,
                f.get(1)?.parse().ok()?,
                f.get(3..)?.join(" "),
            ))
        })
        .collect();
    assert_eq!(
        map,
        vec![
            (0, 12, "data".into()),
            (12, 4084, "hole,zero".into()),
            (4096, 512, "data".into()),
            (4608, 3584, "hole,zero".into()),
            (8192, 4, "data".into()),
            (8196, 1040380, "hole,zero".into()),
        ],
        "{out}"
    );

    let dir = tempfile::tempdir().unwrap();
    let copy = dir.path().join("disk0.img");
    let (ok, out, err) = libnbd(
        "NETGET_NBD_NBDCOPY",
        &[&uri("disk0"), &copy.display().to_string()],
    )
    .await;
    assert!(ok, "{out}{err}");
    let bytes = std::fs::read(&copy).unwrap();
    let mut want = vec![0u8; 1 << 20];
    want[..12].copy_from_slice(b"hello netget");
    want[4096..4608].fill(0xab);
    want[8192..8196].copy_from_slice(&[0xde, 0xad, 0xbe, 0xef]);
    assert!(bytes == want, "the copy differs from the export");

    let (ok, out, err) = libnbd(
        "NETGET_NBD_NBDCOPY",
        &[
            &uri("flaky"),
            &dir.path().join("flaky.img").display().to_string(),
        ],
    )
    .await;
    assert!(!ok && err.contains("Input/output error"), "{out}{err}");
    let (ok, _, err) = libnbd("NETGET_NBD_NBDINFO", &[&uri("secret")]).await;
    assert!(!ok && err.to_lowercase().contains("refused"), "{err}");
    let (ok, _, err) = libnbd("NETGET_NBD_NBDINFO", &[&uri("nope")]).await;
    assert!(!ok && err.contains("has no export named 'nope'"), "{err}");

    let requests = state
        .list_access_logs_for(Some(AccessLogOwner::Server(sid.as_u32())), None)
        .await;
    let asked: Vec<_> = requests
        .iter()
        .filter(|e| e.event_type == "nbd_export_request")
        .map(|e| e.request["export"].as_str().unwrap_or_default().to_owned())
        .collect();
    for name in ["disk0", "flaky", "secret", "nope"] {
        assert!(
            asked.iter().any(|a| a == name),
            "{name} never reached the handler: {asked:?}"
        );
    }
    state.remove_server(sid).await;
}
