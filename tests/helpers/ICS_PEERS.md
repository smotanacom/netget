# Independent ICS peers

Run `python3 tests/helpers/install_ics_peers.py /tmp/netget-ics-peers` with Python
3.10+, pip, C/C++ compilers and CMake installed. Source archives are digest checked;
OpenDNP3's CMake checks its pinned Asio/exe4cpp/ser4cpp dependencies. Use the three
printed exports in the test shell. Linux needs OpenSSL development headers for
NetGet; the peer executables do not require a proprietary license.

| Feature | Independent, unchanged stack |
|---|---|
| s7comm | python-snap7 3.2.1 |
| ethernet_ip | cpppo 5.2.5 |
| dnp3 | OpenDNP3 3.1.2, Apache-2.0, external test executable |
| iec104 | lib60870 2.3.4, GPL-3.0, external test executable only |
| bacnet | bacpypes3 0.0.102 |
| opcua | asyncua 1.1.8 |

The small Python/C/C++ harnesses configure these stacks; they do not implement
NetGet's codecs or patch peer sources. Peer setup is mandatory, with no ignored
or silently skipped tests. Native peers are separate processes, not dependencies
of NetGet's distributed binaries.

For each feature run:

```sh
cargo check --locked --no-default-features --features FEATURE --all-targets
cargo test --locked --no-default-features --features FEATURE \
  --test server --test client -- FEATURE:: --test-threads=100
```

In the shared local programme use the serialized `run_cargo.py` guard documented
in the root instructions. Server tests exercise independent requests and protocol
errors; client tests exercise the independent listening stack. Negative framing,
range/sequence checks and live owner-stop/rebind checks are in the same suites.
Selected surfaces and exclusions are documented in each protocol's source docs.
