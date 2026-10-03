# FTP Client E2E Tests

## Test Strategy

Black-box testing using mock FTP servers to verify client connectivity and command sending.

## LLM Call Budget

- **Target**: < 5 LLM calls per test
- **Current**: 1 LLM call per test (client setup only)

## Test Cases

| Test | Description | LLM Calls | Expected Runtime |
|------|-------------|-----------|------------------|
| `test_ftp_client_connect` | Verify client can connect and receive greeting | 1 | ~2s |
| `test_ftp_client_send_command` | Verify client can send USER command | 1 | ~3s |
| `command_channel_test::injected_ftp_command_reaches_our_own_server` | `send_to_client` injects `send_ftp_command` into a running client against a NetGet FTP server (static handler); bad action rejected; `disconnect` drops the handle | 0 | ~4s |

## Mock Configuration

All tests use:
1. Mock LLM responses via `.with_mock()` builder
2. Mock FTP servers (TcpListener) for controlled testing
3. No actual Ollama or FTP servers required

## Running Tests

```bash
# Run with mocks (default, no Ollama needed)
./test-e2e.sh ftp

# Run with real Ollama
./test-e2e.sh --use-ollama ftp

# Run with cargo
./cargo-isolated.sh test --no-default-features --features ftp --test client::ftp::test
```

## Known Issues

1. **Mock Server Simplicity**: Mock servers only handle basic FTP interactions
2. **No Real FTP Server Tests**: Tests use mocks, not real FTP servers

## Test Architecture

```
┌─────────────────┐        ┌─────────────────┐
│  NetGet Client  │───────>│  Mock FTP       │
│  (FTP protocol) │        │  Server         │
└─────────────────┘        └─────────────────┘
        │
        │ LLM calls (mocked)
        ▼
┌─────────────────┐
│  Mock LLM       │
│  Responses      │
└─────────────────┘
```
