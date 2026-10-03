# SSDP client tests — strategy and what each layer actually proves

Run:

```bash
# The target is the `client` test binary; `client::ssdp` is a filter, not a target name.
./cargo-isolated.sh test --no-default-features --features ssdp --test client -- \
    client::ssdp --test-threads=100
```

**11 tests, all passing, ~7.6s wall.** No `#[ignore]`, no skip-when-missing gate: nothing here
prints `SKIP: … not installed` and returns `Ok(())`, which the root `CLAUDE.md` catalogues as a
silent pass on a runner without the binary.

## LLM call budget: 17 across the suite, 4–6 per end-to-end test

| Test | Process | Calls |
|---|---|---|
| `one_search_collects_every_responder_and_hears_an_announcement` | client | **6** — instruction, connected, 2 × search_response, notify, search_complete |
| `a_retransmitted_answer_is_the_same_device` | client | **5** — instruction, connected, 2 × search_response, search_complete |
| `discovers_netgets_own_ssdp_server` | client | **4** — instruction, connected, search_response, search_complete |
| `discovers_netgets_own_ssdp_server` | server | **2** — instruction, ssdp_msearch |

Each individual exchange is well under the ~10-call guideline; the suite total is higher than
that because there are three separate end-to-end scenarios and each pays its own instruction
and connect calls. Merging them would trade a clear statement of what each layer proves for
two fewer calls, which is the wrong trade — the whole point of the layering is that the
circular-evidence test can be read as circular on its own.

The eight layer-1 tests make **zero** LLM calls and touch no socket. That is deliberate: the
executor's refusals and the exact request bytes are the parts that are cheap to pin precisely,
so they are pinned there rather than inferred from an end-to-end run.

Every mocked test ends with `wait_for_mocks(30)` then `verify_mocks().await?`. Without the
second call a test asserts nothing about LLM interaction at all; without the first it asserts
it too early, and reports "expected 1, got 0" for a step that completed 200ms later under load.

## The three layers, weakest evidence last

### 1. Request bytes and executor — no network, no LLM (8 tests)

What we put on the wire is asserted by **parsing it back with the device half's codec**
(`server::ssdp::message`), not by string comparison. A string comparison only says the bytes
did not change; this says a UDA 1.1 parser finds each field where the specification puts it —
`MAN: "ssdp:discover"` *with* its quotes, `MX` in range, `ST` present, `HOST` naming the
address the search was actually aimed at.

The rest pin refusals: CRLF injection through `st`, a missing or blank `st`, a non-numeric
`mx`, a `target` that is not `ip:port`, a device verb (`send_ssdp_response`) sent to a control
point, and `max-age` parsing out of `CACHE-CONTROL` including the cases that must yield `None`
(`no-cache` is not zero seconds).

### 2. Hand-written devices on the wire (2 tests)

`one_search_collects_every_responder_and_hears_an_announcement` is **the** test of this
protocol's distinguishing property. One search; device one receives it and answers; device two
answers the same search from a different address without having been asked (what happens on a
real multicast network, where every device sees the search); device two then announces itself
with a `NOTIFY ssdp:alive`. `expect_calls(2)` on `ssdp_search_response` is what fails if
someone makes the client return on the first datagram.

`a_retransmitted_answer_is_the_same_device` sends three datagrams from **one** address: the
same USN twice, then a different USN. It expects **two** responders and one duplicate — which
is the only shape that proves the dedupe key is `(address, USN)` rather than either half. A
key of address alone gives one; a key of USN alone gives two but for the wrong reason, and
would merge two devices sharing a service type.

**What these prove and what they do not.** The devices are written in this file from UDA 1.1.
That is an independent *reading* of the specification — the same class as `dhcp`'s in-test RFC
2131 decoder and `tests/helpers/usbip_client.rs` — and explicitly **not** an independent
*implementation*. It cannot catch a misreading of the spec that this file and `src/client/ssdp`
share.

### 3. NetGet's own SSDP server, two processes (1 test)

`discovers_netgets_own_ssdp_server` starts a real NetGet SSDP **server** process, reads its
port, then starts a NetGet SSDP **client** process pointed at it over unicast loopback. Both
have their own mock Ollama and both are verified.

**This is same-project evidence and the test's own doc comment says so.** The peer shares this
client's HTTPU codec and was written in the same pass; it shows the two halves agree and cannot
catch a mistake both make. It is the circular-evidence class the root `CLAUDE.md` names, and
it is why the client is `Experimental`. See `src/client/ssdp/CLAUDE.md` for what would earn
Beta.

It is still worth having: it is the only test where the thing answering is a real server
process making real decisions through a model, rather than a fixed string this file wrote.

Two processes rather than one, deliberately: the client's `remote_addr` needs the server's
actual port, and `{AVAILABLE_PORT}` allocates a *different* port per occurrence, so a
single-process prompt cannot name the same port twice. Two processes also make the ordering
deterministic — the server is listening before the client is spawned — rather than depending on
the order two actions from one instruction happen to execute in.

## Everything is unicast to 127.0.0.1, and that is not a workaround

Measured on macOS 27:

> Bound to `127.0.0.1`, joining `239.255.255.250` **succeeds**; *sending* to the group fails
> with **`EADDRNOTAVAIL` (49)**, because loopback carries no multicast route. Bound to
> `0.0.0.0` both work.

So a test that sent to the well-known group from a loopback-bound socket would fail for reasons
that have nothing to do with the client. Every test therefore passes
`bind_address: "127.0.0.1"`, `join_multicast: false`, and a unicast `remote_addr` on an
ephemeral port. UDA 1.1 §1.3.2 permits a unicast M-SEARCH, so these are real searches, not a
testing-only shortcut — and `send_msearch`'s `target` parameter exists for exactly this.

`join_multicast: false` also keeps the suite off every interface but loopback, which the root
`CLAUDE.md`'s localhost-only rule requires. **Do not "fix" a multicast join that is not
broken.**

## Timing

`response_window_ms: 6000` in every end-to-end test, with `mx: 1` on the wire. The MX the
device sees stays realistic while the collection window is long enough that both answers and
the announcement land inside it with a hundred tests running together. The server's own
`max_response_delay_ms` is set to 100 so its MX jitter does not dominate.

Assertions wait for the log rather than reading it straight after sending: the datagrams
cross the socket well before the harness has necessarily drained the child's stdout, so the
direct form is a race that passes only on a quiet machine. There are three bare
`output_contains` calls, and each sits *behind* a `wait_for_log` that has already forced the
output to be drained — the rule is "never as the thing that waits", not "never at all".

## Things that would look like evidence and are not

- **Adding a Rust SSDP crate as the peer.** The crates are control points — they *search*.
  This client also searches; two searchers never talk to each other. What is needed is a
  **device**, and the server-side agent's finding that no crate can be aimed at a unicast
  loopback port is about the other role and does not settle this one.
- **Driving the client with `tokio-tungstenite`-style circularity** — i.e. building the test
  device on `server/ssdp/message.rs`. The device responses here are written as literal text for
  that reason; only the *assertions* parse with the shared codec, where a shared bug would
  make the test fail rather than pass.
- **`one_search_…` passing with `expect_calls` removed from the response rule.** The count is
  the assertion. The log lines would still look right with one responder.
