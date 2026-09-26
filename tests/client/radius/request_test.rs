//! The RADIUS client's packet construction against RFC-derived values, and what it refuses
//! before anything reaches the wire.
//!
//! Run with:
//!   ./cargo-isolated.sh test --no-default-features --features radius --test client -- radius::request_test --test-threads=100

use netget::client::radius::actions::{request_from_action, Method, RadiusRequest};
use netget::client::radius::wire::{
    access_request, chap_password, hmac_md5, status_server, verify_reply, Credential,
};
use netget::server::radius::packet::{
    decode_user_password, RadiusPacket, ATTR_CHAP_PASSWORD, ATTR_MESSAGE_AUTHENTICATOR,
    ATTR_USER_PASSWORD, CODE_ACCESS_REQUEST,
};
use serde_json::json;

#[test]
fn hmac_md5_matches_rfc_2104_test_vectors() {
    // RFC 2104 Appendix / RFC 2202 test case 2.
    assert_eq!(
        hex::encode(hmac_md5(b"Jefe", b"what do ya want for nothing?")),
        "750c783e6ab0b503eaa86e310a5db738"
    );
    // RFC 2202 test case 6: a key longer than the block is hashed first.
    assert_eq!(
        hex::encode(hmac_md5(
            &[0xaa; 80],
            b"Test Using Larger Than Block-Size Key - Hash Key First"
        )),
        "6b1ab7fe4bd7bf8f0b62e6ce61b9d0cd"
    );
}

#[test]
fn an_access_request_hides_the_password_and_signs_itself() {
    let ra = [7u8; 16];
    let pkt = access_request(9, &ra, Credential::Pap(b"wonderland"), &[], b"secret").unwrap();
    let decoded = RadiusPacket::decode(&pkt).unwrap();
    assert_eq!(decoded.code, CODE_ACCESS_REQUEST);
    // The hidden password decodes back with the shared codec, and is not in the packet in clear.
    let hidden = decoded.first(ATTR_USER_PASSWORD).unwrap();
    assert_eq!(
        decode_user_password(hidden, &ra, b"secret").unwrap(),
        b"wonderland"
    );
    assert!(!pkt.windows(10).any(|w| w == b"wonderland"));
    // The Message-Authenticator is the HMAC over the packet with itself zeroed.
    let ma = decoded.first(ATTR_MESSAGE_AUTHENTICATOR).unwrap().to_vec();
    let mut zeroed = pkt.clone();
    let at = pkt.len() - 16;
    zeroed[at..].fill(0);
    assert_eq!(hmac_md5(b"secret", &zeroed).to_vec(), ma);

    let chap = access_request(9, &ra, Credential::Chap(b"wonderland"), &[], b"secret").unwrap();
    let decoded = RadiusPacket::decode(&chap).unwrap();
    assert_eq!(
        decoded.first(ATTR_CHAP_PASSWORD).unwrap(),
        chap_password(9, b"wonderland", &ra).as_slice()
    );
    assert!(decoded.first(ATTR_USER_PASSWORD).is_none());
}

#[test]
fn a_reply_to_the_wrong_request_code_is_refused() {
    // A Status-Server answered with an Access-Request: not a reply at all.
    let ra = [1u8; 16];
    let probe = status_server(3, &ra, b"secret").unwrap();
    assert_eq!(
        verify_reply(&probe, CODE_ACCESS_REQUEST, &ra, b"secret")
            .unwrap_err()
            .kind(),
        "unexpected_code"
    );
}

#[test]
fn actions_become_requests_and_reserved_attributes_are_refused() {
    let r = request_from_action(&json!({
        "type": "radius_access_request", "user_name": "bob", "password": "pw", "method": "chap",
        "attributes": {"NAS-Port": 3, "Framed-IP-Address": "10.0.0.9", "Called-Station-Id": "ap"}
    }))
    .unwrap()
    .unwrap();
    let RadiusRequest::Access {
        method, attributes, ..
    } = r
    else {
        panic!("expected an Access-Request")
    };
    assert_eq!(method, Method::Chap("pw".into()));
    assert_eq!(
        attributes.len(),
        5,
        "User-Name, NAS-Identifier and three extras"
    );

    // The attributes the client computes itself are refused *as such* — not merely because
    // their dictionary type happens to be an octet string — so the model is told why.
    for reserved in [
        "User-Password",
        "CHAP-Password",
        "Message-Authenticator",
        "State",
        "Proxy-State",
        "EAP-Message",
    ] {
        let err = request_from_action(&json!({
            "type": "radius_access_request", "user_name": "bob", "attributes": {reserved: "x"}
        }))
        .unwrap_err()
        .to_string();
        assert!(
            err.contains("set by the client itself"),
            "{reserved}: expected the reserved-attribute refusal, got {err:?}"
        );
    }

    for bad in [
        json!({"type": "radius_access_request", "user_name": "bob",
               "attributes": {"User-Password": "x"}}),
        json!({"type": "radius_access_request", "user_name": "bob",
               "attributes": {"Message-Authenticator": "00"}}),
        json!({"type": "radius_access_request", "user_name": "bob",
               "attributes": {"State": "00"}}),
        json!({"type": "radius_access_request", "user_name": "bob",
               "attributes": {"NAS-Port": 4294967296u64}}),
        json!({"type": "radius_access_request", "user_name": "bob",
               "attributes": {"No-Such-Attribute": 1}}),
        json!({"type": "radius_access_request", "user_name": "bob", "password": "p".repeat(129)}),
        json!({"type": "radius_access_request", "user_name": "bob", "password": "x",
               "method": "mschap"}),
        json!({"type": "radius_accounting_request", "status_type": "Begin", "session_id": "s"}),
        json!({"type": "radius_accounting_request", "status_type": "Start"}),
    ] {
        assert!(request_from_action(&bad).is_err(), "{bad} must be refused");
    }
}
