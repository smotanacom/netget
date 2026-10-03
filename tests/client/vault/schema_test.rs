use netget::client::vault::api;
use serde_json::{json, Value};
const TOKEN: &str = "hvs.fixture-token-not-for-logs";
const PASSWORD: &str = "fixture-password-not-for-logs";
const TIME: &str = "2026-10-02T12:34:56.123456Z";
fn envelope(data: Value) -> Value {
    json!({"request_id":"fixture-request","lease_id":"","lease_duration":0,"renewable":false,
        "data":data,"auth":null,"wrap_info":null,"warnings":null})
}
fn version() -> Value {
    json!({"version":2,"created_time":TIME,"deletion_time":"","destroyed":false,"custom_metadata":{"owner":"fixture"}})
}
fn login() -> Value {
    let mut v = envelope(json!(null));
    v["auth"] = json!({"client_token":TOKEN,"accessor":"fixture-accessor","policies":["default","fixture"],
        "token_policies":["default","fixture"],"metadata":{"username":"reader"},"lease_duration":3600,
        "renewable":true,"mfa_requirement":null,"entity_id":"fixture-entity","token_type":"service","orphan":true,"num_uses":0});
    v
}
#[test]
fn typed_routes_preserve_cas_version_and_native_data_envelopes() {
    let write = json!({"type":"vault_request","operation":"write","mount":"team/kv","path":"fixture/app","data":{"answer":42},"cas":0});
    let r = api::request(&write, "secret", "userpass").unwrap();
    assert_eq!(
        (r.method, r.path.as_str()),
        ("PUT", "/v1/team/kv/data/fixture/app")
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&r.body).unwrap(),
        json!({"data":{"answer":42},"options":{"cas":0}})
    );
    let read = api::request(
        &json!({"type":"vault_request","operation":"read","path":"fixture/app","version":0}),
        "secret",
        "userpass",
    )
    .unwrap();
    assert_eq!(read.path, "/v1/secret/data/fixture/app?version=0");
    let list = api::request(
        &json!({"type":"vault_request","operation":"list","path":""}),
        "secret",
        "userpass",
    )
    .unwrap();
    assert_eq!(list.path, "/v1/secret/metadata/?list=true");
    let folder = api::request(
        &json!({"type":"vault_request","operation":"list","path":"fixture/"}),
        "secret",
        "userpass",
    )
    .unwrap();
    assert_eq!(folder.path, "/v1/secret/metadata/fixture/?list=true");
    for action in [
        json!({"operation":"read","path":"../a"}),
        json!({"operation":"read","path":"a%2fb"}),
        json!({"operation":"read","path":"a//b"}),
        json!({"operation":"read","path":"a","version":-1}),
        json!({"operation":"write","path":"a","data":[],"cas":0}),
        json!({"operation":"write","path":"a","data":{},"cas":"1"}),
        json!({"operation":"list","path":"a","method":"LIST"}),
        json!({"operation":"health","url":"http://other"}),
        json!({"operation":"delete","path":"a"}),
        json!({"operation":"read","path":"a","mount":"sys"}),
    ] {
        let mut action = action;
        action["type"] = json!("vault_request");
        assert!(
            api::request(&action, "secret", "userpass").is_err(),
            "{action}"
        );
    }
}
#[test]
fn userpass_receipts_omit_password_and_validated_auth_keeps_honest_metadata() {
    let r = api::request(&json!({"type":"vault_userpass_login","username":"fixture-reader","password":PASSWORD,"auth_mount":"team/userpass"}),"secret","userpass").unwrap();
    assert_eq!(
        (r.method, r.path.as_str()),
        ("POST", "/v1/auth/team/userpass/login/fixture-reader")
    );
    assert_eq!(
        serde_json::from_slice::<Value>(&r.body).unwrap(),
        json!({"password":PASSWORD})
    );
    assert!(r.redacted.get("password").is_none());
    assert_eq!(r.redacted["password_present"], true);
    let (v, token) = api::parse("login", login()).unwrap();
    assert_eq!(token.as_ref().unwrap().to_str().unwrap(), TOKEN);
    assert!(token.unwrap().is_sensitive());
    assert!(v["data"].get("client_token").is_none());
    assert_eq!(v["data"]["token_type"], "service");
    assert_eq!(v["data"]["lease_duration"], 3600);
    assert_eq!(v["data"]["token_policies"], json!(["default", "fixture"]));
    assert_eq!(v["data"]["metadata"]["username"], "reader");
    for password in ["", &"x".repeat(api::MAX_TEXT + 1)] {
        assert!(api::request(
            &json!({"type":"vault_userpass_login","username":"reader","password":password}),
            "secret",
            "userpass"
        )
        .is_err());
    }
    for token in ["", "has space", "has\nnewline"] {
        assert!(api::token(token).is_err());
    }
}
#[test]
fn kv_reads_writes_lists_and_metadata_keep_distinct_schemas() {
    let (read, _) = api::parse(
        "read",
        envelope(json!({"data":{"answer":42},"metadata":version()})),
    )
    .unwrap();
    assert_eq!(read["data"]["data"]["answer"], 42);
    assert_eq!(read["data"]["metadata"]["version"], 2);
    let (write, _) = api::parse("write", envelope(version())).unwrap();
    assert_eq!(write["data"]["version"], 2);
    assert!(write["data"].get("data").is_none());
    let (list, _) = api::parse("list", envelope(json!({"keys":["app","folder/"]}))).unwrap();
    assert_eq!(list["data"]["keys"], json!(["app", "folder/"]));
    let (metadata,_)=api::parse("metadata",envelope(json!({"created_time":TIME,"updated_time":TIME,"current_version":2,"oldest_version":0,"max_versions":0,"cas_required":true,"delete_version_after":"0s","custom_metadata":null,"versions":{"1":{"created_time":TIME,"deletion_time":"","destroyed":false},"2":{"created_time":TIME,"deletion_time":"","destroyed":true,"created_by":{"actor":"reader","operation":"write"}}}}))).unwrap();
    assert_eq!(metadata["data"]["versions"]["2"]["destroyed"], true);
    assert_eq!(metadata["data"]["cas_required"], true);
    assert!(api::parse("read", envelope(version())).is_err());
    assert!(api::parse("write", envelope(json!({"data":{},"metadata":version()}))).is_err());
}
#[test]
fn malformed_or_unavailable_versions_do_not_become_success() {
    for (key, value) in [
        ("version", json!(0)),
        ("version", json!("2")),
        ("created_time", json!("yesterday")),
        ("destroyed", json!(true)),
        ("deletion_time", json!(TIME)),
    ] {
        let mut metadata = version();
        metadata[key] = value;
        assert!(api::parse(
            "read",
            envelope(json!({"data":{},"metadata":metadata.clone()}))
        )
        .is_err());
        assert!(api::parse("write", envelope(metadata)).is_err());
    }
    for keys in [json!(["a/b"]), json!([""]), json!(["bad\nkey"]), json!([3])] {
        assert!(api::parse("list", envelope(json!({"keys":keys}))).is_err());
    }
}
#[test]
fn auth_wrapping_dynamic_leases_and_mfa_are_refused_without_exposing_tokens() {
    for (pointer, value) in [
        ("/wrap_info", json!({"token":TOKEN})),
        ("/lease_id", json!("dynamic")),
        ("/lease_duration", json!(3)),
        ("/renewable", json!(true)),
        (
            "/auth/mfa_requirement",
            json!({"mfa_request_id":"challenge"}),
        ),
        ("/auth/token_type", json!("unknown")),
        ("/auth/client_token", json!("invalid token")),
    ] {
        let mut v = login();
        *v.pointer_mut(pointer)
            .unwrap_or_else(|| panic!("missing fixture pointer {pointer}")) = value;
        assert!(api::parse("login", v).is_err(), "{pointer}");
    }
    let mut v = envelope(version());
    v["auth"] = login()["auth"].clone();
    assert!(api::parse("write", v).is_err());
}
#[test]
fn selected_system_schemas_preserve_sealed_and_standby_meaning() {
    let health = json!({"initialized":true,"sealed":true,"standby":true,"performance_standby":false,"server_time_utc":1700000000,"version":"2.0.0","extension":"omit"});
    let (v, _) = api::parse("health", health).unwrap();
    assert_eq!(v["sealed"], true);
    assert_eq!(v["standby"], true);
    assert!(v.get("extension").is_none());
    let seal = json!({"type":"shamir","initialized":true,"sealed":false,"t":1,"n":1,"progress":0,"nonce":"","version":"2.0.0"});
    let (v, _) = api::parse("seal_status", seal.clone()).unwrap();
    assert_eq!(v["seal_type"], "shamir");
    let mut invalid = seal;
    invalid["progress"] = json!(2);
    assert!(api::parse("seal_status", invalid).is_err());
    assert!(api::parse("health", json!({"initialized":true})).is_err());
}
#[test]
fn streaming_json_limits_duplicates_and_trailing_content_are_enforced() {
    assert!(api::json(&vec![b' '; api::MAX_BODY + 1]).is_err());
    assert!(api::json(&serde_json::to_vec(&vec![0; api::MAX_ITEMS + 1]).unwrap()).is_err());
    assert!(api::json(&serde_json::to_vec(&"x".repeat(api::MAX_TEXT + 1)).unwrap()).is_err());
    let mut nested = json!(0);
    for _ in 0..34 {
        nested = json!([nested]);
    }
    assert!(api::json(&serde_json::to_vec(&nested).unwrap()).is_err());
    assert!(api::json(br#"{"key":1,"key":2}"#).is_err());
    assert!(api::json(b"{} {}").is_err());
    let fields = (0..257)
        .map(|n| (format!("key{n}"), json!(n)))
        .collect::<serde_json::Map<_, _>>();
    assert!(api::json(&serde_json::to_vec(&fields).unwrap()).is_err());
}
#[test]
fn known_credentials_are_hidden_in_escaped_errors_and_free_form_keys() {
    let password = "quoted \"password\"\ncontrol";
    let secrets = vec![password.into(), TOKEN.into()];
    let escaped = serde_json::to_string(password).unwrap();
    let shown = api::redact_text(
        &format!("raw {password}; schema {escaped}; token {TOKEN}"),
        &secrets,
    );
    assert!(!shown.contains(password));
    assert!(!shown.contains(&escaped[1..escaped.len() - 1]));
    assert!(!shown.contains(TOKEN));
    let mut value =
        json!({"data":{"metadata":{TOKEN:password},"token_type":"service"},"token_present":true});
    api::redact_response(&mut value, "login", &secrets);
    assert!(!value.to_string().contains(TOKEN));
    assert_eq!(value["data"]["token_type"], "service");
    assert_eq!(value["token_present"], true);
    for password in ["hvs.", "r", "e", "d", "a", "prefix-hvs.fixture"] {
        let secrets = vec![password.into(), TOKEN.into()];
        assert_eq!(api::redact_text(TOKEN, &secrets), "<redacted>");
        assert_eq!(
            api::redact_text(&format!("prefix-{TOKEN}"), &secrets),
            "<redacted>"
        );
        assert_eq!(
            api::redact_errors(
                &[format!("reflected {password} {TOKEN}"), "".into()],
                &secrets
            ),
            ["<redacted>", ""]
        );
    }
    assert_eq!(
        api::redact_text("plain diagnostic", &[]),
        "plain diagnostic"
    );
    let secrets = vec!["r".into(), TOKEN.into()];
    let mut login = json!({"request_id":"native-id","data":{"renewable":true,"token_type":"service","metadata":{TOKEN:"r"}},"warnings":[]});
    api::redact_response(&mut login, "login", &secrets);
    assert_eq!(login["data"]["renewable"], true);
    assert_eq!(login["data"]["metadata"]["<redacted>"], "<redacted>");
    let mut read = json!({"data":{"data":{TOKEN:"r"},"metadata":{"version":1,"destroyed":false,"custom_metadata":{TOKEN:"r"}}}});
    api::redact_response(&mut read, "read", &secrets);
    assert_eq!(read["data"]["metadata"]["version"], 1);
    assert_eq!(read["data"]["metadata"]["destroyed"], false);
    assert_eq!(read["data"]["data"]["<redacted>"], "<redacted>");
    assert_eq!(
        read["data"]["metadata"]["custom_metadata"]["<redacted>"],
        "<redacted>"
    );
}
