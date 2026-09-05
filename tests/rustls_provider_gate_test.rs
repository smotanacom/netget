//! The rustls provider gate in `src/bin/netget.rs` must name every feature that enables
//! `dep:rustls`.
//!
//! rustls 0.23 panics unless exactly one `CryptoProvider` is active, and a NetGet build can
//! easily have two: `ring` arrives with most TLS-using features, `aws-lc-rs` with the AWS SDK.
//! `--features kubernetes,s3` had both and panicked inside `kube::Client::try_default()`.
//! `main` installs one up front to settle it.
//!
//! That gate is a hand-written `cfg(any(...))` list, and a hand-written list drifts: it named
//! 5 of the 15 features that enable rustls before this test existed. A missing feature is not
//! a warning -- under that feature `rustls` is a direct dependency the block never runs for,
//! so the process starts with no provider and panics the first time anything builds a TLS
//! config.
//!
//! This derives the true set from Cargo.toml so the list cannot fall behind again. It reads
//! the manifest rather than using `cfg!`, deliberately: a `cfg!` check can only see the
//! features of the build running the test, and the whole failure mode here is a feature
//! combination nobody built.

use std::collections::BTreeSet;
use std::path::Path;

fn manifest() -> String {
    std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"))
        .expect("read Cargo.toml")
}

fn main_rs() -> String {
    std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("src/bin/netget.rs"))
        .expect("read src/bin/netget.rs")
}

/// Feature names whose definition mentions `dep:rustls`.
fn features_enabling_rustls(cargo: &str) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for line in cargo.lines() {
        let line = line.trim();
        if line.starts_with('#') || !line.contains("dep:rustls") {
            continue;
        }
        let Some((name, rest)) = line.split_once('=') else {
            continue;
        };
        let name = name.trim();
        // Only feature definitions (`foo = [...]`), not dependency tables.
        if !rest.trim_start().starts_with('[') {
            continue;
        }
        if !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
        {
            out.insert(name.to_string());
        }
    }
    out
}

/// Feature names inside the first `cfg(any(...))` block of `main`.
fn gated_features(src: &str) -> BTreeSet<String> {
    let start = src.find("#[cfg(any(").expect("provider gate not found");
    let end = src[start..].find("))]").expect("unterminated gate") + start;
    let mut out = BTreeSet::new();
    for part in src[start..end].split("feature = \"").skip(1) {
        if let Some((name, _)) = part.split_once('"') {
            out.insert(name.to_string());
        }
    }
    out
}

#[test]
fn provider_gate_covers_every_feature_that_enables_rustls() {
    let cargo = manifest();
    let enabling = features_enabling_rustls(&cargo);
    assert!(
        enabling.len() > 5,
        "sanity: expected many features to enable dep:rustls, parsed {enabling:?}"
    );

    let gated = gated_features(&main_rs());
    let missing: Vec<_> = enabling.difference(&gated).cloned().collect();
    assert!(
        missing.is_empty(),
        "these features enable dep:rustls but are absent from the CryptoProvider gate in \
         src/bin/netget.rs, so a build with one of them installs no provider and panics the \
         first time rustls builds a config: {missing:?}"
    );
}

#[test]
fn provider_gate_names_no_feature_that_does_not_enable_rustls() {
    let cargo = manifest();
    let enabling = features_enabling_rustls(&cargo);
    let gated = gated_features(&main_rs());
    let stale: Vec<_> = gated.difference(&enabling).cloned().collect();
    assert!(
        stale.is_empty(),
        "the CryptoProvider gate names features that do not enable dep:rustls: {stale:?}. \
         Under such a feature alone, `rustls` is not a direct dependency and the block does \
         not compile. Either add dep:rustls to the feature or drop it from the gate."
    );
}
