# NetGet's wtransport-proto 0.7.2 patches

The crates.io `wtransport-proto` 0.7.2 source (MIT OR Apache-2.0; original manifest
`Cargo.toml.orig`), patched with QPACK and SETTINGS bounds. The changes and the reasons are
described with the driver changes in `vendor/wtransport/README.netget.md`;
`tests/vendored_wtransport_patch_test.rs` decodes hostile field sections and SETTINGS frames
against this copy.
