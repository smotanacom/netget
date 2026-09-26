# Modbus client tests

```bash
./cargo-isolated.sh test --no-default-features --features modbus --test client -- modbus --test-threads=100
```

| file | peer | LLM calls | proves |
|---|---|---|---|
| `real_server_test.rs` | pymodbus 3.15 device + `mbpoll` | 7 | the client bar (below) |
| `unanswered_test.rs` | a hand-written misbehaving device | 4 | `bad_response`, `timeout`, `MAX_QUEUED` |
| `codec_test.rs` | none | 0 | `encode_request` / `parse_response` and the refusals in `request_from_action` |

## real_server_test.rs — the evidence the rating rests on

The device is **pymodbus 3.15** (`StartAsyncTcpServer` over a `SimDevice` with separate coil /
discrete-input / holding / input blocks of 100 items), written into the `RealServer` temp dir as
`device.py` and run with `python3`; ready when it logs `Server listening`. Discrete inputs 0-2 are
`1 0 1`, input registers 0-2 are `100 200 300`, address 100 and up does not exist. `mbpoll` (a C
master on libmodbus) reads back. A missing `python3`, `pymodbus` or `mbpoll` **fails** the test.

`modbus_client_reads_computes_and_writes_against_pymodbus` (7 LLM calls): the model reads input
registers 0-2 (FC 4), writes each plus one to holding registers 10-12 (FC 16), turns coil 4 on
(FC 5), then in **one** turn reads discrete inputs 0-2 (FC 2) and holding register 200 (FC 3),
shown `[true,false,true]` and then `modbus_exception` code 2 `illegal_data_address`. `mbpoll`
must read `101 201 301` and coil `1`. Each event is matched on `function` and `values`.

That one-turn pair is the regression test for the pipelining finding: sent back to back, pymodbus
dropped the second ADU and the exception never came. Removing the one-transaction-at-a-time
check in `pump` fails this test.

`injected_modbus_writes_reach_pymodbus` drives the command channel in-process (no model; the LLM
endpoint is unreachable on purpose): FC 6 (`Sent { bytes_sent: 12 }`), FC 15 (sent or queued
behind it), two requests refused before the wire (register value 70000, quantity 0), `mbpoll`
reads `4242` and coils `1 0 1 1`, and `disconnect` leaves the client `Disconnected`.

## Verified by mutation

Each removed on its own, with the named test failing: the loop over `result.actions` and the
one-transaction check (`real_server_test`), the response deadline and the function-code check
(`unanswered_test`), the echo check and the byte-count check (`codec_test`), `MAX_QUEUED`
(`unanswered_test`), `encode_request`'s round trip through `parse_request` (`codec_test`).
