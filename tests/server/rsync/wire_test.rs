//! The protocol-29 codec on its own: the whole-file checksum against bytes captured from a
//! stock rsync 3.2.7 exchange, the 12-byte longint form, rsync's sort order, and a file list
//! written by NetGet and read back by NetGet's reader (as a stock-daemon list is read).
use netget::server::rsync::wire::{self, Entry, FlistWriter, Kind, MuxReader, Request};

#[test]
fn checksum_matches_a_captured_exchange() {
    // seed 61 b6 de 6a, data "hello\n": the 16 bytes a stock daemon sent (rsync-wire-spec §9.2).
    let seed = i32::from_le_bytes([0x61, 0xb6, 0xde, 0x6a]);
    assert_eq!(
        hex::encode(wire::file_sum(seed, b"hello\n")),
        "36f24d09e2479ac717ffef3b25aaf6ff"
    );
    assert_eq!(wire::longint(6), [6, 0, 0, 0]);
    assert_eq!(
        wire::longint(0x1_0000_0000),
        [0xff, 0xff, 0xff, 0xff, 0, 0, 0, 0, 1, 0, 0, 0]
    );
}

#[test]
fn sort_order_is_rsyncs() {
    let mut names = [
        ("x", true),
        ("x.y", true),
        ("x-y", true),
        ("b.txt", false),
        (".", true),
        ("x/a", false),
        ("A", false),
    ];
    names.sort_by_cached_key(|(p, d)| wire::sort_key(p, *d));
    let order: Vec<&str> = names.iter().map(|(p, _)| *p).collect();
    // ".", then files bytewise, then dirs by name + "/" (0x2D < 0x2E < 0x2F), each with its subtree.
    assert_eq!(order, [".", "A", "b.txt", "x-y", "x.y", "x", "x/a"]);
}

fn entry(path: &str, kind: Kind, data: &[u8]) -> Entry {
    let bits = match kind {
        Kind::File => wire::S_IFREG | 0o644,
        Kind::Dir => wire::S_IFDIR | 0o755,
        Kind::Symlink => wire::S_IFLNK | 0o777,
    };
    Entry {
        path: path.into(),
        kind,
        data: data.to_vec(),
        mode: bits,
        mtime: 1_704_164_645,
        uid: 1000,
        gid: 1000,
        size: if kind == Kind::Dir {
            4096
        } else {
            data.len() as u64
        },
    }
}

#[tokio::test]
async fn file_list_round_trip() {
    let long = format!("deep/{}", "n".repeat(600));
    let entries = [
        entry(".", Kind::Dir, b""),
        entry("a.txt", Kind::File, b"hello\n"),
        entry("link", Kind::Symlink, b"a.txt"),
        entry("deep", Kind::Dir, b""),
        entry(&long, Kind::File, b"x"),
    ];
    let req = Request {
        sender: true,
        recursive: true,
        links: true,
        owner: true,
        group: true,
        ..Default::default()
    };
    let mut bytes = Vec::new();
    let mut w = FlistWriter::default();
    for (i, e) in entries.iter().enumerate() {
        w.entry(e, i == 0, &req, &mut bytes);
    }
    FlistWriter::finish(&req, 0, &mut bytes);
    // Frame it as a daemon would, split at an awkward boundary, with a keep-alive between.
    let (head, tail) = bytes.split_at(7);
    let mut framed = wire::data_frames(head);
    framed.extend(wire::frame(wire::MSG_DATA, b""));
    framed.extend(wire::frame(wire::MSG_INFO, b"hello from the daemon\n"));
    framed.extend(wire::data_frames(tail));
    let mut r = MuxReader::new(&framed[..]);
    let (back, io_error) = r.file_list(&req).await.unwrap();
    assert_eq!(io_error, 0);
    assert_eq!(back.len(), entries.len());
    for (a, b) in entries.iter().zip(&back) {
        assert_eq!(
            (&a.path, a.kind, a.mode, a.mtime, a.uid, a.size),
            (&b.path, b.kind, b.mode, b.mtime, b.uid, b.size)
        );
    }
    assert_eq!(back[2].data, b"a.txt", "the symlink target");
    assert_eq!(
        r.messages,
        [(wire::MSG_INFO, "hello from the daemon".to_string())]
    );
}

#[tokio::test]
async fn unsafe_names_are_refused() {
    let req = Request {
        sender: true,
        recursive: true,
        ..Default::default()
    };
    for bad in ["../etc/passwd", "/etc/passwd", "a/../../b"] {
        let mut bytes = Vec::new();
        FlistWriter::default().entry(&entry(bad, Kind::File, b"x"), false, &req, &mut bytes);
        FlistWriter::finish(&req, 0, &mut bytes);
        let framed = wire::data_frames(&bytes);
        let err = MuxReader::new(&framed[..])
            .file_list(&req)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unsafe"), "{bad}: {err}");
    }
    assert!(
        wire::entry_from_json(&serde_json::json!({"path": "a/../b", "type": "file"}), 0).is_err()
    );
}
