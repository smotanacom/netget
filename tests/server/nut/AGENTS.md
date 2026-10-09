# NUT validation

`wire_test.rs`: RFC literal bytes, escaping, rejected injection, list correlation,
truncated EOF, line/entry/body bounds and whole-line deadline.
`e2e_test.rs`: static-handler sessions, auth deny/accept, pipelining, lifecycle shutdown,
idle timeout and the 256-peer connection cap. One mocked-model test consumes exactly
three calls (startup plus two requests), waits for expectations and verifies them.
`llm_failure_test.rs`: wire results and distinct status-log decisions for successful replies,
explicit protocol refusals, silence, invalid/duplicate reply actions, dead backend,
and authentication acceptance/denial. No credentials are included in terminal logs.
`real_client_test.rs`: official NUT upsc discovers/describes UPS devices, reads one
variable and reads a complete list; upscmd authenticates and runs an instant command.
Absence of either external binary fails. No ignore/skip gates.

Build test peers without a system installation:

```sh
sh scripts/test-peers/build-nut.sh /absolute/owned/tmp/nut-peers
export NUT_UPSC_BIN=/absolute/owned/tmp/nut-peers/nut-2.8.4/clients/upsc
export NUT_UPSCMD_BIN=/absolute/owned/tmp/nut-peers/nut-2.8.4/clients/upscmd
export NUT_UPSD_BIN=/absolute/owned/tmp/nut-peers/nut-2.8.4/server/upsd
export NUT_DUMMY_BIN=/absolute/owned/tmp/nut-peers/nut-2.8.4/drivers/dummy-ups
cargo test --no-default-features --features nut --test server --test client nut:: -- --test-threads=100
```

The build needs a C/C++ compiler, make, curl, tar and sha256sum or shasum.
The 2.8.4 archive is pinned to SHA256
`0130ba82ea79f04ba4f34c5249a85943977efd984ed7df6aec1a518d5a3594f8`. It disables TLS and hardware
libraries, builds only four CLI/server programs plus the dummy driver, does not install
anything and retains roughly 46 MiB in its owned directory. Peer version verified here:
NUT 2.8.4 release; dummy-ups driver 0.22. System NUT packages are also accepted through
PATH or the existing independent-peer helper's standard binary locations.

Tests use ephemeral loopback ports and need permission to bind sockets. During the
protocol-expansion programme, replace `cargo` with its run_cargo.py wrapper to share
and serialize build artifacts and preserve the free-space reserve. No pcap oracle.
