# Tor Relay Protocol Implementation

## Overview

A partial implementation of the Tor OR protocol: TLS 1.3, an ntor server handshake, per-circuit
AES-128-CTR, stream multiplexing out to TCP targets, and SENDME windows.

**Status**: Experimental. **Not interoperable with real Tor and not a usable relay.**

This file previously described the module as "Beta - Production-ready exit relay",
"cryptographically correct" and "production-quality", and its metadata rated it `Stable` with
`e2e_testing: Official Tor client (tor binary)`. None of that was true. What an audit found:

| Claim | Reality |
|---|---|
| Tested with the official Tor client | No Tor client appears anywhere. The one E2E test was `#[ignore]`d, sent a single zero-filled cell, and printed `✓` for *every* outcome including timeout. It also passed `base_stack: "TorRelay"`, which the registry rejects — so it could never have started a server even if it had been run. Rewritten; see Testing below. |
| Speaks OR protocol v4 | There was no link handshake at all, and the reader consumed fixed 514-byte cells only, so the first variable-length cell a real client sends desynchronised it permanently. Cells are now framed by length and VERSIONS is answered; CERTS / AUTH_CHALLENGE / NETINFO are still never sent or parsed, so a real client still cannot finish the handshake. |
| Usable by any peer | The ntor handshake mixes the relay's onion public key **B** into `secret_input`, and the relay generated a fresh one per process and never published it — only the fingerprint was logged. No peer on earth could derive the same keys. Both values are now logged at startup. |
| Forwards exit traffic | The per-stream forwarder held one `Mutex<TcpStream>` across a blocking `read()`, so `handle_data_cell` could never acquire it to write. Every exit stream deadlocked on its first RELAY/DATA cell, in the client-to-target direction. The stream is now split into owned halves with a lock each. |
| Correct cell handling | The command table was shifted by one from command 7 up, so a client's CREATE2 (10) was read as CREATED2 and dropped as "unhandled", and replies went out with the wrong command byte. Fixed. |
| Cryptographically correct | The ntor KDF took 72 bytes as `Kf\|Kb\|Df\|Db`; tor-spec 5.2.2 specifies 92 bytes as `Df\|Db\|Kf\|Kb\|KH`, so the AES keys were read out of the digest-seed region. The two direction ciphers were also swapped. Both fixed. The relay cell digest is still never computed or verified - see Known non-conformances. |
| LLM-controlled policies | The only event carrying actions was `tor_relay_cell_detected`, which was never emitted; the two events that *were* emitted declared no actions, so the model was offered no tools. All seven async actions returned "implementation in server logic" for logic that did not exist. Fixed by wiring the real events and deleting the dead actions. |

**Protocol Compliance**: partial, against tor-spec.txt
**Version**: targets OR protocol v4 cell framing (link handshake absent)

## The exit is open, and that is deliberate — say it out loud

The destination of every RELAY/BEGIN is whatever the peer wrote in the cell. There is **no exit
policy, allow-list, deny-list or network restriction of any kind**, so loopback, link-local
(including `169.254.169.254`, the cloud instance-metadata endpoint) and every RFC 1918 range are
reachable. Anyone who completes a circuit reaches whatever this host reaches: an SSRF pivot into
the operator's private network, and an exit someone else's traffic can be laundered through.
`proxy` and `socks5` reached the same conclusion and `metadata().notes` now states this in the
same terms.

It is worse than those two in one respect: **the model is not consulted at all.** BEGIN, DATA,
END, SENDME and exit target selection are decided in Rust. There is no `filter_mode` here, not
even in the weak form socks5 has.

Two bounds do apply, and both are on *resources*, never on destination:

- **A BEGIN's connect is abandoned after 10s** (`stream::TARGET_CONNECT_TIMEOUT`).
  `handle_begin_cell` is awaited inline in the cell loop, so until it returns the session reads
  no further cells and writes none of the ones its forwarder tasks have queued. Unbounded, that
  was the OS connect timeout — around 75s on macOS — against an address the peer chose, so one
  BEGIN to a blackholed address froze the whole connection.
- **A circuit may hold at most 64 streams** (`stream::MAX_STREAMS_PER_CIRCUIT`). The stream id
  is 16 bits, so without a cap one circuit could ask for 65,535 outbound TCP connections:
  file-descriptor exhaustion here, and pointed at one victim an outbound connection flood.
  Past the cap the peer gets `END`/`RESOURCE_LIMIT` for that stream and the circuit survives —
  propagating the error would take the connection down through the DESTROY path, handing a peer
  that opens too many streams a way to kill everything else on the same circuit.

**Do not expose this to an untrusted network.**

## Library Choices

### Cryptography Stack

- **x25519-dalek** (v2.0) - Curve25519 DH for ntor handshake (ephemeral and onion keys)
- **ed25519-dalek** (v2.1) - Ed25519 identity keys and signing
- **sha2** (v0.10) - SHA-256 for digests and key derivation
- **hmac** (v0.12) - HMAC-SHA256 for ntor authentication
- **hkdf** (v0.12) - HKDF-SHA256 for key expansion (**92**-byte key material: Df 20 + Db 20 +
  Kf 16 + Kb 16 + KH 20. This file said 72 for a long time, which is the pre-fix figure and is
  contradicted by `circuit.rs`'s own `let mut okm = [0u8; 92];`)
- **aes** (v0.8) - AES-128 cipher for relay cell encryption
- **ctr** (v0.9) - CTR mode for stream cipher (AES-128-CTR)

**Rationale**: These crates provide specification-compliant cryptographic primitives. AES-CTR is fast and the ntor
handshake is proven secure.

### Protocol Implementation

- **tokio-rustls** (v0.26) - TLS 1.3 for OR protocol connections (required by Tor)
- **rcgen** (v0.14) - Self-signed certificate generation for relay identity

**`tor-cell` is NOT used.** It is a declared dependency of the `tor` feature (at 0.36, not the
0.34 this file claimed) and `grep -rn "tor_cell::" src/` returns nothing. Every cell is framed
and parsed by this module's own `take_cell` and `parse_tor_cell`. That is worth knowing before
reasoning about spec conformance: nothing here inherits any correctness from the Tor project's
own crate.

**Rationale**: `tokio-rustls` provides async TLS with proper security.

### Manual Implementation

- **Circuit crypto state** - Custom implementation of AES-CTR cipher state per circuit
- **Stream manager** - Custom HashMap-based stream multiplexing
- **Flow control** - Custom SENDME window tracking (circuit + stream level)
- **Cell encryption** - Custom AES-CTR encrypt/decrypt. **No digest computation** - see the
  known non-conformance below; the running digests exist but nothing consumes them.

**Rationale**: No existing library combines circuit crypto + stream management + flow control. Manual implementation
allows exact spec compliance and LLM integration points.

## Architecture Decisions

### 1. Cryptographic Correctness

**ntor Handshake** (tor-spec.txt section 5.1.4):

- Client sends: CREATE2 with X (32-byte Curve25519 public key)
- Server generates ephemeral keypair Y, computes shared secret
- Derives 92 bytes of key material using HKDF-SHA256
- Returns: CREATED2 with Y (32 bytes) + AUTH (32 bytes)
- Both sides derive: Kf (forward key), Kb (backward key), Df (forward digest), Db (backward digest)

**Relay Cell Encryption** (tor-spec.txt section 6.1):

- AES-128-CTR with separate forward/backward keys
- **No digest**, and this is a KNOWN NON-CONFORMANCE (`circuit.rs` says so at the call site):
  tor-spec wants a running SHA-1 over relay cells, outbound digest fields ship as zero and
  inbound digests are never checked. A peer that validates them rejects every cell.
- Zero IV for CTR mode (standard for Tor)
- 509-byte payload per cell

**Flow Control** (tor-spec.txt section 7.4):

- Circuit-level: 1000 cell start window, 100 increment, SENDME every 100 cells
- Stream-level: 500 cell start window, 50 increment, SENDME every 50 cells
- Package window prevents sending too many cells
- Deliver window tracks received cells

### 2. Circuit Management (circuit.rs)

**Per-Circuit State**:

- Circuit ID (4 bytes, big-endian)
- Forward/backward AES-128-CTR ciphers
- Forward/backward SHA-256 digest state
- Stream manager (HashMap of active streams)
- Flow control windows (package/deliver)
- Bandwidth tracking (bytes sent/received)
- Activity timestamps

**Circuit Lifecycle**:

1. CREATE2 → ntor handshake → CREATED2 (circuit established)
2. RELAY/BEGIN → create stream → RELAY/CONNECTED
3. RELAY/DATA → forward to TCP destination
4. RELAY/END → close stream
5. DESTROY → tear down circuit

### 3. Stream Management (stream.rs)

**Per-Stream State**:

- Stream ID (u16, unique within circuit)
- Target address (host:port format)
- TCP connection (Arc<Mutex<TcpStream>>)
- Flow control windows (package/deliver)
- DATA cell counter for SENDME triggering
- Bytes sent/received counters

**Stream States**:

- **Connecting** - BEGIN cell received, establishing TCP connection
- **Active** - TCP connected, forwarding data bidirectionally
- **Directory** - BEGIN_DIR cell received, serving directory documents over circuit (NEW)
- **Closing** - END cell sent/received, closing connection
- **Closed** - Stream fully closed

### 4. BEGIN_DIR Support (Directory Serving Over Circuits)

BEGIN_DIR streams are accepted and answered with a **hardcoded, unsigned, fake consensus**
listing four 127.0.0.x relays (`generate_test_consensus`). The `directory-signature` is a run of
zeroes. Arti rejects it, as the status list below already admits. This is also the one place the
module stores protocol data in Rust rather than letting the LLM supply it, which the project's
"protocols must not implement storage" rule says it should not do.

**Why BEGIN_DIR**:

- Arti's `FallbackDir` requires OR protocol (not HTTP) for bootstrap
- Directory documents should be served OVER Tor circuits, not plain HTTP
- This matches real Tor architecture (directory authorities serve via OR + BEGIN_DIR)

**Implementation**:

1. **BEGIN_DIR Cell Handler** (`handle_begin_dir_cell`):
   - Creates directory stream (no TCP connection)
   - Responds with CONNECTED cell (like normal BEGIN)
   - Stream enters `Directory` state

2. **Directory Stream Type**:
   - Special `StreamState::Directory` variant
   - Buffers HTTP request data instead of forwarding to TCP
   - Accumulates request until complete (`\r\n\r\n` terminator)

3. **HTTP Request Parsing** (`handle_directory_data`):
   - Detects directory streams in `handle_data_cell`
   - Routes to `handle_directory_data` instead of TCP forwarder
   - Parses HTTP method and path from accumulated data
   - Checks for complete request (ends with `\r\n\r\n`)

4. **Consensus Generation** (`generate_test_consensus`):
   - Serves minimal but valid Tor consensus document
   - 4 relay entries (127.0.0.1-4 for testing)
   - Dynamic timestamps (valid-after, fresh-until, valid-until)
   - Proper HTTP response with Content-Length
   - TODO: Add LLM control for dynamic consensus generation

5. **Response Sending** (`send_directory_response`):
   - Chunks consensus into multiple DATA cells (max 498 bytes per cell)
   - Encrypts each cell with circuit crypto
   - Sends END cell after complete response
   - Properly handles flow control windows

**Consensus Format**:

```
HTTP/1.0 200 OK
Content-Type: text/plain
Content-Length: <len>

network-status-version 3
vote-status consensus
consensus-method 35
valid-after <timestamp>
fresh-until <timestamp>
valid-until <timestamp>
...
r TestRelay1 <base64-fingerprint> <IP> <ORPort> <IP> <DirPort>
s Exit Fast Guard HSDir Running Stable V2Dir Valid
v Tor 0.4.8.0
w Bandwidth=5000
p accept 1-65535
...
directory-footer
bandwidth-weights ...
directory-signature ...
```

**Status**. The ✅ marks this section used to carry had no evidence behind them: nothing in
`tests/server/tor_relay/` exercises BEGIN_DIR or the consensus at all (`grep -i begin_dir
tests/server/tor_relay/` finds only this file). The code exists - `handle_begin_dir_cell`,
`handle_directory_data`, `generate_test_consensus`, `send_directory_response` - and has never
been shown to work.

- ⚠️ Consensus content is hardcoded in Rust, not LLM-supplied
- ❓ BEGIN_DIR handling, HTTP request parsing and consensus serving are **implemented and
  untested**
- ✅ Circuit creation succeeds - this one *is* covered, by `e2e_test.rs`
- ❌ Arti bootstrap still fails (likely signature validation)

**Arti Integration**:

Tor client can now use `directory_server` startup parameter:

```json
{
  "type": "open_client",
  "protocol": "Tor",
  "remote_addr": "example.com:80",
  "startup_params": {
    "directory_server": "127.0.0.1:9001"
  }
}
```

This configures Arti to use localhost relay as FallbackDir instead of real Tor network.

### 5. Bidirectional Data Forwarding

**Architecture**:

```
Client → TLS → Decrypt → RELAY/DATA → TCP Destination
                ↓                         ↑
         Circuit Crypto State      Forwarder Task
                ↓                         ↓
TCP Destination ← RELAY/DATA ← Encrypt ← Channel
```

**Channel-Based Design**:

- Each stream spawns a background forwarder task
- Forwarder reads from TCP, builds RELAY/DATA cells, encrypts, sends via channel
- Main session loop receives from channel and writes to TLS stream
- Concurrent processing with `tokio::select!` for TLS read/write

**Benefits**:

- Non-blocking: main loop doesn't block on TCP reads
- Concurrent streams: multiple streams forward data simultaneously
- Graceful shutdown: forwarder task sends RELAY/END on TCP close

### 5. SENDME Flow Control

**Circuit-Level**:

- Track RELAY cells received (all streams)
- Send circuit-level SENDME every 100 cells (stream_id = 0)
- Increment package window by 100 on receiving SENDME

**Stream-Level**:

- Track RELAY/DATA cells received per stream
- Send stream-level SENDME every 50 DATA cells
- Increment package window by 50 on receiving SENDME

**Window Enforcement**:

- Check package_window > 0 before sending DATA
- Decrement deliver_window on receiving DATA
- Prevents circuit overload and backpressure

### 6. Statistics Tracking

**Per-Circuit Stats**:

- Circuit ID, created_at, last_activity
- Bytes sent/received (entire circuit)
- Active stream count

**Aggregate Relay Stats**:

- Total circuits, total streams
- Total bytes sent/received (all circuits)
- Per-circuit stats array

**Access**: `CircuitManager::get_relay_stats()` returns snapshot

## LLM Integration

The LLM is a bystander on the data path. Everything that matters - the handshake, BEGIN, DATA,
END, SENDME, which TCP address a stream connects to - is decided in Rust. The model sees two
events after the fact and can log, tear down a circuit, or hang up.

**Events** (both carry the action list; `call_llm` advertises `EventType::actions`, *not*
`get_sync_actions()`, so an event without them offers the model nothing):

1. `tor_relay_circuit_created` - a CREATE2 completed the ntor handshake
2. `tor_relay_relay_cell` - a RELAY command the relay does not implement (EXTEND, TRUNCATE,
   RESOLVE, DROP, unknown). BEGIN, BEGIN_DIR, DATA, END and SENDME never reach the model.

**Actions**: `detect_relay_cell` (log only), `tor_relay_log` (log only, writes nothing to the
wire), `send_destroy` (needs `circuit_id`; emits a real 514-byte DESTROY cell),
`close_connection`. Four, not three.

**No async actions.** Seven were declared - `set_relay_type`, `configure_exit_policy`,
`list_active_circuits`, `disconnect_circuit`, `list_active_streams`, `close_stream`,
`get_relay_statistics` - and all seven returned a Custom result reading "implementation in server
logic" for server logic that never existed. Relay state lives in the per-server `CircuitManager`,
which the synchronous `execute_action` cannot reach and could not await if it could. They were
removed rather than left advertised to the model.

**When the LLM call fails**, `tor_relay_relay_cell` answers with a **DESTROY cell (tor-spec
5.4) carrying reason 2 INTERNAL** for that circuit, and drops the circuit's own state so the
cell and the relay's belief agree. That branch is the only answer the peer was ever going to
get — the commands reaching it are the ones this relay does not implement in Rust — and
`if let Ok(execution_result) = call_llm(...)` used to discard it, leaving a circuit that had
accepted a cell and would never speak again. DESTROY is the right vocabulary because it needs
no relay-cell encryption (so it survives a circuit whose crypto is the problem) and cannot be
mistaken for the EXTENDED/RESOLVED/TRUNCATED the peer asked for. There is no retryable cell, so
an overload is answered identically and distinguished only in the log.
`tor_relay_circuit_created` already handled its error and still sends CREATED2, which is
correct: the handshake succeeded in Rust and the model was only being told about it.

**Exit policy is not enforced.** `configure_exit_policy` was one of the dead actions, and
`handle_begin_cell` connects to whatever address the BEGIN cell names. Any peer that establishes
a circuit can open a TCP stream to anything this host can reach.

**Scripting**: not applicable - the relay path is deterministic.

## Connection Management

**TLS Connection**:

- Server generates self-signed certificate with `rcgen`
- TLS 1.3 required (configured with `tokio-rustls`)
- TLS stream split into read/write halves
- Read half processes incoming cells
- Write half sends responses + forwarder channel

**Circuit Manager**:

- Shared across all TLS connections (Arc<CircuitManager>)
- Circuits indexed by CircuitId in HashMap
- Circuits can span multiple TLS connections (not yet implemented)

**Connection Tracking**:

- Connections tracked in AppState (connection_id, remote_addr, local_addr)
- Bytes sent/received tracked per circuit, not per connection
- Packet stats not tracked (cell-based protocol)

## Limitations

### Known non-conformances (why real Tor cannot talk to this)

1. **The link handshake stops after VERSIONS.** tor-spec 4: a connection opens with VERSIONS,
   then CERTS, AUTH_CHALLENGE and NETINFO. VERSIONS is now framed and answered with link
   version 4; the other three are still neither sent nor parsed, so a real client gets one
   cell further than before and then gives up. This alone is still fatal for real Tor.
2. ~~**Variable-length cells desynchronise the reader.**~~ Fixed. `take_cell` frames by
   length: VERSIONS (command 7, 2-byte circuit id) and commands 128+ read their own length
   field, everything else is a 514-byte fixed cell. The `select!` read is also
   cancellation-safe now — it was `read_exact`, which discards a partial cell when the
   outgoing-cell branch wins, desynchronising the connection at random.
3. **Relay cell digest is never computed or verified.** Outbound cells ship a zero digest
   field and inbound digests are ignored, so the tor-spec 6.1 `recognized` check does not
   happen. Conforming needs a running SHA-1; no SHA-1 dependency is available to this crate,
   so this cannot be fixed here without a `Cargo.toml` change.
4. **No EXTEND/EXTENDED**, so a circuit can only ever be single-hop. There is no middle-relay
   or multi-hop behaviour, which is most of what a Tor relay is for.
5. No TRUNCATE/TRUNCATED, no RESOLVE/RESOLVED, no onion services, no circuit padding.
6. No relay flags, no exit policy enforcement, no bandwidth limiting, no circuit timeouts.
7. Circuits are keyed globally rather than per-connection, so two TLS connections that pick
   the same circuit ID collide.

### Current Capabilities

- Accepts CREATE2, BEGIN, BEGIN_DIR, DATA, END, SENDME from a peer that speaks this module's
  dialect (i.e. one that skips the link handshake and ignores cell digests)
- Bidirectional data forwarding to arbitrary TCP destinations
- Specification-compliant cryptography and flow control
- Real-time statistics and monitoring
- TLS 1.3 OR protocol connections

### Known Issues

- No exit policy filtering (allows all destinations)
- No bandwidth limiting
- No circuit timeout enforcement
- No SENDME version negotiation (assumes v1)

## Example Prompts

### Start an exit relay

```
Start a Tor exit relay on port 9001 that allows connections to localhost
```

The three prompts that used to follow - "show me all active circuits", "close circuit
0x00000005", "show me relay statistics" - asked for the seven async actions this same file
records as deleted. The model cannot do any of them, and there is no verb that would let it.

## References

- [Tor Specification](https://spec.torproject.org/) (tor-spec.txt)
- [ntor Handshake](https://spec.torproject.org/tor-spec/create-created-cells.html)
- [Relay Cells](https://spec.torproject.org/tor-spec/relay-cells.html)
- [Flow Control](https://spec.torproject.org/tor-spec/flow-control.html)
- [Arti Project](https://gitlab.torproject.org/tpo/core/arti) (Rust Tor client reference)

## Testing

`tests/server/tor_relay/e2e_test.rs` runs (no `#[ignore]`) and drives a Tor client written
from tor-spec **in the test file**, not by calling into `circuit.rs`. That independence is the
point: the client recomputes the server's AUTH value and derives Kf/Kb itself, so any
divergence in the ntor handshake, the 92-byte KDF layout, the forward/backward key assignment
or the cell layout fails the test.

It covers, in one connection and 2 LLM calls:

1. VERSIONS (11 bytes, 2-byte circuit id) → VERSIONS reply offering link version 4
2. CREATE2 → CREATED2, with the client verifying AUTH and deriving the circuit keys
3. RELAY/BEGIN to a localhost HTTP server → RELAY/CONNECTED
4. RELAY/DATA carrying an HTTP request out, and the response back, decrypted with Kb and
   asserted against the body the HTTP server sent

Not covered, and the reason the rating stays `Experimental`: no real `tor` or Arti binary
(they cannot get past the missing CERTS/AUTH_CHALLENGE/NETINFO), no cell digests, no EXTEND,
no multi-hop.

Order of work to make a `Beta` rating meaningful: finish the link handshake, then cell
digests, then a real `tor` or Arti client in the E2E suite, then EXTEND.
