# Bitcoin RPC Client Implementation

## Overview

The Bitcoin RPC client implementation provides LLM-controlled access to Bitcoin Core via JSON-RPC. The LLM can query
blockchain data, monitor the mempool, inspect network status, and perform wallet operations.

## Implementation Details

### Library Choice

- **reqwest** - HTTP client for JSON-RPC communication
- **Direct JSON-RPC** - No bitcoin-rpc crate dependency, manual JSON construction
- Connects to Bitcoin Core node (bitcoind) via HTTP

### Architecture

```
┌──────────────────────────────────────────┐
│  BitcoinClient::connect_with_llm_actions │
│  - Initialize RPC URL                    │
│  - Store connection in protocol_data     │
│  - Mark as Connected                     │
└──────────────────────────────────────────┘
         │
         ├─► execute_rpc_command() - Called per LLM action
         │   - Build JSON-RPC request
         │   - Execute via HTTP POST
         │   - Call LLM with response
         │   - Update memory
         │
         └─► Background Monitor Task
             - Checks if client still exists
             - Exits if client removed
```

### Connection Model

Bitcoin RPC is **JSON-RPC over HTTP** (request/response based):

- "Connection" = initialization with RPC endpoint URL
- Each RPC call is an independent HTTP request
- LLM triggers RPC commands via actions
- Responses trigger LLM calls for interpretation

### LLM Control

**Async Actions** (user-triggered):

**Blockchain Queries:**

- `get_blockchain_info` - Chain info, block count, difficulty
- `get_block_hash` - Get block hash by height
- `get_block` - Get block details by hash
- `get_transaction` - Get transaction by txid
- `get_mempool_info` - Mempool size, bytes, usage
- `get_raw_mempool` - List of txids in mempool
- `get_mining_info` - Network hashrate, difficulty

**Network Queries:**

- `get_network_info` - Version, connections, protocols
- `get_peer_info` - Connected peers details
- `get_connection_count` - Number of peer connections

**Wallet Operations:**

- `get_wallet_info` - Wallet balance, transaction count
- `get_balance` - Current wallet balance
- `list_transactions` - Recent wallet transactions

**Generic:**

- `execute_rpc` - Execute any Bitcoin RPC method with parameters
- `disconnect` - Stop Bitcoin RPC client

**Sync Actions** (in response to RPC responses):

- `execute_rpc` - Make follow-up RPC call based on response

**Events:**

- `bitcoin_connected` - Fired when client initialized
- `bitcoin_response_received` - Fired when RPC response received
    - Data includes: method, result, error, status_code

### Structured Actions (CRITICAL)

Bitcoin client uses **structured data**, NOT raw bytes:

```json
// Request action
{
  "type": "get_block",
  "block_hash": "00000000839a8e6886ab5951d76f411475428afc90947ee320161bbf18eb6048",
  "verbosity": 1
}

// Response event
{
  "event_type": "bitcoin_response_received",
  "data": {
    "method": "getblock",
    "result": {
      "hash": "00000000839a8e6886ab5951d76f411475428afc90947ee320161bbf18eb6048",
      "confirmations": 750000,
      "height": 1,
      "tx": ["..."]
    },
    "error": null,
    "status_code": 200
  }
}
```

LLMs can construct structured RPC requests and interpret JSON responses.

### Request Flow

1. **LLM Action**: `get_blockchain_info` (or any other RPC action)
2. **Action Execution**: Returns `ClientActionResult::Custom` with RPC method and params
3. **RPC Execution**: `BitcoinClient::execute_rpc_command()` called
4. **Response Handling**:
    - Parse JSON-RPC response
    - Extract result or error
    - Create `bitcoin_response_received` event
    - Call LLM for interpretation
5. **LLM Response**: May trigger follow-up queries

### Startup Parameters

- `rpc_user` (optional) - Bitcoin RPC username
- `rpc_password` (optional) - Bitcoin RPC password

**Both were declared and read by nothing until September 2026**, and bitcoind's RPC is
auth-mandatory — it answers every unauthenticated request `401` — so authenticated Bitcoin Core
RPC could not work at all through this client. They are folded into the URL's userinfo on
connect and applied as a real `Authorization: Basic` header on each request.

### RPC URL Format

Accepted formats for `remote_addr`:

- `http://user:pass@localhost:8332` - Full URL with auth
- `localhost:8332` - Auto-prefixed with `http://`
- `https://bitcoin-node.example.com:8332` - HTTPS support

**The first of those did not work either, and that was the less visible half of the same bug.**
`reqwest` does **not** derive Basic auth from URL userinfo; the URL was handed to it verbatim,
so the one form this file told operators to use also produced a `401` on every call, with
nothing saying why. `split_userinfo` now takes the credential out of the URL and sends it as a
header, which is also what keeps it out of the request line that servers log.

### The credential is redacted everywhere a human or the model reads

A userinfo URL used to be stored in `rpc_url` — which the dashboard renders on the client's
facts line — echoed to the status stream on connect, and put into the `bitcoin_client_connected`
event, **which is handed to the model**. A password in the prompt is not a display bug.

`rpc_url_display` carries `http://***@host:port` and is what all three read; `rpc_url` keeps the
real value for `perform_rpc` alone. `tests/client/bitcoin/rpc_auth_test.rs` asserts the header
arrives at a stub node, that the password is in neither the request line nor the status stream,
and that `split_userinfo` handles a password containing `@` and an `@` in the path.

### Dual Logging

```rust
info!("Bitcoin RPC client {} executing: {}", client_id, method);  // → netget.log
status_tx.send("[CLIENT] Bitcoin RPC client connected");         // → TUI
```

### Error Handling

- **Connection Failed**: Initialization error, client not created
- **RPC Failed**: Log error, return Err, don't crash client
- **Timeout**: reqwest handles with 60s timeout (longer than HTTP default)
- **LLM Error**: Log, continue accepting actions
- **JSON-RPC Error**: Returned in `error` field of response event

## Features

### Supported RPC Methods

**Implemented as Actions:**

- ✅ getblockchaininfo
- ✅ getblockhash
- ✅ getblock
- ✅ getrawtransaction
- ✅ getmempoolinfo
- ✅ getrawmempool
- ✅ getmininginfo
- ✅ getnetworkinfo
- ✅ getpeerinfo
- ✅ getconnectioncount
- ✅ getwalletinfo
- ✅ getbalance
- ✅ listtransactions
- ✅ Generic `execute_rpc` for any method

**Via execute_rpc:**

- Any Bitcoin Core RPC method (v0.21+)

### Bitcoin Core Compatibility

- **Tested:** Bitcoin Core v21.0+
- **Networks:** Mainnet, Testnet, Regtest, Signet
- **Authentication:** HTTP Basic Auth (username/password)

## Limitations

- **No Transaction Signing** - Requires wallet unlocking, complex security
- **No P2P Protocol** - Only JSON-RPC, not Bitcoin P2P wire protocol
- **No Block Streaming** - Full blocks buffered in memory
- **No Watch-Only Addresses** - Wallet operations only
- **No Multi-Wallet** - Single default wallet
- **No ZMQ Subscriptions** - Polling only, no real-time notifications

## Usage Examples

### Query Blockchain Info

**User**: "Connect to Bitcoin Core at http://user:pass@localhost:8332 and get blockchain info"

**LLM Action**:

```json
{
  "type": "get_blockchain_info"
}
```

**Response**:

```json
{
  "result": {
    "chain": "main",
    "blocks": 750000,
    "difficulty": 35364065900457.32,
    "verificationprogress": 0.9999
  }
}
```

### Query Block by Height

**User**: "Get block at height 700000"

**LLM Action 1**:

```json
{
  "type": "get_block_hash",
  "height": 700000
}
```

**LLM Action 2** (follow-up):

```json
{
  "type": "get_block",
  "block_hash": "00000000000000000005f8920febd3925f8272a6a71237563d78c2edfdd09dcd",
  "verbosity": 1
}
```

### Monitor Mempool

**User**: "Check mempool status every 10 seconds"

**LLM Action**:

```json
{
  "type": "get_mempool_info"
}
```

**Response**:

```json
{
  "result": {
    "size": 15234,
    "bytes": 8234567,
    "usage": 45678900,
    "maxmempool": 300000000
  }
}
```

### Custom RPC Call

**User**: "Get the best block hash"

**LLM Action**:

```json
{
  "type": "execute_rpc",
  "method": "getbestblockhash",
  "params": []
}
```

## Testing Strategy

See `tests/client/bitcoin/CLAUDE.md` for E2E testing approach.

## Future Enhancements

- **Transaction Broadcasting** - Submit raw transactions
- **Address Watching** - Monitor specific addresses
- **ZMQ Subscriptions** - Real-time block/transaction notifications
- **Multi-Wallet Support** - Switch between wallets
- **P2P Client Mode** - Direct Bitcoin P2P protocol (hard, see CLIENT_PROTOCOL_FEASIBILITY.md)
- **Lightning Network** - LN RPC integration

## Security Considerations

- **Credentials in URL** - RPC user/pass visible in logs (use environment variables in production)
- **Wallet Operations** - Can send transactions if wallet unlocked
- **Network Exposure** - Only connect to trusted Bitcoin Core nodes
- **Rate Limiting** - Bitcoin Core may rate-limit RPC calls

## Command channel (the dashboard's `[ send ]`)

`AppState::send_to_client` injects an action into a running Bitcoin RPC client. The handle is
registered **before** the `bitcoin_connected` LLM call, because a dashboard-created client
defaults to a `*` → manual rule and that call can park for minutes waiting for a human.

The old "poll `get_client()` every 5 s and exit when it is gone" task is gone: `remove_client`
drops the command sender, so the command loop's `recv()` returns `None` the moment the client
goes away, and that loop is now the client's only long-lived task. Both the connected-event
handler and the command loop go through one `apply_action`, so the `bitcoin_rpc` decoding
exists once — before this, the connected handler inlined its own copy of it.

**Outcome semantics — `Executed`, never `Sent`.** reqwest owns the socket and never reports
how many bytes a request serialised to, so a `Sent { bytes_sent }` here would be a number
someone made up. The command loop **awaits** the JSON-RPC round-trip and reports
`Executed { detail: "bitcoin_rpc 'getblockchaininfo' -> HTTP 200 (result)" }`; a request that
never completes is an `Err`, an unknown action is `Rejected`, and `disconnect` is
`Disconnected` (the loop ends, the status goes to `Disconnected` and the handle is dropped).

The `bitcoin_response_received` event still fires, but from its own registered task rather
than inline — otherwise a manual rule parking that LLM call would wedge the command loop for
the length of a human's think time and `send_to_client` would time out on an RPC that in fact
succeeded. `execute_rpc_command` is unchanged for callers; it is now `perform_rpc` (network
only) followed by `notify_response` (the LLM event).
