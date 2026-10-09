//! NMDC specification byte vectors: keys are raw octets with six reserved values escaped.
use netget::client::dc::calculate_dc_key;
#[test]
fn standard_dcplus_lock_preserves_binary_key() {
    let key = calculate_dc_key(b"EXTENDEDPROTOCOLABCABCABCABCABCABC").unwrap();
    assert_eq!(
        key,
        [
            0x14, 0xd1, 0xc0, 0x11, 0xb0, 0xa0, 0x10, 0x10, 0x41, 0x20, 0xd1, 0xb1, 0xb1, 0xc0,
            0xc0, 0x30, 0xd0, 0x30, 0x10, 0x20, 0x30, 0x10, 0x20, 0x30, 0x10, 0x20, 0x30, 0x10,
            0x20, 0x30, 0x10, 0x20, 0x30, 0x10
        ]
    );
}
#[test]
fn escape_reserved_octets_and_reject_short_locks() {
    assert_eq!(calculate_dc_key(b"AA").unwrap(), b"D/%DCN000%/");
    assert_eq!(calculate_dc_key(&[0, 0x42]).unwrap(), b"t/%DCN036%/");
    for lock in [&[][..], &[42][..]] {
        assert!(calculate_dc_key(lock).is_err());
    }
    assert!(calculate_dc_key(&vec![1; 65537]).is_err());
}
#[tokio::test]
async fn bounded_frames_keep_coalesced_and_fragmented_binary() {
    use netget::utils::line_reader::read_bounded_delimited;
    use tokio::io::{AsyncWriteExt, BufReader};
    let (mut write, read) = tokio::io::duplex(64);
    let task = tokio::spawn(async move {
        write.write_all(b"\xffA").await.unwrap();
        tokio::task::yield_now().await;
        write.write_all(b"|B|tail").await.unwrap();
    });
    let mut reader = BufReader::new(read);
    assert_eq!(
        read_bounded_delimited(&mut reader, b'|', 4).await.unwrap(),
        Some(b"\xffA|".to_vec())
    );
    assert_eq!(
        read_bounded_delimited(&mut reader, b'|', 4).await.unwrap(),
        Some(b"B|".to_vec())
    );
    assert_eq!(
        read_bounded_delimited(&mut reader, b'|', 4)
            .await
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::UnexpectedEof
    );
    task.await.unwrap();
    let mut reader = BufReader::new(&b"12345|"[..]);
    assert_eq!(
        read_bounded_delimited(&mut reader, b'|', 4)
            .await
            .unwrap_err()
            .kind(),
        std::io::ErrorKind::InvalidData
    );
}
