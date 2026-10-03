# Gemini client tests

Run `cargo test --no-default-features --features gemini --test client gemini:: -- --test-threads=100`.
During the expansion programme, every Cargo command uses its shared `run_cargo.py` wrapper.

Independent peer: Agate 3.3.24, https://github.com/mbrubeck/agate/releases/tag/v3.3.24.
Bootstrap without installation/build caches:

```sh
sh scripts/test-peers/fetch-agate.sh /tmp/netget-agate-peer
export NETGET_TEST_AGATE=/tmp/netget-agate-peer/agate-3.3.24
```

The script pins SHA256 digests for macOS/Linux on arm64/x86_64 and leaves a roughly 1 MiB
archive plus the executable in the supplied owned directory. No binaries are committed.
Tests fail if Agate is missing. They generate private certificate/key files and content,
run Agate on loopback with private directories, and remove the owned process group even
on panic through RealServer. Agate shares the rustls primitive but none of NetGet's Gemini
framing, parsing, action, runtime or gemtext implementation.

Independent coverage: real gemtext content, repeated fresh TLS connections, 10/11 input,
31 redirect returned without following then explicit follow,44 delay,60 certificate challenge,
missing-resource failure, untrusted-certificate and hostname mismatch rejection. NetGet
pairing verifies structured gemtext, file-backed server certificate loading, fresh requests,
input percent encoding and explicit endpoint mismatch rejection.

Wire tests cover URL injection/length/userinfo/fragment/scheme/port, gemtext types, relative
links, all status classes, unknown second digits, CRLF/truncation, unsupported MIME/charset,
UTF-8 failure and header/body/line-count bounds. TLS fixtures separately cover fragmented
responses, injected disconnect during a stalled body, and removal during a stalled handshake.
Raw TLS fixtures are framing/lifecycle tests, not the independent-peer evidence.

Existing server tests use ignition-gemini1.0.0 and tshark. Ignition requires Python 3.7..3.12
and cryptography; it explicitly rejects 3.14. Use a temporary Python 3.12 venv and install
`ignition-gemini==1.0.0` there, with its interpreter first on PATH. Server tests already import
cryptography.hazmat.primitives.serialization for ignition's compatibility limitation.

Validation: all 8 client tests passed. Of 24 existing server tests, 21 passed on the
first run; the 3 ignition tests initially failed under unsupported Python 3.14, then
all passed under Python 3.12.14 with ignition 1.0.0, cryptography 50.0.2 and cffi 2.0.0.
The server pcap test passed using the installed tshark. No tests were skipped or ignored.
