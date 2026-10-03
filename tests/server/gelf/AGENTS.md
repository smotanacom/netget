# GELF collector tests

Codec tests cover structured fields, required/version/type/name validation, Unicode/newlines,
all TCP split boundaries, all three UDP encodings, reverse chunks, duplicates/conflicts,
source+ID separation, expired/replayed IDs, malformed/truncated frames, gzip/zlib bombs and
trailing bytes, message/datagram/chunk/count/payload-memory/buffer limits. E2E tests cover
UDP/TCP default model suppression, static/script/manual/model routing, standard memory
across events, offending-peer isolation, receiver timestamp/level defaults, both-role pairing,
mixed valid/invalid action fail-closed decision logs, manual-stop cancellation, socket release and the absolute TCP frame deadline.

Independent pygelf 0.4.3 emits gzip/chunked UDP and NUL-framed TCP; tests require actual
structured observations. Missing peers fail. No ignored tests, optional-peer skips or
heavy Graylog service/container. The client suite uses official Graylog's independent reader.

Bootstrap isolated peers and their Go caches (72MiB measured on this macOS toolchain):

```sh
python3 tests/server/gelf/install_peers.py /tmp/netget-gelf-peers
export PYTHONPATH=/tmp/netget-gelf-peers/python
export NETGET_GELF_PYTHON=python3
export NETGET_GELF_READER=/tmp/netget-gelf-peers/gelf-reader
python3 /Users/matus/dev/netget/.protocol-expansion-20261001/run_cargo.py test --no-default-features --features gelf --test server --test client -- gelf:: --test-threads=4
```

The programme build wrapper is the authorized local build path. Bootstrap uses pinned pygelf
and exact-SHA/sha256-verified Graylog source, a standard-library-only Go build, owned GOCACHE/
GOMODCACHE, GOTOOLCHAIN=local/GOPROXY=off. Network/TLS verification stays enabled for downloads.
No fuzz or pcap evidence claimed.
