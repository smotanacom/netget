# JSON-RPC Client E2E Tests

## Test Strategy

Black-box E2E tests that spawn NetGet server and client instances to verify JSON-RPC 2.0 client functionality. Tests use
local JSON-RPC server (also powered by NetGet) to avoid external dependencies.

## LLM Call Budget

**Target:** < 10 calls per file
**Actual:** ~12 across four tests

- 4 server startups (1 per test)
- 4 client connections (1 per test)
- 4 more in the chaining test (2 server methods, 2 client response events)

## Tests

1. **test_jsonrpc_client_single_request** (2 LLM calls)
    - Spawn JSON-RPC server with add/greet methods
    - Connect JSON-RPC client
    - Verify client can call add(5, 3)
    - Validate request/response flow

2. **test_jsonrpc_client_llm_controlled_request** (2 LLM calls)
    - Spawn JSON-RPC server with echo method
    - Client follows LLM instruction to call echo
    - Verify protocol detection (JSON-RPC)

3. **test_jsonrpc_client_batch_request** (2 LLM calls)
    - Spawn JSON-RPC server with add/multiply methods
    - Client sends batch request with 2 calls
    - Verify batch handling

4. **test_jsonrpc_client_chains_a_second_request_from_a_response** (6 LLM calls)
    - `step_one` on connect, `step_two` in answer to its response, and a third client call
      for `step_two`'s response — so it pins a chain of depth 2, not depth 1.
    - This is the regression guard for `MAX_FOLLOWUP_DEPTH`. The client has got this wrong
      twice: follow-up actions discarded outright, then dispatched through the non-notifying
      `perform_*` cores so the chain was exactly one step deep. Verified non-vacuous by
      setting the bound to 0, which fails it with
      `Rule #2: expected exactly 2, got 1 - event=jsonrpc_response_received`.
    - **One** rule handles both response events and branches on `result`. Two rules on the
      same event with no way to tell them apart is first-match-wins, and the second would
      report zero calls.

## Runtime

**Expected:** < 10 seconds (including server/client startup)

## Known Issues

None

## Test Efficiency

All tests reuse the same pattern:

1. Start server with specific methods
2. Start client with instruction
3. Verify output
4. Cleanup

This minimizes LLM calls while providing good coverage of:

- Single requests
- Batch requests
- LLM-controlled method selection

## Future Tests

- Test notification (no response expected)
- Test error handling (method not found)
- Test complex parameter types (objects, arrays)
- Test request ID tracking
- Test HTTP connection failures
