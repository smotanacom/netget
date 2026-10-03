use netget::client::docker::{api_version, request, schema};
use serde_json::json;
#[test]
fn request_builder_encodes_native_filters_and_refuses_passthrough_or_wrong_fields() {
    let action = json!({"type":"docker_request","operation":"containers","all":true,"size":true,"limit":7,"filters":{"label":["owner=a&b=two"],"status":["exited"]}});
    let r = request(&action, "1.47").unwrap();
    assert!(r
        .path
        .starts_with("/v1.47/containers/json?all=true&size=true&limit=7&filters="));
    let encoded = r.path.split("filters=").nth(1).unwrap();
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&urlencoding::decode(encoded).unwrap()).unwrap(),
        action["filters"]
    );
    for v in [
        json!({"operation":"create"}),
        json!({"operation":"info","path":"/containers/create"}),
        json!({"operation":"info","all":true}),
        json!({"operation":"container","container_id":"../x"}),
        json!({"operation":"container","container_id":".."}),
        json!({"operation":"containers","all":"true"}),
        json!({"operation":"containers","limit":0}),
        json!({"operation":"containers","filters":{"label":"x"}}),
        json!({"operation":"volumes","filters":{"name":["a\nb"]}}),
    ] {
        let mut v = v;
        v["type"] = json!("docker_request");
        assert!(request(&v, "1.47").is_err(), "{v}");
    }
    for v in ["+1.47", "1.+47", "1.47.1", "1.9999", "1.", "1. 47"] {
        assert!(api_version(v).is_err(), "{v}");
    }
}
#[test]
fn list_and_inspect_have_distinct_typed_shapes_and_native_nulls() {
    let list = json!([{"Id":"a1b2c3d4e5f6a7b8","Names":["/fixture"],"Image":"alpine:3","ImageID":"sha256:abc","Command":"sleep 1","Created":1700000000,"Ports":[{"PrivatePort":80,"PublicPort":8080,"Type":"tcp","IP":"127.0.0.1"}],"Labels":{"owner":"x"},"State":"exited","Status":"Exited (7)","SizeRw":12,"SizeRootFs":100,"UnrecognizedExtension":"omit"}]);
    let parsed = schema::parse("containers", &serde_json::to_vec(&list).unwrap()).unwrap();
    assert_eq!(parsed[0]["created"], 1700000000);
    assert_eq!(parsed[0]["ports"][0]["protocol"], "tcp");
    assert!(parsed[0].get("UnrecognizedExtension").is_none());
    let mut null_ports = list.clone();
    null_ports[0]["Ports"] = json!(null);
    assert!(
        schema::parse("containers", &serde_json::to_vec(&null_ports).unwrap()).unwrap()[0]["ports"]
            .is_null()
    );
    assert!(schema::parse("container", &serde_json::to_vec(&list).unwrap()).is_err());
    let inspect=netget::server::docker::api::render_container(&json!({"id":"a1b2c3d4e5f6a7b8","name":"fixture","image":"alpine:3","state":"exited","exit_code":7,"env":["NETGET_FIXTURE=1"],"ports":[{"private_port":80,"public_port":8080,"ip":"127.0.0.1"}]})).unwrap();
    let parsed = schema::parse("container", &serde_json::to_vec(&inspect).unwrap()).unwrap();
    assert_eq!(parsed["state"]["exit_code"], 7);
    assert_eq!(parsed["config"]["image"], "alpine:3");
    assert!(parsed["network_settings"]["ports"].is_object());
    let volumes = schema::parse("volumes", br#"{"Volumes":null,"Warnings":null}"#).unwrap();
    assert!(volumes["volumes"].is_null());
    assert!(volumes["warnings"].is_null());
    let image=schema::parse("images",br#"[{"Id":"sha256:abc","RepoTags":null,"RepoDigests":null,"Created":1700000000,"Size":20,"SharedSize":-1,"Containers":-1}]"#).unwrap();
    assert!(image[0]["repo_tags"].is_null());
    assert_eq!(image[0]["shared_size"], -1);
}
#[test]
fn malformed_schemas_and_limits_do_not_synthesize_success() {
    for (op, body) in [
        ("volumes", json!([])),
        ("containers", json!({})),
        (
            "version",
            json!({"Version":"x","ApiVersion":"1.+47","Os":"linux","Arch":"arm64"}),
        ),
        ("images", json!([{"Id":"x","Created":"y","Size":1}])),
        (
            "images",
            json!([{"Id":"x","Created":1,"Size":1,"Containers":-2}]),
        ),
        (
            "volumes",
            json!({"Volumes":[{"Name":"v","Driver":"local","Mountpoint":"/v","Scope":"local","UsageData":{"RefCount":-2,"Size":-1}}]}),
        ),
    ] {
        assert!(
            schema::parse(op, &serde_json::to_vec(&body).unwrap()).is_err(),
            "{op} {body}"
        );
    }
    assert!(schema::json(&serde_json::to_vec(&json!([])).unwrap()).is_ok());
    assert!(schema::json(&serde_json::to_vec(&vec![0; schema::MAX_ITEMS + 1]).unwrap()).is_err());
    assert!(schema::json(&serde_json::to_vec(&"x".repeat(schema::MAX_TEXT + 1)).unwrap()).is_err());
    let mut nested = json!(0);
    for _ in 0..34 {
        nested = json!([nested]);
    }
    assert!(schema::json(&serde_json::to_vec(&nested).unwrap()).is_err());
    assert!(schema::json(&vec![b' '; schema::MAX_BODY + 1]).is_err());
    assert!(schema::json(br#"{"Name":"first","Name":"second"}"#).is_err());
    assert!(schema::json(br#"{} {}"#).is_err());
    assert_eq!(
        schema::error(br#"{"message":"No such container: missing"}"#).unwrap(),
        "No such container: missing"
    );
    assert!(schema::error(br#"{"message":4}"#).is_err());
}
