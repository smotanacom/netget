# mDNS Protocol E2E Tests

## Test Overview

Tests mDNS service advertisement with real mDNS-SD client library (`mdns-sd`) validating service discovery and TXT
record properties.

## Test Strategy

- **Consolidated per feature** - Each test focuses on a specific mDNS capability
- **Multiple server instances** - 4 separate servers (one per test)
- **Real mDNS client** - Uses `mdns-sd` library for protocol correctness
- **Discovery-based validation** - Tests verify services are advertised and discoverable
- **No scripting** - Action-based service registration only

## LLM Call Budget

- `test_mdns_service_advertisement()`: 1 startup call (single service registration)
- `test_mdns_multiple_services()`: 1 startup call (multiple service registrations)
- `test_mdns_service_with_properties()`: 1 startup call (service with TXT properties)
- `test_mdns_custom_service_type()`: 1 startup call (custom service type)
- **Total: 4 LLM calls** (4 startups, 0 subsequent calls)

**Well under 10 LLM call limit** - mDNS is startup-only, no per-request processing.

## Scripting Usage

**Scripting Disabled** - mDNS uses action-based service registration

- Services registered once at startup via `register_mdns_service` action
- No ongoing network events to script
- LLM interprets user prompt and returns service definitions

## Client Library

**mdns-sd** v0.15 - Multicast DNS service discovery

- `ServiceDaemon::new()` - Create mDNS daemon
- `browse(service_type)` - Browse for services of a type
- `recv_async()` - Receive service events asynchronously
- Events: `ServiceFound`, `ServiceResolved(ResolvedService)`, `ServiceRemoved`

**This is circular evidence**, and it is why `mdns` is Experimental: `mdns-sd` is
also the crate the *server* registers through, so the suite asserts that one crate
round-trips through itself. It does genuinely cover NetGet's registration plumbing;
it does not cover interop. An independent client (`dns-sd -L` on macOS,
`avahi-browse -r` on Linux) is what a Beta rating would need.

## Expected Runtime

- Model: qwen3-coder:30b
- Runtime: ~40-60 seconds for full test suite
- Fast due to only 4 LLM calls
- Most time spent waiting for mDNS service resolution (10-second timeouts per test)

## Failure Rate

- **Medium** (10-15%) - mDNS discovery can be flaky
- Common issues:
    - Service not discovered within timeout (network/timing)
    - LLM returns incorrect service_type format
    - LLM omits required fields (port, instance_name)
    - Firewall blocking multicast traffic (224.0.0.251)

## Test Cases

1. **test_mdns_service_advertisement** - Tests basic service registration and discovery
2. **test_mdns_multiple_services** - Tests registering multiple services simultaneously
3. **test_mdns_service_with_properties** - Tests TXT record properties
4. **test_mdns_custom_service_type** - Tests custom service types (non-standard)

## Known Issues

- **Discovery timing** - Tests wait up to 20 seconds for service resolution, on the
  *condition* rather than on a fixed sleep, so a fast machine returns immediately
- **Flaky on some networks** - Multicast may be blocked or delayed. Where it is, these
  tests now fail rather than passing quietly, which is the point
- Circular evidence - see "Client Library" above

### Fixed in September 2026, and worth not reintroducing

Three defects, all of which made a green run meaningless:

1. **Nothing was asserted.** Every test computed a `found_service` flag and then
   *printed* it: `if found { println!("verified") } else { println!("Note: not
   discovered") }`. `verify_mocks()` was the only real check, and it proves the startup
   LLM call happened - not that a byte reached the group. This section used to record
   "Tests may pass even if properties are missing" as a known issue; it was the whole
   problem.
2. **A neighbour's advertisement counted as success.** Browsing `_http._tcp.local.`
   returns every such service on the link, these four tests run concurrently, and two of
   them advertise into it. A captured run had `test_mdns_service_advertisement` report
   `Instance: Web Service._http._tcp.local.` - the *other* test's service - and call
   itself verified. Every wait now matches the instance name it registered, and the
   instance names are distinct.
3. **The wait loop broke on the wrong event.** `ServiceFound` arrives before
   `ServiceResolved`, and the `_ => break` arm treated it as end-of-stream, abandoning a
   service that was about to resolve. This section used to describe accepting
   `ServiceFound` as acceptable; it is not - only `ServiceResolved` carries the SRV/TXT
   data the assertions read.

## Example Test Pattern

```rust
// Start server with service registration prompt
let server = start_netget_server(ServerConfig::new(prompt)).await?;

// Create mDNS browser
let mdns = mdns_sd::ServiceDaemon::new()?;
let service_type = "_http._tcp.local.";
let receiver = mdns.browse(service_type)?;

// Poll for service discovery (with timeout)
let mut found_service = false;
let timeout_duration = Duration::from_secs(10);
let start = Instant::now();

while start.elapsed() < timeout_duration {
    match tokio::time::timeout(Duration::from_secs(2), async {
        receiver.recv_async().await
    }).await {
        Ok(Ok(event)) => {
            match event {
                ServiceEvent::ServiceResolved(info) => {
                    // Service fully resolved with IP/port
                    assert_eq!(info.get_port(), expected_port);
                    found_service = true;
                    break;
                }
                ServiceEvent::ServiceFound(ty, fullname) => {
                    // Service found but not yet resolved
                    // May be sufficient for some tests
                }
                _ => {}
            }
        }
        _ => continue, // Timeout or error, keep polling
    }
}

assert!(found_service, "Service should be discovered");

// Cleanup
mdns.shutdown();
server.stop().await?;
```

## mDNS Discovery Flow

1. **Advertiser** (NetGet) registers service
2. **Browser** (test client) sends multicast query for service type
3. **Advertiser** responds with PTR, SRV, TXT, A records
4. **Browser** receives `ServiceFound` event (quick)
5. **Browser** resolves SRV/A records → `ServiceResolved` event (may take seconds)

## Performance Considerations

- mDNS uses multicast (224.0.0.251:5353)
- Service resolution can take 1-5 seconds depending on network
- Tests use generous 10-second timeouts to avoid flakes
- Multiple services discovered independently (no batching)

## Network Requirements

- **Multicast support** - Network must allow 224.0.0.251
- **Local network only** - mDNS is link-local (not routed)
- **Firewall rules** - UDP port 5353 must be allowed
- **Docker/VM** - May have issues with multicast forwarding

## Service Type Format

Service types must follow DNS-SD naming:

- Format: `_<service>._<proto>.local.`
- Examples:
    - `_http._tcp.local.` - HTTP service
    - `_ftp._tcp.local.` - FTP service
    - `_myapp._tcp.local.` - Custom application
- **Trailing period required** - `local.` not `local`
- **Lowercase** - Service types are case-insensitive but lowercase preferred

## TXT Record Properties

Properties are key-value pairs:

- Max 255 bytes per property
- Keys typically lowercase
- Values as strings
- Example: `{"version": "1.0", "path": "/api"}`
