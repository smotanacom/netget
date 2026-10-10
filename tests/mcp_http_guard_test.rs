//! The admission policy on `--mcp-http`'s `/mcp` endpoint, decided from headers alone.
//!
//! `tests/mcp_http_transport_test.rs` proves the policy is wired in front of the real
//! transport; this file pins what the policy says, case by case, with no socket.
#![cfg(feature = "mcp-http")]
use http::header::{AUTHORIZATION, CONTENT_LENGTH, HOST, ORIGIN};
use http::{HeaderMap, HeaderValue, StatusCode, Uri};
use netget::mcp_stdio::http_guard::{HttpGuard, MAX_REQUEST_BODY_BYTES};

fn headers(pairs: &[(&http::HeaderName, &str)]) -> HeaderMap {
    let mut map = HeaderMap::new();
    for (name, value) in pairs {
        map.insert((*name).clone(), HeaderValue::from_str(value).unwrap());
    }
    map
}

fn loopback() -> HttpGuard {
    HttpGuard::new("127.0.0.1".parse().unwrap(), None).unwrap()
}

fn with_token() -> HttpGuard {
    HttpGuard::new("0.0.0.0".parse().unwrap(), Some("s3cret".into())).unwrap()
}

fn uri() -> Uri {
    "/mcp".parse().unwrap()
}

#[test]
fn loopback_mode_admits_local_hosts_and_origins() {
    let guard = loopback();
    for host in [
        "127.0.0.1:8080",
        "localhost:8080",
        "LOCALHOST",
        "[::1]:8080",
        "127.0.0.1",
    ] {
        assert_eq!(
            guard.check(&headers(&[(&HOST, host)]), &uri()),
            Ok(()),
            "{host}"
        );
    }
    for origin in [
        "http://localhost:3000",
        "http://127.0.0.1",
        "http://[::1]:9",
    ] {
        assert_eq!(
            guard.check(
                &headers(&[(&HOST, "127.0.0.1:8080"), (&ORIGIN, origin)]),
                &uri()
            ),
            Ok(()),
            "{origin}"
        );
    }
}

#[test]
fn loopback_mode_refuses_a_rebound_host() {
    let guard = loopback();
    for host in [
        "attacker.example:8080",
        "10.0.0.5:8080",
        "127.0.0.1.attacker.example",
    ] {
        let refusal = guard
            .check(&headers(&[(&HOST, host)]), &uri())
            .expect_err(host);
        assert_eq!(refusal.status, StatusCode::FORBIDDEN, "{host}");
    }
    let refusal = guard
        .check(&HeaderMap::new(), &uri())
        .expect_err("no host at all");
    assert_eq!(refusal.status, StatusCode::FORBIDDEN);
}

#[test]
fn loopback_mode_reads_the_h2_authority_when_there_is_no_host_header() {
    let guard = loopback();
    let local: Uri = "http://127.0.0.1:8080/mcp".parse().unwrap();
    assert_eq!(guard.check(&HeaderMap::new(), &local), Ok(()));
    let rebound: Uri = "http://attacker.example:8080/mcp".parse().unwrap();
    assert_eq!(
        guard.check(&HeaderMap::new(), &rebound).unwrap_err().status,
        StatusCode::FORBIDDEN
    );
}

#[test]
fn loopback_mode_refuses_a_foreign_origin() {
    let guard = loopback();
    for origin in [
        "http://attacker.example",
        "https://localhost.attacker.example",
        "null",
        "http://127.0.0.1.attacker.example:8080",
    ] {
        let refusal = guard
            .check(
                &headers(&[(&HOST, "127.0.0.1:8080"), (&ORIGIN, origin)]),
                &uri(),
            )
            .expect_err(origin);
        assert_eq!(refusal.status, StatusCode::FORBIDDEN, "{origin}");
    }
}

#[test]
fn the_literal_bind_address_counts_as_local() {
    let guard = HttpGuard::new("127.0.0.2".parse().unwrap(), None).unwrap();
    assert_eq!(
        guard.check(&headers(&[(&HOST, "127.0.0.2:8080")]), &uri()),
        Ok(())
    );
}

#[test]
fn a_non_loopback_bind_needs_a_token() {
    let err = HttpGuard::new("0.0.0.0".parse().unwrap(), None).unwrap_err();
    assert!(err.to_string().contains("--mcp-token"), "{err}");
    assert!(HttpGuard::new("::".parse().unwrap(), None).is_err());
    assert!(HttpGuard::new("0.0.0.0".parse().unwrap(), Some("t".into())).is_ok());
    assert!(HttpGuard::new("0.0.0.0".parse().unwrap(), Some(String::new())).is_err());
    assert!(HttpGuard::new("0.0.0.0".parse().unwrap(), Some("a b".into())).is_err());
}

#[test]
fn token_mode_requires_the_exact_bearer_token_and_ignores_host_and_origin() {
    let guard = with_token();
    let good = headers(&[
        (&HOST, "attacker.example"),
        (&ORIGIN, "http://attacker.example"),
        (&AUTHORIZATION, "Bearer s3cret"),
    ]);
    assert_eq!(guard.check(&good, &uri()), Ok(()));
    assert_eq!(
        guard.check(&headers(&[(&AUTHORIZATION, "bearer s3cret")]), &uri()),
        Ok(()),
        "scheme is case-insensitive"
    );
    for bad in [
        "Bearer s3cre",
        "Bearer s3cret2",
        "Bearer S3CRET",
        "Basic s3cret",
        "s3cret",
        "Bearer ",
    ] {
        let refusal = guard
            .check(
                &headers(&[(&HOST, "127.0.0.1"), (&AUTHORIZATION, bad)]),
                &uri(),
            )
            .expect_err(bad);
        assert_eq!(refusal.status, StatusCode::UNAUTHORIZED, "{bad}");
    }
    let refusal = guard
        .check(&headers(&[(&HOST, "127.0.0.1")]), &uri())
        .expect_err("no Authorization at all");
    assert_eq!(refusal.status, StatusCode::UNAUTHORIZED);
}

#[test]
fn a_declared_oversized_body_is_refused_before_it_is_read() {
    let too_big = (MAX_REQUEST_BODY_BYTES + 1).to_string();
    let exactly = MAX_REQUEST_BODY_BYTES.to_string();
    for guard in [loopback(), with_token()] {
        let refusal = guard
            .check(
                &headers(&[
                    (&HOST, "127.0.0.1"),
                    (&AUTHORIZATION, "Bearer s3cret"),
                    (&CONTENT_LENGTH, &too_big),
                ]),
                &uri(),
            )
            .unwrap_err();
        assert_eq!(refusal.status, StatusCode::PAYLOAD_TOO_LARGE);
        assert_eq!(
            guard.check(
                &headers(&[
                    (&HOST, "127.0.0.1"),
                    (&AUTHORIZATION, "Bearer s3cret"),
                    (&CONTENT_LENGTH, &exactly),
                ]),
                &uri(),
            ),
            Ok(())
        );
    }
}
