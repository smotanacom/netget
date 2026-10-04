//! The vendored wtransport and wtransport-proto in `vendor/` are what Cargo builds, and they
//! still carry NetGet's patches (`vendor/wtransport/README.netget.md`).
//!
//! The lockfile and source checks read files only, so they hold at any feature set; the
//! decoding checks need the `webtransport` feature, which brings the crate in.
use std::fs;
use std::path::Path;

fn read(rel: &str) -> String {
    let path = Path::new(env!("CARGO_MANIFEST_DIR")).join(rel);
    fs::read_to_string(&path).unwrap_or_else(|e| panic!("cannot read {}: {e}", path.display()))
}

/// Every `[[package]]` entry in Cargo.lock named `name`: (version, source line if any).
fn locked_packages(lock: &str, name: &str) -> Vec<(String, Option<String>)> {
    lock.split("[[package]]")
        .skip(1)
        .filter_map(|entry| {
            let field = |key: &str| {
                entry.lines().find_map(|l| {
                    l.strip_prefix(&format!("{key} = "))
                        .map(|v| v.trim_matches('"').to_string())
                })
            };
            (field("name")? == name).then(|| (field("version").unwrap(), field("source")))
        })
        .collect()
}

#[test]
fn cargo_builds_the_vendored_copies_at_the_vendored_version() {
    let root = read("Cargo.toml");
    let lock = read("Cargo.lock");
    for name in ["wtransport", "wtransport-proto"] {
        assert!(
            root.lines()
                .any(|l| l.trim() == format!(r#"{name} = {{ path = "vendor/{name}" }}"#)),
            "Cargo.toml's [patch.crates-io] must point {name} at vendor/{name}"
        );
        let vendored = read(&format!("vendor/{name}/Cargo.toml"));
        assert!(
            vendored.contains("\nversion = \"0.7.2\""),
            "vendor/{name} is not 0.7.2; re-check every patch before changing the version"
        );
        assert_eq!(
            locked_packages(&lock, name),
            vec![("0.7.2".to_string(), None)],
            "Cargo.lock must resolve {name} to the vendored path copy only (a `source` line or a second entry means the patch went unused)"
        );
    }
}

#[test]
fn the_patches_are_still_in_the_source() {
    let driver = read("vendor/wtransport/src/driver/mod.rs");
    for marker in [
        "impl Drop for Driver",
        "worker: tokio::task::JoinHandle<()>",
        "readers: tokio::task::JoinSet<Result<(), DriverError>>",
        "if self.readers.len() < 16",
        "value(SettingId::H3Datagram) != 1",
    ] {
        assert!(driver.contains(marker), "driver patch lost: {marker}");
    }
    assert!(
        !driver.contains("TODO(biagio): validate settings"),
        "the driver's settings check is gone"
    );
    let endpoint = read("vendor/wtransport/src/endpoint.rs");
    assert!(
        endpoint.contains("SettingId::EnableConnectProtocol"),
        "clients must wait for the server's ENABLE_CONNECT_PROTOCOL"
    );
    let qpack = read("vendor/wtransport-proto/src/qpack.rs");
    for marker in [
        "MAX_FIELDS: usize = 64",
        "MAX_FIELD_SECTION: usize = 16 * 1024",
        "DuplicateField",
    ] {
        assert!(qpack.contains(marker), "QPACK patch lost: {marker}");
    }
    assert!(read("vendor/wtransport-proto/src/settings.rs").contains("seen_ids"));
}

#[cfg(feature = "webtransport")]
mod decoding {
    use wtransport_proto::error::ErrorCode;
    use wtransport_proto::frame::Frame;
    use wtransport_proto::qpack::{Decoder, DecodingError, Encoder};
    use wtransport_proto::settings::Settings;

    fn decode(fields: &[(&str, &str)]) -> String {
        match Decoder::decode(Encoder::encode(fields.iter().copied())) {
            Ok(h) => format!("{} fields", h.len()),
            Err(e) => format!("{e:?}"),
        }
    }
    fn err(e: DecodingError) -> String {
        format!("{e:?}")
    }

    #[test]
    fn field_sections_are_bounded_and_well_formed() {
        assert_eq!(
            decode(&[(":method", "CONNECT"), ("origin", "https://a.example")]),
            "2 fields"
        );
        let names: Vec<String> = (0..65).map(|i| format!("x-{i}")).collect();
        let many: Vec<(&str, &str)> = names.iter().map(|n| (n.as_str(), "v")).collect();
        assert_eq!(decode(&many), err(DecodingError::FieldSectionTooLarge));
        assert_eq!(decode(&many[..64]), "64 fields");
        let big = "v".repeat(16 * 1024);
        assert_eq!(
            decode(&[("x-big", &big)]),
            err(DecodingError::FieldSectionTooLarge)
        );
        assert_eq!(
            decode(&[("x-a", "1"), ("x-a", "2")]),
            err(DecodingError::DuplicateField)
        );
        assert_eq!(
            decode(&[("X-Upper", "1")]),
            err(DecodingError::InvalidField)
        );
        assert_eq!(
            decode(&[("x-a", "line\nbreak")]),
            err(DecodingError::InvalidField)
        );
        assert_eq!(
            decode(&[("x-a", "1"), (":path", "/")]),
            err(DecodingError::InvalidField)
        );
        // Required Insert Count 1: a reference into a dynamic table nobody advertised.
        assert!(matches!(
            Decoder::decode([0x01u8, 0x00]),
            Err(DecodingError::DynamicNotSupported)
        ));
    }

    #[test]
    fn a_repeated_setting_is_a_settings_error() {
        let ok = Settings::builder()
            .enable_webtransport()
            .enable_h3_datagrams()
            .build();
        let frame = ok.generate_frame();
        assert!(Settings::with_frame(&frame).is_ok());
        // SETTINGS_ENABLE_CONNECT_PROTOCOL (0x08) = 1, twice.
        let twice = Frame::new_settings(vec![0x08, 0x01, 0x08, 0x01].into());
        assert!(Settings::with_frame(&twice).err() == Some(ErrorCode::Settings));
    }
}
