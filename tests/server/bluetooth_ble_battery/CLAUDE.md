# BLE Battery Service E2E Tests

**4 files, 12 tests**, and this doc named none of them until 22 September 2026.

| file | tests | what it holds |
|---|---|---|
| `characteristic_encoding_test.rs` | 1 | the encoder against the SIG field definition, where clamping is least survivable |
| `decision_tag_test.rs` | 3 | every terminal outcome grep-able as `decision=<token>` |
| `e2e_test.rs` | 2 | the service starting and answering, with the model driving it |
| `gatt_examples_test.rs` | 6 | **every UUID and value byte in the startup examples, against the SIG layout** |

**`gatt_examples_test.rs` is the one to read first, and it is the reason this
family has such a file at all.** The startup examples in `actions.rs` are the GATT
layout a model copies verbatim, and every UUID and value byte in them is a literal
nobody checks at runtime: a swapped-endian value, or a characteristic UUID one digit
off, still starts, still advertises and still answers reads. It is wrong only in the
eyes of a real central — and no test in this tree has one, because the BLE suites
that do claim the machine's single adapter and are `#[ignore]`d so a 100-thread run
does not deadlock on it. Checking the bytes against the SIG layout is the strongest
evidence this profile admits.

## Test Strategy

Battery Service is the simplest GATT service - single byte characteristic (0-100%).

### Test Cases

1. **Server startup** - Validates server starts without crashing
2. **Set battery level** - Validates level updates
3. **Simulate drain** - Validates gradual battery drain

## LLM Call Budget

**Total**: < 5 LLM calls

## Expected Runtime

**Total suite**: 10-15 seconds

## Limitations

- Cannot test actual battery status reporting without hardware
- Tests only validate server doesn't crash
