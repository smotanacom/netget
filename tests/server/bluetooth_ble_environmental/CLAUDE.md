# BLE Environmental Tests


**2 files, 10 tests**, and this doc named none of them until 22 September 2026.

| file | tests | what it holds |
|---|---|---|
| `e2e_test.rs` | 1 | the service starting and answering, with the model driving it |
| `gatt_examples_test.rs` | 9 | **every UUID and value byte in the startup examples, against the SIG layout** |

**`gatt_examples_test.rs` is the one to read first, and it is the reason this
family has such a file at all.** The startup examples in `actions.rs` are the GATT
layout a model copies verbatim, and every UUID and value byte in them is a literal
nobody checks at runtime: a swapped-endian value, or a characteristic UUID one digit
off, still starts, still advertises and still answers reads. It is wrong only in the
eyes of a real central — and no test in this tree has one, because the BLE suites
that do claim the machine's single adapter and are `#[ignore]`d so a 100-thread run
does not deadlock on it. Checking the bytes against the SIG layout is the strongest
evidence this profile admits.
