# Wake-on-LAN Test Strategy

Two files, and the split between them is the point.

| File | What it covers | Cost |
|---|---|---|
| `decode_test.rs` | the magic-packet decoder, against literal bytes | pure function calls — no socket, no process, no LLM |
| `e2e_test.rs` | what the *server* does with the decoded result | 3 NetGet processes, **12 LLM calls total** |

Wake-on-LAN defines no response, so there is no reply to assert and the decode direction is
essentially the whole of the protocol's correctness. That is cheap to test exhaustively, so it
is tested exhaustively — and the expensive e2e suite is kept to the three things a pure
function call cannot show.

## `decode_test.rs` — 14 tests, all pure

Every packet is assembled in the test from the Magic Packet specification: six `0xFF`, then the
target MAC sixteen times, optionally with a SecureON trailer. Cases:

* a bare 102-byte packet → right MAC, `sync_offset` 0, no password, `transport: udp`;
* the payload at offsets 1, 7, 20 and 137 inside a larger datagram — **the decoding subtlety**;
  a decoder that assumes offset 0 silently drops real wake requests;
* a **false sync stream** (six `0xFF` not followed by sixteen repetitions) *before* the real
  packet — a decoder that gives up on the first candidate misses what follows;
* 4-byte and 6-byte SecureON passwords reported as such;
* trailers of 1, 2, 3, 5, 7, 8 and 32 bytes reported as **no** password (SecureON defines two
  lengths; claiming a password we are not sure about is worse than reporting none);
* **near-misses that must be rejected**: 15 repetitions (bare *and* padded to a full 102 bytes,
  so only the count is wrong), four wrong sync streams, and one flipped byte in repetitions 1,
  8 and 15;
* datagrams shorter than 102 bytes;
* an encapsulated Ethernet frame (14-byte header, EtherType `0x0842`) → `sync_offset` 14,
  `transport: ethernet`; and a bare `08 42` with no header in front of it → still `udp`,
  because the check is structural rather than a two-byte guess;
* `FF:FF:FF:FF:FF:FF`, where sync stream and MAC are indistinguishable — the scan must still
  terminate with the right answer;
* the MAC crosses to the model as a 17-character formatted string, never as bytes.

**These packets are not third-party evidence.** No `wakeonlan` or `etherwake` binary is
installed here and no WoL crate is a dependency, so this is the repository reading the
specification for itself — an independent reading, not an independent implementation. That is
the `dhcp` situation, and it is why the protocol is `Experimental`. See
`src/server/wol/CLAUDE.md` for what would make it `Beta`.

## `e2e_test.rs` — 3 tests, 12 LLM calls

The per-test headings below sum to 12, and this line said 11 until the arithmetic was checked
against `grep -c expect_calls` rather than against itself.

All three bind **127.0.0.1 on a high port**. Port 9 is the real Wake-on-LAN port and is
privileged, so `PrivilegedPort(9)` genuinely fires there — that declaration is protection, not
decoration, which is exactly why these tests must not use it.

### 1. `test_wol_decodes_every_magic_packet_shape_and_rejects_near_misses` — 6 calls

One server, seven datagrams: two near-misses followed by five packets that must be accepted
(offset 0; offset 20; 4-byte password; 6-byte password; encapsulated Ethernet frame).

The mock has one rule on `wol_magic_packet_received` with `expect_calls(5)`, answering via
`respond_with_actions_from_event` so it **echoes the event's own `target_mac`** — an event
carrying the wrong MAC would produce a record for the wrong MAC rather than passing quietly.
The five `(offset N, transport, password_length=N)` lines are then asserted individually.

**The near-misses are sent first, and that ordering is the assertion.** The receive loop
decodes datagrams sequentially, so by the time the fifth LLM call has landed both near-misses
have already been through the decoder — which makes `expect_calls(5)` a real statement about
them rather than a race. They also use a distinct MAC (`AA:BB:CC:DD:EE:01`) that must appear
nowhere in the output, and the "not a magic packet" line must appear, because a silent drop is
indistinguishable from the decoder never having seen them.

### 2. `test_wol_is_silent_on_the_wire_but_distinguishes_its_silences_in_the_log` — 4 calls

The core test. Four magic packets, one MAC per outcome, so first-match-wins rules can tell them
apart — `and_event_data_contains("target_mac", …)` on three of them and **no rule at all** for
the fourth, so the mock answers 500 and the LLM call genuinely fails.

| MAC | mock answer | required log |
|---|---|---|
| `00:11:22:00:00:01` | `[]` | `decision=model_silent` |
| `00:11:22:00:00:02` | `ignore_magic_packet` | `decision=model_reject` |
| `00:11:22:00:00:03` | `announce_host_awake` | `decision=model_accept` + **refusal** |
| `00:11:22:00:00:04` | *(no rule → 500)* | `decision=fail_closed_llm_error category=…` |

All four are **byte-identical on the wire** — silence — so the log is the only place the
distinction can exist, and each is matched with a regex anchored on both the MAC and the tag
(`{mac} from \S+ decision={tag}`): the tag alone would not prove it was *this* packet's
outcome, and the MAC alone appears on every line about the packet.

The third packet doubles as the gate test: this server was started without
`allow_non_standard_ack`, so `announce_host_awake` must be refused and nothing may be sent.

Finally, `recv_from` on a 3-second timeout proves nothing came back for any of the four.

### 3. `test_wol_sends_the_non_standard_announcement_only_when_enabled` — 2 calls

The same escape hatch with `startup_params: {"allow_non_standard_ack": true}`. The startup
warning must be logged, and the model's `message` must arrive back at the sender **verbatim**.
This exists so the refusal in test 2 is known to be the gate rather than a broken send.

## The trap this suite hit, and it is not WoL-specific

**Do not `connect()` the test socket.** A connected UDP socket discards datagrams from any
other source address, and `announce_host_awake` sends from an **ephemeral** socket rather than
from the server's own port. Test 3 failed on exactly this — the server logged
`sent a NON-STANDARD awake announcement … (28 bytes)` while the test timed out waiting for it —
and, worse, test 2's "nothing came back" assertion would have passed by filtering out precisely
the packet it exists to catch. Both use unconnected sockets with `send_to`/`recv_from`.

## Conventions

Every test finishes with `wait_for_mocks(30)` and then `verify_mocks().await?`. Waiting on the
mocks waits on the exchange: the last LLM call is the last thing these datagrams provoke.
`NetGetConfig::new_no_scripts` everywhere, so the mock rules are what answer.

## Not covered

Nothing runs on port 9, so the `PrivilegedPort(9)` preflight is exercised only by *not* firing.
Nothing produces a real EtherType `0x0842` frame — NetGet binds no raw socket, so there is
nothing to receive one with. And no machine is actually woken; NetGet is not a NIC.
