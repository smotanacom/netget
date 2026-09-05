//! NetGet must never ask the system resolver about a literal IP address.
//!
//! `reqwest` hands the URL host to its resolver unconditionally, and `hyper-util`'s
//! `GaiResolver` does not special-case a dotted quad — so `http://127.0.0.1:11434` performs a
//! real `getaddrinfo("127.0.0.1")`. On macOS that goes through libinfo to mDNSResponder, one
//! system-wide daemon, which serialises under concurrency: measured at **8.25 seconds** with
//! ~100 processes asking at once.
//!
//! The override that avoids it is only applied when the host parses as an `IpAddr`, so the
//! host-extraction below is load-bearing. Its first version stripped only the scheme, leaving
//! `127.0.0.1:54321`, which does not parse — so the override silently did not engage and the
//! call it was written for kept spending its whole 5-second budget in name resolution. A
//! short-circuit that quietly fails to engage is worse than none, because the symptom is
//! unchanged; that is what these cases guard.
//!
//! Run with:
//!   ./cargo-isolated.sh test --no-default-features --features tcp --test literal_ip_dns_bypass_test

use netget::llm::ollama_client::host_of;

#[test]
fn host_of_extracts_the_bare_host() {
    // The shapes NetGet actually passes, from --ollama-url, --openai-url and the test mocks.
    assert_eq!(host_of("127.0.0.1"), "127.0.0.1");
    assert_eq!(host_of("http://127.0.0.1"), "127.0.0.1");
    assert_eq!(host_of("http://127.0.0.1:11434"), "127.0.0.1");
    assert_eq!(host_of("https://127.0.0.1:11434/v1"), "127.0.0.1");
    assert_eq!(host_of("http://127.0.0.1:11434/"), "127.0.0.1");

    // IPv6 is bracketed, and its own colons must not be read as a port separator.
    assert_eq!(host_of("http://[::1]:11434"), "::1");
    assert_eq!(host_of("[::1]"), "::1");

    // Hostnames come back untouched: resolving them is the resolver's job, and /etc/hosts or
    // split-horizon DNS may legitimately point them somewhere unexpected.
    assert_eq!(host_of("http://localhost:11434"), "localhost");
    assert_eq!(host_of("https://api.openai.com/v1"), "api.openai.com");
}

/// Every shape above that is an IP must parse as one — this is the condition the override is
/// gated on, so it is the thing that actually decides whether the fix runs.
#[test]
fn the_extracted_host_parses_as_an_ip_for_every_literal_form() {
    for url in [
        "127.0.0.1",
        "http://127.0.0.1",
        "http://127.0.0.1:11434",
        "https://127.0.0.1:11434/v1",
        "http://[::1]:11434",
    ] {
        assert!(
            host_of(url).parse::<std::net::IpAddr>().is_ok(),
            "{url} reduces to {:?}, which does not parse as an IpAddr — the DNS bypass would \
             silently not apply",
            host_of(url)
        );
    }
    for url in ["http://localhost:11434", "https://api.openai.com/v1"] {
        assert!(
            host_of(url).parse::<std::net::IpAddr>().is_err(),
            "{url} must keep using the system resolver"
        );
    }
}
