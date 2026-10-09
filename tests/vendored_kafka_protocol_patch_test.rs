//! The vendored kafka-protocol is the one Cargo builds, still carries its patch, and the
//! patch does what it is for. See vendor/kafka-protocol/README.netget.md.
//!
//! The decode test is the reproduction: unpatched, `MetadataRequest::decode` on a topics
//! array declaring 0x7fffffff entries calls `Vec::with_capacity` for ~144 GiB and the test
//! binary aborts with "memory allocation of … bytes failed" rather than failing.

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
fn kafka_protocol_patch_preserves_pinned_version_license_and_provenance() {
    let manifest = read("vendor/kafka-protocol/Cargo.toml");
    assert!(manifest.contains("name = \"kafka-protocol\""));
    assert!(manifest.contains("version = \"0.14.1\""));
    assert!(manifest.contains("license = \"MIT/Apache-2.0\""));
    for license in ["LICENSE-APACHE", "LICENSE-MIT"] {
        assert!(
            Path::new(env!("CARGO_MANIFEST_DIR"))
                .join("vendor/kafka-protocol")
                .join(license)
                .is_file(),
            "{license} missing"
        );
    }
    let provenance: serde_json::Value =
        serde_json::from_str(&read("vendor/kafka-protocol/.cargo_vcs_info.json")).unwrap();
    assert_eq!(
        provenance["git"]["sha1"],
        "838fec2967d76bffb8bae96fe2a4acfe47bfa73e"
    );
}

#[test]
fn cargo_builds_the_vendored_kafka_protocol() {
    let root = read("Cargo.toml");
    let patch = root
        .split("[patch.crates-io]")
        .nth(1)
        .map(|rest| rest.split("\n[").next().unwrap_or(rest))
        .unwrap_or("");
    assert!(
        patch
            .lines()
            .any(|l| l.trim() == r#"kafka-protocol = { path = "vendor/kafka-protocol" }"#),
        "Cargo.toml's [patch.crates-io] must point kafka-protocol at vendor/kafka-protocol"
    );

    let lock = read("Cargo.lock");
    let locked = locked_packages(&lock, "kafka-protocol");
    assert!(
        !locked.is_empty(),
        "Cargo.lock has no kafka-protocol at all"
    );
    for (version, source) in &locked {
        assert!(
            source.is_none() && version == "0.14.1",
            "Cargo.lock resolves kafka-protocol {version} from {}: the array-allocation patch \
             is not the crate being built (vendor/kafka-protocol/README.netget.md)",
            source.as_deref().unwrap_or("a path")
        );
    }
    let unused = lock.split("[[patch.unused]]").skip(1).any(|entry| {
        entry
            .lines()
            .take_while(|l| !l.starts_with("[["))
            .any(|l| l.trim() == r#"name = "kafka-protocol""#)
    });
    assert!(
        !unused,
        "Cargo.lock lists vendor/kafka-protocol under [[patch.unused]]"
    );
}

#[test]
fn both_array_decoders_bound_their_preallocation_by_the_bytes_remaining() {
    let types = read("vendor/kafka-protocol/src/protocol/types.rs");
    assert!(
        types.contains("Vec::with_capacity((n as usize).min(buf.remaining()))"),
        "Array<E>::decode lost its bound"
    );
    assert!(
        types.contains("Vec::with_capacity(((n - 1) as usize).min(buf.remaining()))"),
        "CompactArray<E>::decode lost its bound"
    );
    assert_eq!(
        types.matches("Vec::with_capacity(n as usize)").count()
            + types
                .matches("Vec::with_capacity((n - 1) as usize)")
                .count(),
        0,
        "an unbounded with_capacity from a wire count is back"
    );
}

#[cfg(feature = "kafka")]
mod decode {
    use bytes::Bytes;
    use kafka_protocol::messages::{FetchRequest, MetadataRequest, ProduceRequest};
    use kafka_protocol::protocol::Decodable;

    /// A non-flexible array whose Int32 count is i32::MAX, followed by nothing.
    const HOSTILE_ARRAY: [u8; 4] = [0x7f, 0xff, 0xff, 0xff];

    #[test]
    fn a_request_declaring_two_billion_topics_is_an_error_not_an_abort() {
        let mut buf = Bytes::from_static(&HOSTILE_ARRAY);
        let err = MetadataRequest::decode(&mut buf, 1).expect_err("truncated array");
        let text = format!("{err:#}");
        assert!(
            text.contains("Not enough bytes") || text.contains("remaining"),
            "unexpected error: {text}"
        );
    }

    #[test]
    fn fetch_and_produce_arrays_are_bounded_the_same_way() {
        // Fetch v4: replica_id, max_wait_ms, min_bytes, max_bytes, isolation_level, then
        // the topics array. Produce v3: transactional_id (nullable string), acks,
        // timeout_ms, then the topics array.
        let mut fetch = Vec::new();
        fetch.extend_from_slice(&(-1i32).to_be_bytes());
        fetch.extend_from_slice(&500i32.to_be_bytes());
        fetch.extend_from_slice(&1i32.to_be_bytes());
        fetch.extend_from_slice(&1_048_576i32.to_be_bytes());
        fetch.push(0);
        fetch.extend_from_slice(&HOSTILE_ARRAY);
        assert!(FetchRequest::decode(&mut Bytes::from(fetch), 4).is_err());

        let mut produce = Vec::new();
        produce.extend_from_slice(&(-1i16).to_be_bytes());
        produce.extend_from_slice(&1i16.to_be_bytes());
        produce.extend_from_slice(&1000i32.to_be_bytes());
        produce.extend_from_slice(&HOSTILE_ARRAY);
        assert!(ProduceRequest::decode(&mut Bytes::from(produce), 3).is_err());
    }
}
