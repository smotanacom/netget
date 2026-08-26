# NetGet — LLM-Controlled Network Protocol Server & Client

Rust CLI where an LLM (Ollama or any OpenAI-compatible endpoint) drives ~116 network
protocols as servers and ~90 as clients. NetGet owns the network stack; the LLM decides
what to say on the wire, either by reasoning per-request or via deterministic handlers.

Three ways to run it: interactive TUI (default), headless (`--mcp` / `--mcp-http`, see
`src/mcp_stdio/CLAUDE.md`), and non-interactive one-shot (`src/cli/non_interactive.rs`).

**The interactive TUI is the full-screen ratatui dashboard (`src/tui/`)**; the older
rolling-terminal TUI (`src/cli/rolling_tui.rs` + `sticky_footer.rs`) is still there behind
`--legacy-tui`. Chat is on the left, unchanged in contract — `UserCommand::parse` is shared, so
every slash command still works. The right-hand rail is **one borderless tree** of every server
and client, and is the first way to **create and modify instances without the LLM**.

Everything applies through `cli::management`'s `ServerForm`/`ClientForm`/`update_*`, so
validation and the hot-apply vs restart split are identical to the LLM and MCP paths. The forms
submit only *changed* fields — re-sending an unchanged port or host reads as a change and forces
a needless restart.

**Every action is a row** (`tree::RowAction`), placed under the thing it acts on, rather than a
button somewhere: `[ edit config ]` under `config`, `[ + add handler ]` under `handlers`,
`[ + connect a … ]` and `[ message this peer ]` under `peers`, `[ disconnect ]` /
`[ connect ]` / `[ remove client ]` on a client, `[ + new server ]` / `[ + new client ]` at the
foot of the rail. Enter and a mouse click go through the same `activate_row`, so the two cannot
drift. A client's own protocol verbs are inlined as rows too (telnet's `[ send_command ]`,
`[ send_text ]`), and pressing one opens the composer already on that action's parameters;
`projection::is_initiable_action` keeps response-only verbs (`wait_for_more`) and duplicates of
the lifecycle rows (`disconnect`) out of that list, while `n` still opens the full vocabulary.

A server's live peer gets the same treatment where the protocol registered a peer handle
(`server/peer_support.rs`; `tcp` and `telnet` have): `[ message this peer ]` opens the composer
on the server's wire verbs, `[ disconnect this peer ]` runs its `close_connection` through the
same handle (half-close + marked closed at once, because a peer that never reads — our own
client parked on a manual question — would otherwise leave the row "live" after you hung up).
A protocol without a handle shows one dim row saying so instead of nothing. A peer whose request
is parked for you is flagged `⚠ waiting for your answer` on its own row; the activatable question
row stays first under the instance.

`config` and `handlers` are **collapsed by default** — settings, not traffic; `peers` is open.
The letter shortcuts still work (`a` add, `e` config, `r` handlers, `c` connect a client,
`n` compose, `x` stop/remove, F1 help), but nothing depends on knowing them — and **no modal
requires a chord**. Every button is a Tab stop: in the text editor Tab leaves the text for
`[ Accept ]` / `[ Cancel ]` (it used to insert a tab, leaving Ctrl-S as the only way out); the
form, composer and routing editors have no Ctrl-S/Ctrl-J bindings any more, only their buttons.

**Answering a parked request is the composer, not a JSON box.** The intercept modal offers three
things: `[ Compose answer… ]` opens `ComposerModel::for_intercept` — pick one of the protocol's
actions from a list, fill its parameters as fields (a `bool` parameter is a checkbox: Enter or
Space flips it), Send resolves the intercept and closes both modals; `[ Answer with nothing ]`
resolves with zero actions (acknowledge, say nothing — a real answer, distinct from a timeout);
`[ Fail closed ]` refuses. Raw JSON is still one button away inside the composer, and for an
intercept it accepts an array so a multi-action answer is possible; it is never the starting
point. The earlier "Compose actions… (JSON editor) + Send response" pair was the reported
confusion: two buttons whose difference was invisible, and a free-form JSON field nobody could
fill.

Stopping is immediate: only the bulk actions (stop all, quit) still confirm.

**`[ view in wireshark ]`** (`w`, and a `[ View in Wireshark ]` button on the create/edit form so
the capture can be running *before* the instance starts) opens a modal with a paste-ready
`wireshark -k …` / `tshark -l …` line, plus the pieces separately: interface, BPF capture filter,
display filter and a `-d` decode-as clause. NetGet writes no pcap; `src/tui/wireshark.rs` is a
pure table of NetGet protocol → transport + Wireshark dissector name, and every name in it was
checked against `tshark -d` / `-Y`. Three traps it encodes: `isis` is an Ethernet-only BPF keyword
and is rejected on loopback; `drda` (db2) is heuristic-only and not in the `tcp.port` decode-as
table; `ipp` is reached through `http`. A protocol missing from the table is treated as plain TCP
(correct for everything in the registry that is not UDP, raw or off-network), and the off-network
families (USB, BLE, NFC, pty/pipe/stdio) get an explanation of where to look instead of a command.

## Protocol inventory — always query, never trust a list

Protocol lists in docs go stale within weeks. Get ground truth from the registry:

```bash
# Every registered server protocol + maturity + implementation note
grep -n "register(Arc::new" src/protocol/server_registry.rs
grep -rn "DevelopmentState::" src/server/<protocol>/actions.rs   # that protocol's own claim

# At runtime (authoritative — reflects compiled-in features)
netget --mcp   # then call list_protocols / get_protocol_docs
```

Maturity lives in each protocol's `metadata()` (`ProtocolMetadataV2`, `src/protocol/metadata.rs`):

- **Stable** — real spec compliance, good LLM prompting, scripting support, validated against a
  real client. **Currently none.** Three protocols have held this rating and all three lost it on
  inspection for the same reason — never actually validated against a real client. `tor_relay`
  and `openvpn` went first. `wireguard` was the last, demoted August 2026: NetGet implements none
  of the WireGuard protocol itself (it orchestrates `defguard_wireguard_rs`, which needs root and,
  on macOS, an external `wireguard-go` binary), so it cannot be started or handshaked in this
  environment at all, and its Stable rating rested on a test that mocked a `wireguard_packet_received`
  event and a `log_packet` action **that do not exist**, all `#[ignore]`d behind root so the
  mismatch never surfaced. The bar for Stable is a test that a real independent peer completed a
  real exchange — treat any Stable claim without one as this same bug.

  **That demotion undershot, and the correction is the useful lesson.** `wireguard` was moved
  Stable→Beta on the grounds that it had never been validated against a real client — but that
  is *also* the definition of Beta, so the same evidence ruled Beta out and nobody noticed for
  months. It is now Experimental. When you demote for missing evidence, check which ratings that
  evidence actually supports rather than stepping down one notch by reflex.
- **Beta** — human-reviewed, works against real clients (24 protocols). The original ten are
  `dns`, `doh`, `dot`, `http`, `ntp`, `openai`, `snmp`, `tcp`, `udp`, `whois`; August 2026 added
  fourteen that are each driven by the protocol's own third-party client in a test that is **not**
  `#[ignore]`d — `amqp` (lapin), `cassandra` (scylla), `coap` (coap-lite), `imap` (async-imap),
  `ldap` (ldap3), `mongodb` (official driver), `mssql` (tiberius), `mysql` (mysql_async),
  `postgresql` (tokio-postgres), `redis` (redis-rs), `ssh` (russh), `sqs` (aws-sdk-sqs), `webdav`
  (reqwest_dav), `zookeeper` (zookeeper-async). Each protocol's `metadata()` names its client.

  Deliberately **not** promoted despite an audit suggesting them: anything whose only evidence is
  a generic HTTP client (`reqwest` proves an HTTP server answers, not that the protocol on top is
  right — `couchdb`, `etcd`, `oci_registry`, `kubernetes`, `openapi`, `spark`, `xmlrpc`, `yarn`,
  `git`, `jsonrpc`, `oauth2`, `saml_sp`, `proxy`, `http2`); anything with no independent peer at
  all (`memcached`, `modbus`, `named_pipe`, `openvpn`, `pty`, `radius`, `socket_file`, `stdio`,
  `websocket`); `mqtt`, whose rumqttc tests are all `#[ignore]`d; and `rss`, whose test currently
  fails. `dhcp` and `wireguard` were removed for the same reason — neither has a third-party
  peer. dhcp's own metadata says no real DHCP client can be pointed at it (dhclient/ipconfig bind
  UDP/68, need root, and cannot target an ephemeral loopback port), so its peer is an in-test RFC
  2131 decoder: an independent reading of the spec, but not an independent implementation.

  Re-derive this list rather than trusting it; the counts drift.
- **Experimental** — LLM-authored or newly implemented, not fully reviewed. The overwhelming
  majority (~99).
- **Incomplete** — hidden from the LLM entirely (`is_available_to_llm()` returns false). **None
  remain.** The last one, `bluetooth_ble_beacon`, was a platform limit rather than unfinished
  work, and was resolved by making the platform explicit rather than by hiding the protocol:
  - A beacon *is* its advertising payload, and `CBPeripheralManager.startAdvertising:` accepts
    only `CBAdvertisementDataLocalNameKey` and `CBAdvertisementDataServiceUUIDsKey`; every other
    key is documented as ignored, so macOS cannot emit one and no CoreBluetooth binding would
    change that. Linux/BlueZ can, via `org.bluez.LEAdvertisement1`'s
    `ManufacturerData`/`ServiceData` registered on `org.bluez.LEAdvertisingManager1`.
  - It is now `Experimental`, implemented on Linux with `bluer` (already in the tree as
    `ble-peripheral-rust`'s Linux backend, so no second D-Bus stack), and `spawn()` returns a
    clear `Err` naming the reason on every other platform. **Hiding a protocol is not the same
    as refusing to start it**: hidden, the model never learns why; refused, the user gets
    `ServerStatus::Error` with the CoreBluetooth key that makes it impossible.
  - Its payload construction is pure and exhaustively unit-tested against literal spec bytes;
    its BlueZ transport has never been compiled or run on Linux. `metadata().notes` says both,
    which is what `Experimental` is for.

Twelve protocols left `Incomplete` in August 2026 — `amqp`, `bgp`, `kafka`, `nfc`, `openvpn`,
`turn`, `usb/serial`, `usb/smartcard`, `vnc`, `webdav`, `webrtc`, `zookeeper`. Each was verified
against a **real client** where one exists (`lapin`, `reqwest_dav`, `zookeeper-async`, `webrtc-rs`
as a peer, a real `openvpn` 2.7.4 binary, two UDP sockets proving TURN relays a payload), and
against an **independent codec or RFC-derived literal bytes** where none does (BGP via
`netgauze`, Kafka via `kafka-protocol`'s client-side codecs). Each `metadata()` note says which,
and what is still untested.

Derive these rather than trusting the counts, which drift. **Match the fully-qualified form
too** — roughly half the protocols write `crate::protocol::metadata::DevelopmentState::X`, and a
pattern anchored on the bare `DevelopmentState::` silently reports them as declaring nothing.
An earlier version of this section claimed four USB protocols declared no state for exactly that
reason; all four declare one:

```bash
python3 - <<'EOF'
import re, pathlib
from collections import Counter
rows=[]
for f in sorted(list(pathlib.Path('src/server').glob('*/actions.rs'))
              + list(pathlib.Path('src/server').glob('*/*/actions.rs'))):
    m=re.search(r'\.state\(\s*(?:crate::protocol::metadata::)?DevelopmentState::([A-Za-z]+)',
                f.read_text(errors='ignore'))
    rows.append((m.group(1) if m else 'NONE', str(f)))
print(Counter(r[0] for r in rows))
EOF
```

Treat `Experimental` as "compiles and has a test", not "works". Before assuming a protocol
behaves, check what it actually offers the model — and check it two ways, because one grep
lies in both directions:

```bash
grep -A3 "fn get_sync_actions" src/server/<p>/actions.rs | grep -c 'vec!\[\]'  # hollow?
grep -cE "DnsProtocol|BluetoothBle::" src/server/<p>/actions.rs                # or delegating?
grep -c "\.with_actions(" src/server/<p>/actions.rs                            # reachable at all?
```

An empty `get_sync_actions()` is fine if the protocol **delegates** — `doh` and `dot` forward
`DnsProtocol`'s set verbatim, so the model sees the full DNS vocabulary. It is a trap if it
does not. And actions can be declared yet unreachable: `call_llm` builds the model's tool list
from `event.event_type.actions`, so a protocol whose event types never call `.with_actions(...)`
leaves the model unable to answer at all. That was found in 17 protocols, is now fixed
everywhere, and is guarded two ways: `EventType::with_no_actions()` marks the deliberate case,
anything else logs at ERROR and falls back, and `tests/event_action_declarations_test.rs` fails
the build on any new occurrence across every registered protocol.

**That guard had a hole worth remembering, because it inverted the whole point.**
`audit_event_action_declarations` early-returned when `get_sync_actions()` was empty, on the
reasoning that a protocol advertising nothing is withholding nothing. It is backwards: offering
the model *no* vocabulary is the worst case, not the exempt one. `usb-fido2` had zero sync
actions, zero LLM integration and three events the model could not answer, and
`cargo test --features usb-fido2,tcp` reported **3 passed**. The rule now fails that case; re-run
at `--all-features` it flagged exactly `usb-fido2` and produced no false positives across the
other 115, and delegation is unaffected because `doh`/`dot` return a non-empty set of their own.
The `registry-audit` CI job runs these audits at `--all-features` for that reason — every other
job compiles 6 of 116, so a registry-walking test is only as wide as its feature set.

**A fourth variant: an event can be declared and never emitted.** Declaring actions on it then
buys nothing, because it never fires. The USB family was the live case and is now fixed —
`usb_*_detached`, `usb_msc_read`, `usb_msc_write`, `usb_keyboard_led_status` and the
`usb_serial_*` events all have real emit sites. Two of those repairs are worth knowing about,
because the protocols were more broken than "an event does not fire": `usb-mouse`'s
`handle_connection` took the socket as `_stream` and dropped it, ran no USB/IP session at all and
parked on `sleep(u64::MAX)`, while a complete handler sat unwired in `handler.rs`; and every
`usb-keyboard` action demanded `connection_id.as_u64()` while events report `"conn-2"`, so no
value the model could send would ever have worked. Check the emit side, not just the declaration:

```bash
grep -rn "_EVENT" src/server/<p>/mod.rs   # which events does the server actually raise?
```

**A fifth variant, on the client side, and it was the widest of all: `call_llm_for_client` built
its tool list from `get_async_actions()` alone.** It read neither `get_sync_actions()` nor
`event.event_type.actions`. **53 of the 91 registered clients** had at least one action the model
could never see — 11 hiding something protocol-specific (`irc`'s send_privmsg/notice/raw, `nfc`'s
send_apdu, `icmp`'s send_echo_request, whose async list was empty outright) and 42 hiding
`wait_for_more`, so no stream client could say "that response was partial". TFTP declared
`send_ack` sync-only, so every DATA block came back `Unknown Action` and every transfer stalled
at block 1.

**Clients union; servers narrow — and that asymmetry is deliberate.** A server has two LLM entry
points, so its async/sync split is real and `ssh_auth` can legitimately narrow to
`ssh_auth_decision`. A client has one: `call_llm_for_client` serves both the initial instruction
(`event: None`) and every network event, so no client *can* express a narrowing. Hence
`client_llm_action_set(...)` = async ∪ sync ∪ the firing event's actions. The tree confirms the
split was never meaningful there — 85 of 91 clients attach no actions to any event type, and ~40
duplicated their whole list into both methods purely to work around this.

`tests/event_action_declarations_test.rs` now walks **both** registries, and additionally
round-trips each advertised name through the protocol's own executor — which caught a sixth
variant, *advertised but unexecutable* (`ssh_agent/modify_instruction`,
`pop3/modify_pop3_instruction`, and four more that were removed as unimplementable). A static
declaration check cannot find those.

The whole `bluetooth_ble_*` family was a third variant: `BluetoothBle::spawn_with_llm_actions`
hardcodes `BluetoothBleProtocol` when it calls `call_llm`, so all sixteen profiles' own actions
and events were unreachable regardless of what they declared. Fifteen now delegate explicitly.
`bluetooth_ble_beacon` took the other exit: it no longer goes through the base at all, so it
calls `call_llm` with its own protocol and its own actions really are the ones offered and
executed. That is the only way a profile can own a vocabulary — delegate the base's, or stop
using the base.

Per-protocol docs: `src/server/<protocol>/CLAUDE.md` and `tests/server/<protocol>/CLAUDE.md`.
**Read both before modifying a protocol.** Note that these files are frequently more
aspirational than the code — verify claims against the source.

## Architecture

**Modules**: `cli/` (TUI, startup, args) · `server/<protocol>/` · `client/<protocol>/` ·
`protocol/` (registries, metadata, spawn context) · `state/` (app state) · `llm/` (backends,
prompting, actions) · `events/` (coordination) · `scripting/` (deterministic handlers) ·
`mcp_stdio/` (MCP server) · `easy/` (simplified layer for small models).

**Decentralization (CRITICAL)**: no centralized per-protocol logic. Each protocol implements
traits independently. The only legitimate central touchpoints are:

- `protocol/server_registry.rs` / `protocol/client_registry.rs` — one feature-gated `register()` line
- `Cargo.toml` — feature flag
- `src/server/mod.rs` / `src/client/mod.rs` — feature-gated `pub mod`
- `tests/server/mod.rs` / `tests/client/mod.rs` — feature-gated `pub mod` (see the footgun below)

`cli/server_startup.rs` and `cli/client_startup.rs` are **fully generic** — they look the
protocol up in the registry and call `spawn(ctx)` / `connect(ctx)`. There is no
per-protocol match statement. Do not add one.

**`ProtocolConnectionInfo`** (`state/server.rs`) is a generic `serde_json::Value` wrapper,
not an enum. Adding a protocol does not require touching it.

**Connection I/O**: split `TcpStream` with `tokio::io::split()` (never clone). Never hold a
`Mutex`/`RwLock` guard across an `.await` that performs I/O or an LLM call — acquire, copy
out what you need, drop the guard in an inner scope, then await.

**Per-connection state machine** (Idle → Processing → Accumulating) prevents concurrent LLM
calls on one connection and queues data arriving mid-call. Note: `state/machine.rs` defines a
generic `StateMachine<S>` that **nothing uses** — every protocol hand-rolls its own copy.
Copy the TCP implementation (`src/server/tcp/mod.rs`) as the reference.

**Actions**: protocols implement `ProtocolActions` (`src/llm/actions/protocol_trait.rs`) with
async actions (user-triggered) and sync actions (network-event-triggered), in
`src/server/<protocol>/actions.rs`. Clients implement `Client` (`llm/actions/client_trait.rs`).

**Handling modes** — a matched `event_handlers` rule decides who answers:
1. **Script handler** — inline Python/JS, runs in-process, no LLM call
2. **Static handler** — fixed actions, no LLM call
3. **Manual handler** — the event parks (`src/state/intercepts.rs`) and a **human** composes
   the answer at the dashboard; no answer within `timeout_secs` (default 300) **fails closed**
   through the same path as an LLM failure. The dashboard shows parked events as
   "⚠ waiting for YOUR answer" rows. Instances created interactively through the dashboard
   default to a `*` → manual rule — the human is there, driving; instances the model creates
   through its own tools get no such default. **Dashboard-created clients refine this**: the
   `*` → manual rule is preceded by one zero-action static rule per connect event (each
   `<proto>_connected` id the client's registry entry declares), so establishing a connection is
   "answered with nothing" and does not park — several clients handle their connect event inline
   in `connect()`, so parking it would stall creation itself. Everything after the connect
   handshake still falls through to `*` → manual. Servers keep the single `*` → manual rule.
4. **LLM** — one model round-trip per event (the fallback when no rule matches)

Scripts and static handlers are the right default for deterministic behavior (echo, canned
responses, routing). Reserve the LLM for responses that genuinely require reasoning.

A caveat manual handlers exposed: several clients handle their `*_connected` event inline in
`connect()` before returning, so a parked connect event delays creation until answered — the
command channel must therefore register **before** that call (`tcp` and `telnet` do), or
`[ send ]` reads "no command channel" for the whole park.

### Action & event design rules (CRITICAL)

**Never put raw bytes or base64 in action parameters or event data.** Models cannot reliably
produce or parse them. Use structured fields: `{"method": "GET", "path": "/", "headers": {…}}`,
not `{"data": "SGVsbG8="}`.

**If an action does document a hex or encoded field, the executor must actually decode it.**
TCP had a bug of exactly this shape and it is worth knowing as the reference case (fixed in
`d70bb5b5`): `send_tcp_data` was documented in three places as accepting "text or hex-encoded
binary", but its executor did `data.as_bytes()` and never decoded hex, so a model following the
documentation put literal ASCII on the wire. Inbound data *was* hex-encoded when non-printable,
making the round-trip asymmetric — an echo server could not echo. The fix was an explicit
`encoding` field (`"utf8"` default, `"hex"`) on both directions rather than sniffing, because
`"48656c6c6f"` is simultaneously valid text and valid hex and only the sender knows which it
means. When you touch a protocol, verify its documented encoding matches its executor.

**Protocols must not implement storage** — no databases, filesystems, or persistence written
into a protocol's Rust implementation. The LLM supplies all data via actions, scripts, static
responses, or server memory. MySQL has no tables; the model answers every query.

The sanctioned exception is the generic SQLite facility (`src/state/sqlite.rs`, feature
`sqlite`, included in `all-protocols` and therefore in default builds). The LLM can call
`create_database` / `execute_sql` / `list_databases` / `delete_database` at runtime, scoped to
a server, a client, or globally. The point of the rule is that storage is a *generic runtime
capability the model opts into*, never something a protocol hardcodes. Two caveats worth
knowing: `create_database` defaults to **file-backed**, writing `./netget_db_<name>.db` into
the process's working directory, and server/client-scoped databases are deleted when their
owner closes while global ones persist.

### Privilege model

`ProtocolMetadataV2::privilege_requirement` declares `None` / `PrivilegedPort(u16)` /
`RawSockets` / `Root`, checked against `SystemCapabilities` (`src/privilege.rs`) in
`server_startup.rs` before spawn, which now gates on `PrivilegeRequirement::is_met_by()` alone
and detects capabilities by actually probing (a `SOCK_RAW` socket, `/dev/bpf*`, `geteuid()`)
rather than inferring. Both were broken until this pass — the probe used `pcap::Device::list()`,
which any unprivileged user can call, so the check never fired for anything.

Declare `privilege_requirement` on any new protocol that needs raw sockets, a TUN device, or a
port below 1024. Two failure modes to avoid:

- **A `PrivilegedPort` above 1023 can never fire.** `svn` declared `PrivilegedPort(3690)`, which
  read as protection and was dead code. Declare `None` if the default port is unprivileged.
- **Don't claim more than you need.** `ospf` declared `Root` when it wants `CAP_NET_RAW`, which
  would refuse to start on a capability-only process that could in fact run it.

There is no variant for *device* access — Bluetooth adapters, USB, NFC readers. Those seventeen
protocols sit at `None` because every other option would be a lie; see IMPROVEMENTS item 60.

Startup must report failure. `spawn()` has to await readiness and return `Err` so
`server_startup` sets `ServerStatus::Error`. ARP, DataLink and ICMP used fire-and-forget
`spawn_blocking` and sat in `Running` having captured nothing — a server that lies about being
up is worse than one that refuses to start.

`get_dependencies()` / `ProtocolDependency` (`src/protocol/dependencies.rs`) is plumbed —
`get_excluded_protocols()` is called from the event handler and the TUI — but **no protocol
overrides `get_dependencies()`**, so the exclusion map is always empty and the mechanism does
nothing. Adopting it is cheap: declare dependencies and the existing plumbing starts excluding
unusable protocols with install hints.

## Adding a server protocol

1. `src/server/<protocol>/mod.rs` — server loop, dual logging, connection tracking, register
   the accept-loop `JoinHandle` via `AppState::register_server_task()` (required for
   `stop_server` to actually release the socket)
2. `src/server/<protocol>/actions.rs` — implement `ProtocolActions`: `metadata()` (state +
   privilege), `get_startup_parameters()`, async/sync actions, `get_event_types()`,
   `execute_action()`
3. `src/server/<protocol>/CLAUDE.md` — implementation notes, library choice, limitations
4. `src/server/mod.rs` — feature-gated `pub mod`
5. `src/protocol/server_registry.rs` — feature-gated `register()`
6. `Cargo.toml` — feature flag, optional deps, add to `all-protocols`; add to `dist` /
   `dist-darwin` / `dist-windows` only if it links no system library unavailable on that target
7. `tests/server/<protocol>/e2e_test.rs` — mocked E2E (see Testing)
8. `tests/server/<protocol>/CLAUDE.md` — strategy, mock expectations, LLM call budget
9. **`tests/server/mod.rs` — add `pub mod <protocol>;`** (see footgun below)

Client protocols follow the same shape against `client_registry.rs`, `src/client/mod.rs`,
`tests/client/mod.rs`, and the `Client` trait. Consult `CLIENT_PROTOCOL_FEASIBILITY.md` first.
Clients are less finished than servers, but the blanket claim that their `JoinHandle` is never
stored is **stale** — `register_client_task` now has ~80 of 85 adopters (`bgp`, `ssh_agent` and
others among them). Check the specific client rather than assuming. Where it does bite, the
subtler form is worth knowing: aborting a task does **not** abort tasks it spawned, so BGP's
keepalive timer kept the socket alive after `remove_client()` even though the read loop was
registered. Register every task you spawn, not just the top one.

Two client-side gaps closed recently, both worth knowing before you touch a client:

- **Clients accept injected actions.** `AppState::send_to_client(client_id, action, timeout)`
  executes an action inside a running client's connection loop and returns a
  `ClientSendOutcome` (`src/state/client_handles.rs`). Until this existed, `Client::execute_action`
  was reachable *only* from inside each client's own loop, and only the LLM could produce an
  action for it — nothing, not even a scheduled task, could put bytes on the wire on demand.
  A client opts in with a ~25-line diff: `command_support::register_command_channel` plus a
  `tokio::select!` arm calling `handle_stream_client_command` (`src/client/command_support.rs`);
  `tcp` and `telnet` are wired, and non-adopters simply never register (the dashboard greys out
  `[send]`). The channel is **bounded** — "client busy" backpressure is correct for
  user-initiated sends, unlike the unbounded status channels.
- **Client `event_handlers` are dispatched.** They were stored, validated and round-tripped for a
  long time while `get_client_event_handler_config` had *zero* callers, so every client event
  went to the LLM regardless. `try_execute_client_event_handler`
  (`src/llm/event_handler_executor.rs`) now mirrors the server dispatcher and is wired at the one
  choke point every client protocol uses — `client/llm_budget.rs::call_llm_for_client` — **before**
  the budget debit, so a deterministic handler costs no LLM budget. Its `Handled` variant returns
  raw action JSON rather than an `ExecutionResult`, because only the client's own loop owns the
  socket. Scripts get a client-shaped `ScriptInput` (`client` set, `server` absent and
  skip-serialized, so server scripts see byte-identical input).

**Startup parameters**: every key a caller may pass must be declared in
`get_startup_parameters()`. `StartupParams::new` and all `get_*` accessors return
`Result<_, StartupParamError>` (`src/protocol/spawn_context.rs`), and params are validated
*before* `add_server`, so an undeclared key or wrong-typed value produces a clean error naming
the key and listing the allowed ones, and leaves no half-registered server behind. They used to
panic, which over MCP killed the per-request task before it could reply — the caller hung with
no error and the server stuck in `Starting`. Propagate the error with `?`; never `unwrap()` it.

Two related traps when declaring parameters: a parameter that is declared but never read is
dead weight the model will try to use (nine were found in the cloud protocols alone), and a
parameter read but never declared is rejected at startup. Both are worth a grep when you touch
`get_startup_parameters()`.

## Testing

Black-box and prompt-driven: the LLM (or a mock of it) interprets an instruction, and tests
validate the result with real protocol clients.

**Tests define expected behavior. When a test fails, fix the implementation, not the test.**
"The test passes but the implementation doesn't work" means you are not done. Only change a
test when its expectation is genuinely wrong.

### Test location policy (CRITICAL)

**All tests live in `tests/`. Never add `#[cfg(test)] mod tests` to `src/`.** Tests reach
internals via `use netget::` public APIs; make items public or refactor if needed.

This policy is currently violated by 9 files in `src/` (`llm/config`, `llm/reference_parser`,
`llm/hybrid_manager`, `llm/embedded_inference`, `protocol/event_logger`, `protocol/log_template`,
`system_stats`, `server/proxy/cert_cache`, `server/bluetooth_ble/mod`). Migrate them if you are
working nearby; do not add more. Current list:

```bash
grep -rln "#\[cfg(test)\]" src/ --include='*.rs'
```

### The mod.rs footgun (CRITICAL)

`tests/server.rs` and `tests/client.rs` only compile submodules explicitly declared in
`tests/server/mod.rs` / `tests/client/mod.rs`. **A test directory that exists on disk but is
not declared is silently never compiled and never run — no error, no warning.**

This was the single largest hole in the suite — **15 of 116 server test dirs and 61 of 83
client test dirs were orphaned**, including complete, correctly-gated E2E suites for `arp`,
`whois`, `bitcoin`, `igmp`, `tls`, `sip`, and every USB protocol. All 76 are now declared, and
the `orphaned-tests` job in `.github/workflows/ci.yml` fails the build if it happens again.
Verify locally with:

```bash
comm -23 <(ls -d tests/server/*/ | sed 's|tests/server/||;s|/||' | sort) \
         <(grep -oE "pub mod [a-z0-9_]+" tests/server/mod.rs | awk '{print $3}' | sort)
```

Note `$3`, not `$2`: `grep -oE "pub mod <name>"` yields three fields and `$2` is the literal
word `mod`, so the version this file carried until now reported every directory as orphaned.

### Mocks

Default mode needs no Ollama. `tests/helpers/mock_ollama.rs` runs a real in-process axum
server implementing `/api/chat`, `/api/generate`, `/api/tags`. Configure with `.with_mock()`
and **always finish with `server.verify_mocks().await?`** — without it the test asserts
nothing about LLM interaction. An unmatched request returns HTTP 500 with a clear error, and
`verify_calls()` dumps full call history on mismatch.

UDP-style protocols (DNS, STUN, NTP, DHCP, BOOTP, TFTP…) **must** use
`.respond_with_actions_from_event()` to echo the client's random transaction/query ID back.
Static mocks with hardcoded IDs cause client timeouts. See `tests/server/dns/CLAUDE.md`.

```rust
.on_event("dns_query")
.and_event_data_contains("domain", "example.com")
.respond_with_actions_from_event(|e| serde_json::json!([{
    "type": "send_dns_a_response",
    "query_id": e["query_id"].as_u64().unwrap_or(0),   // ← must be dynamic
    "domain": "example.com", "ip": "93.184.216.34"
}]))
.expect_calls(1)
```

Keep each suite under ~10 LLM calls: reuse servers, bundle scenarios, prefer script mode.
Bind to localhost only (127.0.0.1 / ::1); never contact external endpoints.

### Running tests

```bash
# Single protocol (fast: 10-30s)
./cargo-isolated.sh test --no-default-features --features tcp \
    --test server::tcp::e2e_test -- --test-threads=100

# Full sweep (slow, 3GB+ RAM)
./cargo-isolated.sh test --all-features --no-fail-fast -- --test-threads=100
```

**Always pass `--test-threads=100`.** Single-threaded runs are 10-20x slower; if a test hangs,
fix the hang rather than serializing the suite.

`./test-e2e.sh <protocol>` runs mocked; `./test-e2e.sh --use-ollama <protocol>` uses a real model.

### CI reality

Two workflows. `release.yml` triggers on `v*` tags and manual dispatch, and runs `cargo build`
for the `dist*` feature sets across 6 targets. `ci.yml` is the PR/push-to-master gate, added
after a long period when no CI job ran `cargo test` at all:

| Job | Blocking | What it does |
|---|---|---|
| `lint` | yes | `cargo fmt --check`; `clippy -D correctness -D suspicious`. A full default clippy runs advisory-only — ~50 style/complexity warnings predate the gate |
| `test` | yes | `cargo test` on `tcp,http,dns,udp,redis,mcp-stdio` |
| `single-feature` | yes | `cargo check --tests` on 14 protocol features **one at a time** — catches a feature whose deps are under-declared, which no multi-feature build can |
| `orphaned-tests` | yes | Fails if a test dir on disk is undeclared in `mod.rs` (see the footgun above) |

The gate is deliberately not `--all-features`: that needs system libraries the runner does not
install (`protoc`, `libpcap`, `dbus`, `libusb`, `pcsclite`). So **the CI feature set covers 6 of
116 protocols** — a green PR says nothing about the other 110. Run the relevant tests yourself
before claiming done.

## Building

`./cargo-isolated.sh` wraps cargo with sccache, disabled incremental, stable path remapping,
and automatic logging. Despite the name it now uses the **shared `target/` directory** —
concurrent runs from different sessions contend on the same lock, so serialize your builds.

Logs land in `./tmp/netget-<command>-<PPID>.log`; the path is printed on each run.

```bash
./cargo-isolated.sh --print-last                     # whole log
./cargo-isolated.sh --print-last | grep "error\[E"   # all compile errors at once
./cargo-isolated.sh --print-last | grep -B2 -A5 "^error:"
./cargo-isolated.sh --print-last | grep -A5 "FAILED"
```

**Fix every error in one pass.** Builds cost 10s-2min; rebuilding after each individual fix
wastes hours. Build once, extract the full error list from the log, fix all of it, rebuild once.

Use minimal features. `--all-features` compiles 50+ protocols and their dependency trees
(1-2 min, 3GB+ RAM); `--no-default-features --features <protocol>` takes 10-30s. Reach for
`--all-features` only for release validation.

Kill stuck builds with `./cargo-isolated-kill.sh`, never `pkill cargo`.

### The installed binary — `/Users/matus/bin/netget`

The maintainer's `netget` on `PATH` lives at `/Users/matus/bin/netget` and **must always have every
protocol compiled in.** Build it with the **`all-protocols`** feature, release — **NOT
`--all-features`** (see the footgun below). After any change that adds a protocol or affects the
compiled surface, rebuild and reinstall:

```bash
./cargo-isolated.sh build --release --no-default-features --features all-protocols && \
  cp target/release/netget /Users/matus/bin/.netget.new && \
  mv -f /Users/matus/bin/.netget.new /Users/matus/bin/netget   # atomic; safe if netget is running
```

The atomic `mv` matters: the maintainer runs `netget --mcp` interactively, and overwriting the
file in place can fail with `ETXTBSY` while it is running. Rename swaps the inode instead — the
live process keeps the old image, the new binary takes effect on next launch. **Never kill the
running `netget` to install** — the rename never needs it stopped.

**`--all-features` vs `all-protocols` — they are NOT interchangeable, and using the wrong one
crashes the binary at startup.** `--all-features` is a Cargo built-in that turns on *every* feature
in `Cargo.toml`: not just protocols but also `embedded-llm`, **`gpu`** (Metal/MLX GPU backend),
`android-termux`, the test-only `terminal-snapshot`, and the `dist*`/`portable-base` aggregates.
On macOS the `gpu` feature initializes a Metal context at startup that CFRelease-crashes
(`EXC_BREAKPOINT`/SIGTRAP in CoreFoundation) — so an `--all-features` binary dies the instant the
TUI renders. Use `--all-features` **only** for a compile check (`cargo check --all-features`), never
for a binary you run. `all-protocols` is the curated "every protocol, and only things safe to run"
set — it includes `embedded-llm` (dormant unless `--embedded-model` is passed) but **not** `gpu`,
which is why it runs. If `gpu`/Metal startup is ever fixed the two could converge; until then, build
the runtime binary from `all-protocols`.

The TUI installs a native-crash terminal restorer (`crash_restore` in `src/cli/rolling_tui.rs`): a
SIGSEGV/SIGABRT/SIGTRAP from a C/ObjC library bypasses Rust's `Drop`/panic machinery, so without it
a crash leaves the shell wedged in raw mode. The handler restores cooked mode + cursor before the
process dies. It is a safety net, not a licence to ship a crashing binary — fix the crash too.

### Features unavailable in Claude Code for Web

Detect with `./am_i_claude_code_for_web.sh` or `[ "$CLAUDE_CODE_REMOTE" = "true" ]`.
These need system libraries absent there — derive the current list from `Cargo.toml`
rather than a hardcoded copy:

| Group | Needs | Features |
|---|---|---|
| Bluetooth LE | `libdbus-1-dev` | all `bluetooth-ble*` (18) |
| USB | `libusb-1.0-dev` | `usb`, `usb-keyboard`, `usb-mouse`, `usb-serial`, `usb-msc`, `usb-fido2`, `usb-smartcard` |
| NFC | `pcsclite` | `nfc`, `nfc-client` |
| Protobuf | `protoc` | `etcd`, `grpc`, `zookeeper` (**not** `kubernetes` — see below) |
| Packet capture | `libpcap` | `datalink`, `arp`, `isis` |
| Other | — | `smb-client` (`libsmbclient`) |

```bash
# Safe pattern
./cargo-isolated.sh build --no-default-features --features tcp,http,dns
```

`kubernetes` was listed under `protoc` for a long time and never needed it: `build.rs` invokes
`protoc` only under `#[cfg(feature = "etcd")]`. The `kubernetes` *client* pulls `kube` +
`k8s-openapi` (large, but no system library); the `kubernetes-server` protocol deliberately
depends on neither, because `kubectl` speaks JSON to an apiserver by default. Derive this table
from `Cargo.toml` and `build.rs` rather than trusting it — it has been wrong.

## MCP surface

`--mcp` (stdio) and `--mcp-http PORT` expose 12 tools sharing the TUI's code paths. See
`src/mcp_stdio/CLAUDE.md`. Current gaps to keep in mind when testing a protocol through MCP:

- **No client tools.** `list_protocols` lists client protocols and `get_protocol_docs`
  instructs the caller to use `open_client`, but no MCP tool starts a client.
- `start_server` cannot pass `interface` or `mac_address` (hardcoded `None`), so
  interface-bound protocols (`arp`, `datalink`, `icmp`, `isis`) can't be targeted at a real
  NIC; nor `scheduled_tasks`, `initial_memory`, or `feedback_instructions`.
- `send_first` is accepted by `start_server_from_action` as `_send_first` and **ignored
  entirely** — on every path, not just MCP.
- Unknown action names in `event_handlers` are accepted at startup and silently do nothing at
  runtime; the client gets no response and the access log records the action as if it ran.
- `get_protocol_docs` returns the TUI LLM's documentation (`open_server`, `base_stack`), which
  describes an API MCP callers cannot invoke.
- No state persists across process restarts, and `stop_server`/`stop_all` skip
  `cleanup_server_tasks()`, orphaning scheduled tasks.

A long-running `netget --mcp` process keeps executing its original binary image after a
rebuild. When behavior contradicts the source, confirm which build is actually running before
concluding there's a bug.

## Logging

**Dual logging everywhere**: tracing macros (`error!`/`warn!`/`info!`/`debug!`/`trace!`) →
`netget.log`, and `status_tx.send()` → TUI/MCP status stream. Levels: ERROR critical, WARN
non-fatal, INFO lifecycle, DEBUG summaries, TRACE full payloads.

Every status/event channel is an **unbounded** `mpsc` — there is no backpressure anywhere.
Don't add high-frequency per-byte messages to these channels.

## Scheduled tasks

Three scopes: **Global** (any server), **Server** (auto-cleaned on close), **Connection**
(auto-cleaned on close). Create via the `open_server` action's `scheduled_tasks` array or the
`schedule_task` action; add `connection_id` for connection scope. Parameters: `task_id`,
`recurring`, `interval_secs`/`delay_secs`, `instruction`. Use connection scope only for
long-lived connections (SSH, WebSocket); short-lived request/response protocols should use
server scope.

## Multi-instance collaboration

Assume other agents work in this repo concurrently.

- **Never `git add -A`, `git add .`, `git add -u`, or `git commit -a`.** Stage explicit paths
  only — `git add <path> && git commit -m …`, listing every file. This is not style: a broad
  `git add` sweeps up whatever other agents have half-written, and a half-written change
  committed without its other half breaks `master` for everyone.

  It has happened three times. Twice it broke the build: once landing a caller without its
  callee (`CertificateCache::new` gaining a third argument in `proxy/mod.rs` while
  `proxy/cert_cache.rs` stayed uncommitted), once landing 24 import swaps without the module
  they imported. The third time, a `git add -u` intended for a documentation cleanup swept
  fourteen source files from five different agents into a commit titled "docs: remove 46
  one-off session and status reports" — it happened to compile, but the history now
  misattributes that work.

  **`git add -u` counts.** It stages every tracked modification in the tree, which during
  parallel work is everyone's. The `-u` flag reads as narrower than `-A` and is not.

- **Staging explicit paths is NOT enough — pass the paths to `git commit` too.** This is the
  fourth incident and the subtlest: `git add <paths> && git commit -m ...` stages what you
  listed and then commits **the whole index**, including anything another agent staged and had
  not yet committed. In August 2026 that swept a *deletion* of `src/server/usb/smartcard/crypto.rs`
  — staged by an agent mid-edit — into an unrelated BGP-client commit, leaving HEAD referencing
  a module whose file was gone. It took a follow-up revert to repair.

  Use the pathspec form, which commits only those paths regardless of what else is staged:

  ```bash
  git add <paths> && git commit <paths> -m "..."     # or: git commit -- <paths>
  ```

  And check before you commit, because the index is shared state:

  ```bash
  git diff --cached --name-only     # must list only your files
  ```

- **The pathspec form protects against a dirty INDEX, not a dirty WORKING TREE — and for a
  shared file that distinction is the whole problem.** `git commit <pathspec>` commits the
  *working-tree* content of those paths. So if another agent has `Cargo.toml` mid-edit and you
  name `Cargo.toml`, you commit **their uncommitted hunks along with yours**. Three agents hit
  this independently in one session, on `Cargo.toml`, both registries and both test `mod.rs`
  files. The rule above reads as complete protection and is not.

  For a file only you touched, the pathspec form is fine. For a **shared** file, rebuild its
  blob from `HEAD` plus only your own lines and stage that, leaving the working tree alone so
  the other agent's edits survive:

  ```bash
  # HEAD-based patch of just your hunks, applied to the index only
  git diff HEAD -- <shared-file> > /tmp/mine.patch   # then edit down to your hunks
  git apply --cached /tmp/mine.patch
  git diff --cached --name-only    # must list only your files
  git diff --cached -- <shared-file>   # must show only your lines
  git commit -m "..."              # no pathspec: the index is already exactly right
  ```

  `git hash-object` + `git update-index --cacheinfo` achieves the same thing when the file is
  easier to reconstruct than to patch. Either way, **verify `git diff --cached` before
  committing** — that is the only step that actually catches the mistake.

- **Even the patch-the-index method races, because the index is shared. Under heavy concurrency,
  commit through a PRIVATE index.** The fifth incident (August 2026, a wave of ~10 concurrent
  agents): three agents each staged only their own hunks correctly via `git apply --cached`, and
  were *still* swept — because in the window between their staging and their `git commit`, another
  agent's bare `git commit` (no pathspec) committed the whole shared index, carrying the first
  agent's staged files into the second agent's commit. `git diff --cached` was clean when they
  checked it; the race happened after. Several new protocols landed misattributed inside
  unrelated commits this way. It compiled and nothing was lost, but the branch ref also got
  rewound twice, orphaning commits.

  The shared `.git/index` is the contended resource. Give yourself a private one with
  `GIT_INDEX_FILE` so no other agent's commit can see or sweep your staging:

  ```bash
  export GIT_INDEX_FILE=$(mktemp /tmp/idx-<your-task>.XXXX)
  git read-tree HEAD                      # seed the private index from HEAD
  git add -- <your paths>                 # stages into the PRIVATE index only
  git diff --cached --name-only           # must list only your files
  T=$(git write-tree)
  C=$(git commit-tree "$T" -p HEAD -m "feat(x): ...")   # GPG: add -S
  git update-ref refs/heads/master "$C" HEAD            # CAS: fails if master moved — then retry
  unset GIT_INDEX_FILE
  ```

  Two cautions. (1) `commit-tree`+`update-ref` is the one legitimate use of `update-ref` — but it
  must be a compare-and-swap against the `HEAD` you built on (the third arg), so it *fails* rather
  than clobbering if master advanced; on failure, re-read HEAD and rebuild. Never `update-ref`
  without the old-value CAS argument — an unconditional one is the force-move that got flagged as a
  security violation. (2) This only serializes *your* commit's atomicity; it does not order you
  against other agents. When two agents both target `master`, the CAS makes the loser retry — which
  is correct. If your harness gives each agent its own **worktree** (separate `.git` index and
  working tree both), that is strictly better and none of this is needed — prefer it for any agent
  expected to commit under contention. But worktrees branched from a stale base still cost a
  re-apply for anything touching hot central files (`action_helper.rs`, the `CommonAction` enum,
  `executor.rs`), so isolation moves the cost from *corruption* to *merge*, it does not remove it.
- **Shared files** (`Cargo.toml`, both registries, `server/mod.rs`, `client/mod.rs`, both test
  `mod.rs` files, `state/server.rs`): use `Edit`, add incrementally, never overwrite wholesale.
- **Give scratchpad files a name unique to you.** Two agents independently wrote `mod.rs.bak`
  into the shared session scratchpad; one clobbered the other, and restoring "the" backup put
  one protocol's source into another protocol's file. Prefix every scratch file with your task.
- **Never revert a dirty working-tree file you did not demonstrably write** — no
  `git checkout -- <path>`, `git restore`, or overwrite to "tidy up" an edit that looks
  half-finished. You cannot tell your own abandoned work from another agent's work in progress,
  and the tree is routinely mid-edit for several agents at once. An unverified edit left in the
  tree is a far smaller problem than deleted work. This happened: a batch of subagents died
  mid-task leaving three modified files, and the reflex to restore a clean tree wiped edits that
  may not have been theirs. They survived only because `git diff > patch` had been run first.
  If you truly must clear a path, save `git diff -- <path>` to a uniquely named scratch file
  **first** and say so — but prefer leaving it alone and reporting it.
- **Don't fan out a large agent wave while the API is failing.** If agents start returning
  "stalled" or the tool-permission classifier times out, stop and wait instead of launching the
  next slice. Three waves were attempted during one degraded period and burned **~9.8M tokens to
  return 5 usable results**; the dead agents also left half-written files in the shared tree for
  someone else to trip over.

  Two diagnoses that looked obvious and were both **wrong**, recorded so they are not re-tried:
  (1) *a cold `target/` making agents queue on the build lock* — warming the cache first changed
  nothing; (2) *agents blocking >180s inside `cargo check` with no output* — a rewritten wave that
  forbade cargo entirely still lost 28 of 29 agents. The stall is in the agent infrastructure, not
  in what the agents were asked to do, so **rewriting the task does not rescue it — only waiting
  does.** The tell is uniformity: when nearly every agent dies with the same "no progress for
  180000ms" on all 6 retries while one or two trivial ones succeed, that is the platform, not the
  prompt.
- **Pause and report** if you hit an error in code you did not modify. It is almost always
  another agent mid-edit; retry rather than "fixing" their file.
- **Verify HEAD, not the working tree.** During parallel work the working tree is routinely
  mid-edit and its failures belong to nobody. Check the committed state in a throwaway
  worktree: `git worktree add --detach <tmp> HEAD && cargo check --all-features` with its own
  `CARGO_TARGET_DIR`. Remove the worktree afterwards.
- `--ollama-lock` serializes LLM API access (default in tests). Concurrent `git` work should
  use worktrees.
- Never `pkill cargo`; use `./cargo-isolated-kill.sh`.
- The user runs `netget --mcp` interactively. **Never kill netget processes.**

## Known systemic issues

Read before assuming a subsystem is sound:

- **Fail-open defaults are the most dangerous pattern in this codebase.** When the LLM returns
  nothing usable, a protocol must not fall through to a permissive default. OAuth2 did: no
  action meant a hardcoded authorization code, a hardcoded access token, and introspection
  answering `{"active": true}` for *every* bearer token — so an LLM outage silently issued
  credentials, and a model's explicit denial was indistinguishable from silence and became an
  approval. Its own CLAUDE.md documented this as a feature ("ensures the server always responds
  correctly"). Default to refusal, and make the model's rejection path structurally distinct
  from its no-answer path. `radius` is the worked example of the shape to copy: nothing in its
  `actions.rs` can synthesise an Access-Accept, accept and reject share no code path, and a
  no-answer logs `decision=fail_closed_*` while a model denial logs `decision=model_reject` —
  distinguishable in the log *and* on the wire. Its regression test asserts the fail-closed
  packet is also correctly *signed*, because a denial the client discards as corrupt is just a
  timeout.
- **A tokio blocking API called from async context, with the panic swallowed by `tokio::spawn`.**
  Found three times: `block_on` inside `UsbInterfaceHandler::handle_urb` (usb-msc, usb-fido2) and
  `tokio::sync::Mutex::blocking_lock()` in SMB's connection task. The failure mode is identical
  and nasty — the task panics, `tokio::spawn` swallows it, the server stays `Running`, the log
  shows the operation *succeeding*, and the peer hangs. In SMB every SESSION_SETUP killed its own
  connection the moment the LLM approved the login. If a synchronous callback needs an async
  result, restructure so the sync side parks the request and an async task feeds the answer back
  (usb-fido2 does this over CTAPHID `KEEPALIVE`); do not bridge with `block_on`.
- Byte-index string truncation (`&s[..N]` guarded only by `s.len() > N`) panicked on multi-byte
  UTF-8 at the cut point, on LLM output and event descriptions. Fixed in `b9aa1058` —
  `src/utils/truncate.rs` has char-boundary helpers; use `truncate_for_log` rather than slicing.
  `src/protocol/log_template.rs` was the important one (`c1515188`): being shared
  infrastructure, it defeated protocols' own local fixes.
- **Resolved (August 2026), recorded because the claim outlived the code.** This file used to
  say `git` and `mercurial` "call `generate_with_retry` directly, bypassing the rate limiter,
  the retry/repair loop, and event-handler dispatch". They do not, and a `grep -rn
  generate_with_retry src/` now finds callers only inside `src/llm/` itself: the `git` *client*
  goes through `call_llm_for_client` (budget, limiter and client `event_handlers` all apply),
  and `src/server/git/mod.rs` and `src/server/mercurial/mod.rs` both use
  `action_helper::call_llm`. Re-run that grep before trusting any similar claim here.
- On LLM failure a protocol must not reset to Idle and write nothing, leaving the peer to hang
  until its own timeout. **Swept across all 135 server protocols in August 2026** (audit found
  64 defective: 54 silent, 4 leaking the error onto the wire, 6 mixed). 44 now answer with a
  `crate::utils::WireFailure` category; copy `http` (503 + `Retry-After` vs 500) or `tcp`
  (half-close, so the peer reads EOF) when you touch a new one. Re-derive rather than trusting
  this: grep `LLM error` in `src/server/*/mod.rs` and check whether the following ~18 lines
  write anything.

  **20 protocols are deliberately silent and must stay that way** — `arp`, `bootp`, `datalink`,
  `dhcp`, `igmp`, `ipsec`, `isis`, `mdns`, `openvpn`, `ospf`, `radius`, `rip`, `rtp`, `syslog`,
  `udp`, `usb/mouse`, `usb/keyboard` and the BLE profiles. Each says so in its own CLAUDE.md.
  The rule is that **a fabricated reply is worse than silence when every reply the protocol
  defines is a positive assertion.** `openvpn` is the case to remember: its only pre-TLS server
  message is `P_CONTROL_HARD_RESET_SERVER_V2`, and sending it *is* admitting the peer — so
  "fixing the silence" there would turn a backend outage into an authentication bypass. ARP
  would write a fabricated MAC into the requester's neighbour cache; OSPF/IGMP/RIP/BOOTP/DHCP
  have no error frame at all. Where the wire cannot carry the distinction, the **log** must:
  tag `decision=model_reject` / `model_silent` / `fail_closed_llm_error` as `radius` does.
- **A static handler with an empty `actions` array appears NOT to suppress the LLM call —
  mechanism unexplained, treat with suspicion.** Observed in `tests/server/xmpp/peer_inject_test.rs`
  under full-suite load: with `{"type":"static","actions":[]}` on a `*` pattern, a probe on the
  `call_llm` error branch fired with a real backend error, proving the model was consulted;
  changing only that JSON to `[{"type":"wait_for_more"}]` made the probe stop firing. The two
  runs differed in nothing else, so the attribution is sound, but **the mechanism was not found**.
  Ruled out by inspection: `parse_event_handlers` accepts an empty array, `EventHandlerType::validate`
  passes it, `find_handler`'s `*` matches, `execute_static_handler` returns `Handled` regardless
  of length, `action_helper` propagates a handler `Err` rather than falling back, and
  `start_server_from_action` applies `event_handler_config` (~line 608) *before* `spawn` (~line
  727), so there is no config-not-yet-applied race. Something else is going on. Until it is
  understood:
  - prefer a real no-op verb the protocol declares (`wait_for_more`) over an empty list;
  - **`src/tui/modal/form.rs:607` uses exactly the empty-list form** for every dashboard-created
    client's `<proto>_connected` rule, whose entire purpose is to stop the connect event parking
    or reaching the model. If the observation generalises, that rule does nothing and connect
    events go to the LLM anyway. **Unverified — worth an experiment before trusting it.**
  - the `wait_for_more` replacement has **one** full-suite confirmation, so it may itself be a
    lucky run rather than a fix. Re-run under load before relying on it.
- **`ServerForm::create` substitutes a default instruction** (`"You are a {protocol} server.
  Handle requests appropriately."`) whenever `instruction` is `None` — see
  `src/cli/management.rs`. Any non-empty instruction makes `operator_wants_dynamic` true, so a
  test built with `..Default::default()` **does consult the model**, whatever its comments claim.
  Two `peer_inject` tests documented "Zero LLM calls" while doing the opposite, and passed only
  because the old fail-open swallowed the resulting error. Pass `instruction: Some(String::new())`
  when you want a genuinely model-free server.
- **Answering the peer is not a licence to tell it anything.** Fixing the silence above
  introduced the opposite defect across ~25 protocols at once: each interpolated the error into
  the reply, so a plain `telnet` session printed `[netget] cannot answer right now: ✗  LLM
  failed to generate valid response after retries.` — netget's own retry machinery, verbatim, on
  a stranger's terminal. Backend URLs, model names, file paths, serde and prost messages and
  `anyhow` context chains all reached the wire this way. It spread because each protocol was
  written by copying its neighbour.

  The rule is **the peer gets a category, the log gets the error**. `crate::utils::wire_failure`
  (`WireFailure::{Overloaded, Unavailable}`) classifies an error and returns `&'static str`
  — deliberately, because a helper returning `String` is one `format!` away from leaking again.
  Keep the two categories distinct: protocols map them onto different codes (503 vs 500, RESP
  `LOADING` vs `ERR`, MySQL 1205 vs 1105, gRPC `UNAVAILABLE` vs `INTERNAL`) so a client backs
  off rather than recording a permanent fault. `smtp` was the one protocol that already had
  this right — byte-literal replies, nothing interpolated. `tests/wire_failure_test.rs` fails
  the build if any of the leaked idioms reappear under `src/server/`.
- Related and now fixed, but worth knowing as the reference case: the rate limiter used to
  `try_acquire` for `RequestSource::Network` and bail on contention, so at the shipped default
  of `--llm-max-concurrent 1` any request overlapping an in-flight LLM call was **discarded** —
  two simultaneous `curl`s were enough — while the flag's help text promised "sequential
  processing". Network requests now wait for a permit, bounded by `--llm-queue-timeout`
  (default 120s) and `--llm-max-queued` (default 128), and both bounds fail with a typed
  `RateLimitError` that `crate::llm::is_overload_error` detects. It survived for so long
  because `tests/helpers/netget.rs` passes `--llm-max-concurrent 1000` to every E2E test, so
  the shipped value was exercised by no test at all; `tests/llm_concurrency_default_test.rs`
  now runs with the flag omitted entirely.
- **The 10-second idle sweep is for connectionless protocols only — declare it.**
  `AppState::cleanup_old_connections` (ticked by the TUI and the MCP loop) evicts any connection
  whose `last_activity` is older than 10s. It exists for UDP/raw/link-level servers, whose
  per-remote-address entries nothing ever closes. It used to run over **every** server, so any
  idle TCP-style connection — a telnet peer parked >10s for a human's manual answer, a BGP
  session between keepalives — was removed from state and drawn as `(closed)` while its socket
  was alive, and every later stat update and the real close targeted an entry that was gone.
  It now runs only where `ProtocolMetadataV2::connectionless` is set (`.connectionless()` on the
  builder; the 22 UDP/raw servers declare it). A new connectionless protocol that forgets the
  flag leaks idle entries until its server stops — mild; the old default was the dangerous one.
  Connection-oriented servers should still call `update_connection_stats` on every read and
  write: it is what the rail's `↓/↑` counters and connection-scoped task prompts read.
- Per-connection tasks are untracked, so `stop_server` does not cancel in-flight connections.
- `AppState` is one global `RwLock` over everything — a throughput ceiling, not a deadlock.
- ~50 of the 63 root markdown files are one-off session/status reports last touched in 2025.
  `ARCHITECTURE.md`, `METADATA_EXAMPLES.md`, `CLIENT_PROTOCOL_FEASIBILITY.md`,
  `LICENSE_ANALYSIS.md`, `SYSTEM_DEPENDENCIES_macOS.md`, `TERMUX_INSTALL.md`, and
  `PROTOCOL_MIGRATION_GUIDE.md` are the durable ones. Do not add new status-report files.

## Git

- Never `git stash`. Always merge with `--no-ff`. Never amend, rebase, or squash shared history.
- Combine `git add` and `git commit` in one command chain.
- Commit as Matus Faro <matus@matus.io>, GPG-signed:
  ```bash
  git config user.name 'Matus Faro' && git config user.email 'matus@matus.io' && \
  git config commit.gpgsign true && git config user.signingkey matus@matus.io
  ```
- Conventional Commits, one logical change per commit. No co-author or bot attribution
  trailers of any kind.
- **Commit as you go.** Land each logical change as soon as it is verified, rather than
  accumulating a large uncommitted tree and committing at the end. Other agents are editing this
  repo continuously: a big working tree is a merge hazard, it is what broad `git add` sweeps pick
  up, and an interrupted session loses all of it. Verify, commit, continue.
- **Verify against a clean baseline, not against "does it pass".** The suite has pre-existing
  failures, so a red run proves nothing on its own. Run the same suite at unmodified `HEAD` in a
  throwaway worktree and **diff the two failure sets** — only the difference is yours. This is
  what separates a real regression from the repo's existing red and from load-flaky tests (the
  suite has at least one: `git::e2e_test::test_git_with_scripting` asserts a wall-clock
  "near-instant" bound and fails under parallel load while passing 3/3 in isolation).
  Cross-check any suspect failure by re-running it in isolation before calling it a regression.
