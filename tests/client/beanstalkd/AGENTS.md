# Beanstalkd client tests

Run with the existing feature:

```sh
cargo test --no-default-features --features beanstalkd --test client beanstalkd:: -- --test-threads=100
```

Use the programme's run_cargo.py wrapper during protocol expansion to serialize builds,
share artifacts and preserve free disk space. Tests bind ephemeral loopback sockets.

`real_server_test.rs` launches the independent installed beanstalkd (verified here: 1.12 and 1.13)
through RealServer, with persistence disabled and a private temporary directory. Missing
beanstalkd fails, never skips: install via `brew install beanstalkd` or the Debian/Ubuntu
`beanstalkd` package. The test drives tube selection/watch/ignore, exact UTF-8 job round
trip, reserve/touch/release/peek/reserve-job/bury/kick/delete, stats, tube lists/pause and
empty-queue/missing-job errors. RealServer owns and reaps the process group on exit.

`session_test.rs` also connects the new client to NetGet's existing Beanstalkd server using
static event handlers, verifies framing and tube state, interrupts a stalled reservation
through injected disconnect, and delivers a fragmented final response even after EOF.
`wire_test.rs` checks numeric/size/injection validation, response correlation, byte counts,
CRLF, EOF and non-UTF8 rejection, as well as flat YAML shape, aliases and entry bounds.
Legacy 1.12 unquoted Linux/Darwin uname strings, numeric/boolean-shaped tube names,
quoted 1.13 text and blank rows within counted bodies have literal regressions.
Failure diagnostics report client status and the last injected action.

All handlers in these tests are deterministic. The LLM endpoint is deliberately unreachable;
no test needs a live model or silent skip. No pcap/fuzz claim is made.

The scalar parser regression includes 512-level nesting, same-value alias expansion and
recursive aliases. Server peer tests use greenstalk 2.1.1: `python3 -m pip install
--target /tmp/netget-python-peers --no-cache-dir greenstalk==2.1.1`, then set
`PYTHONPATH=/tmp/netget-python-peers` for the existing server suite. Combined validation:
9 client tests and 30 existing server tests passed, with no ignored or skipped cases.

The v1.12 independent peer was built uninstalled from
https://codeload.github.com/beanstalkd/beanstalkd/tar.gz/refs/tags/v1.12
(SHA256 f43a7ea7f71db896338224b32f5e534951a976f13b7ef7a4fb5f5aed9f57883f)
using `make -j4` in an owned temporary directory. Prefix its directory on PATH
for the same suite. The original lifecycle test reproduced the published Ubuntu 1.12
failure before the fix. Local 1.12 and Homebrew 1.13 both pass after the fix.
