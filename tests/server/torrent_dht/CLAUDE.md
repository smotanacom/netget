# tests/server/torrent_dht

BitTorrent DHT (BEP 5 / KRPC over UDP) **server**. Every query in this suite is bencode
hand-built with `serde_bencode` and sent over a raw `tokio::net::UdpSocket` owned by the test.
There is **no third-party DHT client** anywhere — no transmission, no aria2, no `bencodepy`,
nothing to install. That is an independent *reading* of BEP 5, not an independent
implementation, and it is why `src/server/torrent_dht` is `DevelopmentState::Experimental` and
must stay there.

## Files

### `e2e_test.rs` — one test, **4 LLM calls**

`test_dht_queries`. One mocked NetGet subprocess, one UDP socket, three queries in sequence
inside the single test body: ping, find_node, get_peers. Rules (`.expect_calls(1)` each):
startup instruction, `dht_ping_query`, `dht_find_node_query`, `dht_get_peers_query`.

Asserts each reply is a KRPC response (`y = "r"`), that find_node's `r` dict carries `nodes`,
and that get_peers carries a token.

### `llm_failure_test.rs` — one test, **1 LLM call**

`test_dht_answers_krpc_error_when_llm_fails`. The failure is forced by mocking only the
*startup* instruction: `dht_ping_query` then matches no rule, the mock answers HTTP 500, and
`call_llm` returns `Err` — the same shape as a real backend outage.

Asserts a BEP 5 error message (`y = "e"`) echoing the query's transaction id with code **201**
(non-transient; 202 is the overloaded category so a node can back off), that the
human-readable half leaks nothing from netget's internals (a forbidden-token list covers
backend URLs, model names, paths, `anyhow` chains), and that the log carries
`decision=fail_closed_llm_error`, which only the LLM-error path writes — a model answering
with `send_dht_error_response` never does.

Silence is not an option here: a querying node holds the transaction id open, retries, then
marks us bad. That is a stall, not a "not me".

### `bencode_depth_guard_test.rs` — four tests, **2 LLM calls**

Three `#[test]` unit tests of `netget::utils::bencode::check_bencode_structure`
(`test_guard_refuses_depth_bomb_and_accepts_real_krpc`, `test_guard_boundary_is_exact`,
`test_guard_bounds_declared_length_against_the_input`) plus one `#[tokio::test]` wire test,
`test_dht_server_survives_a_depth_bomb_datagram` (startup + `dht_ping_query` mocks).

`serde_bencode` 0.2 has no depth limit; one byte opens a nesting level, so ~1,000 `l` bytes
overflow a 2 MiB tokio worker. That is a `SIGSEGV` against the guard page, **not** a panic:
`tokio::spawn` cannot contain it and `catch_unwind` cannot see it — the whole process dies,
for the cost of one `sendto` against an unauthenticated UDP socket.

The wire test's shape is the part worth copying: after the 9,000-level bomb it asks the same
server a second, well-formed question and requires an answer, **because asserting only that
the bomb produced no reply would pass just as happily against a server that had died.** The
file's own header carries the measurement table.

## Running

```bash
./cargo-isolated.sh test --no-default-features --features torrent-dht \
    --test server -- server::torrent_dht --test-threads=100
```

All mocked; no Ollama. There is no "Ollama lock" serializing anything — the flag that
suggested one is inert and its plumbing is deleted.
