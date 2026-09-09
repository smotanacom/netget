# DataLink Client Implementation

## Overview

The DataLink client provides LLM-controlled raw Ethernet frame injection and capture at Layer 2 (Data Link layer). This
enables custom protocol testing, ARP spoofing detection, network monitoring, and Ethernet frame analysis.

## Architecture

### Library Choice

**Primary Library:** `pcap` crate (v2.2)

- **Rationale:** Industry-standard libpcap wrapper for packet capture/injection
- **Capabilities:**
    - Raw frame injection via `sendpacket()`
    - Promiscuous mode frame capture
    - BPF (Berkeley Packet Filter) filtering
    - Cross-platform (Linux, macOS, Windows)
- **Limitations:**
    - Requires root/CAP_NET_RAW privileges
    - Blocking I/O (requires `spawn_blocking`)
    - Platform-specific behavior differences

### Connection Model

Unlike TCP/UDP clients, the DataLink client:

1. **Interface-based**: Opens a network interface (e.g., `eth0`, `en0`) instead of connecting to a remote address
2. **Bidirectional**: Can both inject frames and capture frames in promiscuous mode
3. **Blocking Operations**: pcap is blocking, so we use `tokio::spawn_blocking` for the capture/injection loop
4. **Channel-based Injection**: Uses `mpsc::unbounded_channel` to send injection commands from async context to blocking
   pcap thread

### Lifecycle: `connect()` awaits the capture handle

`connect_with_llm_actions` does **not** return until the blocking pcap task has reported, over a
oneshot, that it found the device and opened a handle. A failure — no such interface, or no BPF
access — comes back as `Err`, and `client_startup` puts the client in `ClientStatus::Error`.

This was the capture family's signature bug and this client had it: the pcap work was
fire-and-forget, so `connect` returned `Ok` before anything was known, and on a host without
capture access the client sat in `Connected` unable to inject a byte. `[ send ]` was live and
answered every frame with an explanation. `datalink_client_refuses_an_interface_it_cannot_open`
and `connect_outcome_on_loopback_matches_capture_privilege` assert both branches, and the second
has teeth on either kind of host.

### Shutdown: the pcap loop can be stopped

`JoinHandle::abort()` cannot interrupt a thread parked in `next_packet()`, so the loop polls a
[`StopSignal`](../../utils/shutdown.rs) it gets through `register_client_task` — the same
mechanism the four capture *servers* use. `remove_client` aborts that parked task, the guard
trips the flag, and the loop exits within one poll (≤10ms in injection-only mode, ≤100ms while
in `next_packet`). An injected or model-issued `disconnect` trips it directly.

Before this the loop had no exit at all. It held the capture handle after the client was gone
and kept calling the model, and it did something worse in tests: the runtime could not shut
down, because `BlockingPool::shutdown` waits for blocking tasks and this one was
`loop { … thread::sleep(10ms) }`. `injected_frame_is_transmitted` did not fail, it **hung the
whole test binary** — which is why nobody had noticed the loop was unstoppable.

### State Management

- **ConnectionState**: `Idle` / `Processing` — one model turn per client at a time
- **ClientData**: that state, plus a **count** of frames dropped because a turn was running.
  It used to hold a `Vec<Vec<u8>>` of those frames that was only ever `clear()`ed — unbounded
  growth (up to 64 KiB a frame) in exchange for nothing — and a second copy of the client's
  memory that diverged from the one in `AppState` every other path reads.
- **InjectionCommand**: channel message for a frame injection, carrying the frame, an optional
  acknowledgement channel, and `llm_depth` (see **Bounds** below)

### Dual Mode Operation

1. **Injection-only Mode** (`promiscuous: false`):
    - LLM can inject frames via `inject_frame` action
    - No frame capture
    - Suitable for ARP spoofing tests, custom protocol injection

2. **Capture + Injection Mode** (`promiscuous: true`):
    - Captures all frames on interface
    - LLM analyzes captured frames
    - LLM can inject response frames
    - Requires root/CAP_NET_RAW for promiscuous mode

## LLM Integration

### Events

The full list, what raises each, and the depth rules are under **Events, and the chain between
them** at the foot of this file. Both frame-carrying events use the same payload, built by the
public, pure `frame_event_fields()`:

| field | meaning |
|---|---|
| `frame_hex` | hex of the **first 2048 bytes** (`MAX_HEX_BYTES_TO_MODEL`) |
| `frame_length` | the frame's true length, always |
| `captured_length` | how many bytes `frame_hex` covers |
| `truncated` | whether `frame_hex` is a prefix |

A frame can be 65535 bytes and all of it as hex is 131070 characters of prompt. The length and
the prefix are reported separately so a model is never told a 9000-byte frame was 2048 bytes
long. `tests/client/datalink/action_test.rs` asserts this against literal bytes, including the
exact boundary, and that every field the payload carries is one the events declare.

### Actions

#### Async Actions (User-triggered)

1. **inject_frame**: Inject raw Ethernet frame
   ```json
   {
     "type": "inject_frame",
     "frame_hex": "ffffffffffff001122334455080600010800060400010011223344550a0000010000000000000a000002"
   }
   ```

2. **disconnect**: Close the DataLink client and release interface
   ```json
   {
     "type": "disconnect"
   }
   ```

#### Sync Actions (Response to captured frames)

1. **inject_frame**: Inject frame in response to captured frame
    - Same parameters as async version

2. **wait_for_more**: Wait for more frames before responding
   ```json
   {
     "type": "wait_for_more"
   }
   ```

### Frame Format

The LLM must construct complete Ethernet frames:

- **Destination MAC** (6 bytes): Target MAC address (or broadcast `ffffffffffff`)
- **Source MAC** (6 bytes): Sender MAC address
- **EtherType** (2 bytes): Protocol type (e.g., `0x0806` for ARP, `0x0800` for IPv4)
- **Payload**: Protocol-specific data
- **No FCS.** `pcap::sendpacket` puts exactly the bytes it is given on the wire and the
  interface computes the frame check sequence itself, so four bytes of model-computed FCS
  would go out as payload. The `inject_frame` definition used to say "including dst MAC, src
  MAC, ethertype, payload, **FCS**"; a model that followed it corrupted every frame it sent.
  `inject_frame_does_not_ask_the_model_for_an_fcs` keeps that wording from coming back.

`execute_action` bounds the frame before libpcap sees it: at least
`MIN_ETHERNET_FRAME_BYTES` (14 — a runt has no EtherType) and at most
`MAX_ETHERNET_FRAME_BYTES` (65535), each refused with a message naming the actual length so
the model can correct it. Octet separators (`:`, `-`, ` `, `.`) are stripped before decoding,
because a model writing hex out of a packet dump writes them and they carry no information.

Example ARP request frame:

```
ff ff ff ff ff ff  // Destination MAC (broadcast)
00 11 22 33 44 55  // Source MAC
08 06              // EtherType (ARP)
00 01              // Hardware type (Ethernet)
08 00              // Protocol type (IPv4)
06                 // Hardware address length
04                 // Protocol address length
00 01              // Operation (ARP request)
00 11 22 33 44 55  // Sender MAC
0a 00 00 01        // Sender IP (10.0.0.1)
00 00 00 00 00 00  // Target MAC (unknown)
0a 00 00 02        // Target IP (10.0.0.2)
```

## Use Cases

1. **ARP Testing**: Inject ARP requests/replies for network discovery
2. **Custom L2 Protocols**: Test proprietary Ethernet protocols
3. **Network Monitoring**: Capture and analyze Ethernet traffic
4. **ARP Spoofing Detection**: Monitor for duplicate ARP replies
5. **MAC Address Analysis**: Track MAC address usage on network
6. **Frame Timing Analysis**: Measure frame arrival times

### Dashboard injection (`[ send ]`)

`connect_with_llm_actions` registers a command channel
(`client::command_support::register_command_channel`) before the pcap task starts and spawns
a `command_loop` task (registered with `register_client_task`), so `[ send ]` is live from
the moment the client exists — including while the connected-event call below is parked on a
manual routing rule.

There is no `AsyncWrite` half here, so the generic `handle_stream_client_command` cannot be
used. An injected `inject_frame` goes through the protocol's own `execute_action` and lands
in the same `InjectionCommand` queue the LLM path uses. That command now carries an optional
`oneshot` acknowledgement, which the pcap loop fills with the result of `sendpacket` - that
is what makes a truthful outcome possible:

| Outcome | When |
|---|---|
| `Sent { bytes_sent }` | the pcap loop acknowledged a successful `sendpacket` for that many bytes |
| `Executed { detail }` | the frame could not be handed over, or was not acknowledged - `detail` names the reason and the frame size |
| `Rejected { error }` | unknown verb, undecodable `frame_hex`, or a frame too short/too long to be Ethernet |
| `Disconnected` | an injected `disconnect` |

**There is no unprivileged path into this any more.** It used to be reachable — `connect`
returned `Ok` without a capture handle, so the dashboard offered `[ send ]` on a client that
could never send, and every injection came back `Executed` with an explanation. Now `connect`
fails instead, so the client does not exist to be sent to. The `Executed` branch survives for
the one case that remains real: the pcap loop has exited (a `disconnect`, or a capture error)
while the handle has not yet been dropped.

**Known gap**: in injection-only mode the pcap loop polls the queue every 10ms and in
promiscuous mode it can sit up to 100ms in `next_packet`, so an injected frame is put on the
wire with up to that much latency. The acknowledgement wait is bounded at 5s.

Tests: `tests/client/datalink/action_test.rs` (no privilege — the whole model-facing surface)
and `command_channel_test.rs` (the lifecycle; its privileged half asserts a real acknowledged
`sendpacket`, a real `disconnect`, and that the capture loop actually stops).

## Limitations

1. **Privilege Requirement**: layer-2 capture access — `/dev/bpf*` on macOS/BSD, root or
   `CAP_NET_RAW` on Linux. Declared as `PrivilegeRequirement::PacketCapture`, matching the
   DataLink *server*. It said `RawSockets` until this pass, which is a **different** capability
   in `SystemCapabilities`: a macOS user in the ChmodBPF group has capture and not raw sockets,
   and would have been refused a client that works on their machine.
2. **Blocking I/O**: pcap is blocking, so operations run in `spawn_blocking`
3. **No FCS**: the interface computes it; do not put one in `frame_hex`
4. **Platform Differences**: libpcap behavior varies across OS
5. **No TCP/UDP**: This is Layer 2 only - for Layer 3+ protocols, use TCP/UDP/IP clients
6. **Performance**: capture can outrun the model. One model turn runs at a time per client;
   frames that arrive during it are **dropped and counted**, not queued
7. **Dummy SocketAddr**: Returns `127.0.0.1:0` since DataLink doesn't use sockets
8. **Unproven except by privileged tests**: injection has been exercised end to end on macOS
   (2026-09-08), but only by tests that need capture access. Nothing an unprivileged run
   executes has ever put a frame on a wire. `Experimental`, and for that reason.

## Startup Parameters

- **interface** (required, string): Network interface name (e.g., `eth0`, `en0`, `wlan0`)
- **promiscuous** (optional, boolean): Enable promiscuous mode for frame capture (default: `false`)

Example:

```json
{
  "interface": "eth0",
  "promiscuous": true
}
```

## Testing Strategy

See `tests/client/datalink/CLAUDE.md` for E2E testing approach.

## Security Considerations

**CRITICAL**: Raw frame injection can disrupt networks and violate regulations. Only use on authorized test networks:

- ✅ Isolated lab environments
- ✅ Virtual networks (VMs, containers)
- ✅ Personal test setups
- ❌ Production networks
- ❌ Public networks
- ❌ Networks you don't own

ARP spoofing and MAC address manipulation can be malicious - use responsibly.

## Implementation Notes

1. **Channel-based Injection**: The async LLM code sends injection commands via `mpsc::unbounded_channel` to the
   blocking pcap thread
2. **State Machine**: Prevents concurrent LLM calls (Idle → Processing → Accumulating)
3. **Dual Logging**: Uses both `tracing` macros and `status_tx` for TUI updates
4. **Error Handling**: Injection failures are logged but don't crash the client
5. **Graceful Shutdown**: Client tracks disconnection via `ClientStatus::Disconnected`

## Future Enhancements

- BPF filtering support (filter captured frames)
- Multiple interface support
- Frame statistics (capture rate, injection rate)
- Frame timing control (scheduled injection)
- VLAN tag support
- Jumbo frame support


## Events, and the chain between them

Three, all raised by this client and all returned from `get_event_types()` as clones of the
same statics, so a declaration cannot drift from what is emitted:

- **`datalink_connected`** — the capture handle is open. This is what lets the model act on
  its instruction; before it existed the client opened the interface and asked the model
  nothing, so a client created with "inject an ARP request for 10.0.0.2" opened `lo0` and
  then sat there having done nothing at all.
- **`datalink_frame_injected`** — a frame really went out (`sendpacket` returned Ok). It was
  declared and raised nowhere, so a model that injected a frame was never told it had worked
  and could not follow it with anything.
- **`datalink_frame_captured`** — promiscuous mode only.

All three carry `.with_actions(...)` — `inject_frame` and `wait_for_more`. A client's tool list
is async ∪ sync ∪ the firing event's actions, so this is belt and braces rather than load
bearing, but an event that declares no vocabulary is a declaration that means nothing.

### Bounds

**The chain runs through the injection queue, not the stack.** `run_llm_turn` raises one event,
queues whatever frames the model asks for, and returns. Injecting raises
`datalink_frame_injected` from the pcap loop, which calls `run_llm_turn` again — so
inject → report → inject continues with no recursion to box.

**It is still a cycle, and it is bounded.** The earlier version of this note treated "no
recursion" as the end of the argument and said in as many words that the chain "continues
without any recursion to bound" — but a model that answers every injection with another
injection then loops forever at one LLM call per hop, which is precisely what the repo's
`MAX_FOLLOWUP_DEPTH` rule exists to prevent. `InjectionCommand::llm_depth` carries the hop
count through the queue: the pcap loop raises `datalink_frame_injected` only while it is below
`MAX_INJECTION_FOLLOWUPS` (4), and past that the frame still goes out, the model is simply not
asked again (`decision=followup_depth_capped`). A frame the operator injects from the dashboard
always starts a fresh chain at depth 0 — the cap is on the model's runaway, not on the human.

**One model turn per client.** A captured frame arriving while a turn is running is dropped and
counted (WARN on the first and every hundredth), never queued.

The injected-frame event is raised with `runtime.spawn` from inside the blocking pcap thread
rather than awaited there. That thread owns the libpcap handle and is the only thing that can
call `sendpacket`; blocking it on an LLM call that a manual rule can park for minutes would
stop every other injection, including the dashboard's.

### Failure semantics: nothing is injected, and the log says which nothing

DataLink is one of the deliberately silent protocols — a fabricated frame on a real network is
worse than no frame — so every outcome below looks identical on the wire and is distinguished
in the log. Grep `decision=`:

| tag | meaning |
|---|---|
| `decision=model_inject` | the model answered with at least one frame, which was queued |
| `decision=model_silent` | the model was asked and returned no actions |
| `decision=model_reject` | every action the model returned was refused by the executor |
| `decision=model_no_frame` | actions ran but none injected anything (`wait_for_more`) |
| `decision=model_disconnect` | the model closed the client |
| `decision=fail_closed_llm_error` | the LLM call errored; `category=overloaded` vs `unavailable` from `WireFailure::classify`, plus the full error |
| `decision=followup_depth_capped` | the frame went out; the chain hit `MAX_INJECTION_FOLLOWUPS` |
