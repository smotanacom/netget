//! Redfish rules without peers: envelope checks, the unauthenticated documents, sessions and
//! Basic auth, Base-registry errors, tasks and their monitors, fail-closed answers, and the
//! NetGet client/service pair.
use crate::helpers::redfish::*;
use netget::server::redfish::model;
use netget::state::{client_handles::ClientSendOutcome, AccessLogOwner};
use serde_json::{json, Value};
use std::time::Duration;

#[test]
fn envelope_rules() {
    assert_eq!(
        model::normalize("/redfish/v1").as_deref(),
        Some("/redfish/v1/")
    );
    assert_eq!(
        model::normalize("/redfish/v1/Systems/1/").as_deref(),
        Some("/redfish/v1/Systems/1")
    );
    for bad in [
        "/redfish/v2/x",
        "/redfish/v1/../etc",
        "/redfish/v1//x",
        "/other",
        "/redfish/v1/a\\b",
    ] {
        assert!(model::normalize(bad).is_none(), "{bad}");
    }
    assert!(model::valid_type("#ComputerSystem.v1_22_0.ComputerSystem"));
    assert!(model::valid_type("#ChassisCollection.ChassisCollection"));
    for bad in [
        "ComputerSystem.v1_0_0.X",
        "#A.v1_0.B",
        "#A.v1_0_0.B.C",
        "#A-B.C",
    ] {
        assert!(!model::valid_type(bad), "{bad}");
    }
    let mut r = json!({"@odata.type": "#Sensor.v1_9_0.Sensor", "Id": "T", "Name": "Temp"});
    model::check_resource("/redfish/v1/Chassis/1/Sensors/T", &mut r).unwrap();
    assert_eq!(
        r["@odata.id"], "/redfish/v1/Chassis/1/Sensors/T",
        "filled in"
    );
    let mut c = json!({"@odata.type": "#SensorCollection.SensorCollection", "Name": "S", "Members": [{"@odata.id": "/redfish/v1/a"}, {"@odata.id": "/redfish/v1/b"}], "Members@odata.count": 9});
    model::check_resource("/redfish/v1/S", &mut c).unwrap();
    assert_eq!(c["Members@odata.count"], 2, "Rust counts the members");
    for (path, bad) in [
        (
            "/redfish/v1/x",
            json!({"@odata.id": "/redfish/v1/y", "@odata.type": "#A.v1_0_0.A", "Id": "x", "Name": "x"}),
        ),
        ("/redfish/v1/x", json!({"Id": "x", "Name": "x"})),
        (
            "/redfish/v1/x",
            json!({"@odata.type": "#A.v1_0_0.A", "Name": "x"}),
        ),
        (
            "/redfish/v1/x",
            json!({"@odata.type": "#AC.AC", "Name": "x", "Members": [{"@odata.id": "http://elsewhere"}]}),
        ),
        ("/redfish/v1/x", json!([1])),
    ] {
        let mut bad = bad;
        assert!(model::check_resource(path, &mut bad).is_err(), "{bad}");
    }
    assert_eq!(
        model::split_action("/redfish/v1/Systems/1/Actions/ComputerSystem.Reset"),
        Some((
            "/redfish/v1/Systems/1".into(),
            "ComputerSystem.Reset".into()
        ))
    );
    let e = model::error_body("ResourceMissingAtURI", "gone");
    assert_eq!(
        e["error"]["@Message.ExtendedInfo"][0]["MessageId"],
        format!("{}.ResourceMissingAtURI", model::BASE_REGISTRY)
    );
}

struct Http {
    base: String,
    client: reqwest::Client,
}

impl Http {
    async fn go(
        &self,
        method: &str,
        path: &str,
        headers: &[(&str, &str)],
        body: Option<Value>,
    ) -> (u16, reqwest::header::HeaderMap, Value) {
        let mut r = self
            .client
            .request(method.parse().unwrap(), format!("{}{path}", self.base));
        for (k, v) in headers {
            r = r.header(*k, *v);
        }
        if let Some(b) = body {
            r = r
                .header("Content-Type", "application/json")
                .body(b.to_string());
        }
        let resp = r.send().await.unwrap();
        let status = resp.status().as_u16();
        let h = resp.headers().clone();
        let text = resp.text().await.unwrap();
        (
            status,
            h,
            serde_json::from_str(&text).unwrap_or(Value::String(text)),
        )
    }
}

fn message_id(v: &Value) -> String {
    v["error"]["@Message.ExtendedInfo"][0]["MessageId"]
        .as_str()
        .unwrap_or("")
        .rsplit('.')
        .next()
        .unwrap_or("")
        .to_owned()
}

#[tokio::test(flavor = "multi_thread")]
async fn sessions_errors_tasks_and_fail_closed_over_http() {
    let state = state();
    let dir = tempfile::tempdir().unwrap();
    let (sid, addr) = server_in(
        &state,
        bmc_policy(&dir.path().join("bmc.json")),
        json!({"session_timeout_secs": 60}),
    )
    .await;
    let h = Http {
        base: format!("http://{addr}"),
        client: reqwest::Client::new(),
    };
    let (s, _, v) = h.go("GET", "/redfish", &[], None).await;
    assert_eq!((s, v), (200, json!({"v1": "/redfish/v1/"})));
    let (s, hd, root) = h.go("GET", "/redfish/v1", &[], None).await;
    assert_eq!((s, hd["odata-version"].to_str().unwrap()), (200, "4.0"));
    assert_eq!(
        root["Links"]["Sessions"]["@odata.id"],
        "/redfish/v1/SessionService/Sessions"
    );
    let (s, hd, xml) = h.go("GET", "/redfish/v1/$metadata", &[], None).await;
    assert_eq!(
        (s, hd["content-type"].to_str().unwrap()),
        (200, "application/xml")
    );
    assert!(xml
        .as_str()
        .unwrap()
        .contains("ServiceRoot.v1_16_1.ServiceContainer"));
    let (s, _, odata) = h.go("GET", "/redfish/v1/odata", &[], None).await;
    assert!(s == 200 && odata["value"].as_array().unwrap().len() >= 6);

    let (s, hd, v) = h.go("GET", "/redfish/v1/Systems", &[], None).await;
    assert_eq!((s, message_id(&v).as_str()), (401, "NoValidSession"));
    assert!(hd["www-authenticate"]
        .to_str()
        .unwrap()
        .starts_with("Basic"));
    let (s, _, _) = h
        .go(
            "GET",
            "/redfish/v1/Systems",
            &[("Authorization", "Basic YWRtaW46bm9wZQ==")],
            None,
        )
        .await;
    assert_eq!(s, 401, "admin:nope");
    let (s, hd, session) = h
        .go(
            "POST",
            "/redfish/v1/SessionService/Sessions",
            &[],
            Some(json!({"UserName": "admin", "Password": "secret"})),
        )
        .await;
    assert_eq!(s, 201);
    let token = hd["x-auth-token"].to_str().unwrap().to_owned();
    let location = hd["location"].to_str().unwrap().to_owned();
    assert_eq!(session["@odata.id"], location);
    let auth = [("X-Auth-Token", token.as_str())];
    let (s, _, systems) = h.go("GET", "/redfish/v1/Systems", &auth, None).await;
    assert_eq!((s, systems["Members@odata.count"].as_u64()), (200, Some(1)));
    let (_, _, sessions) = h
        .go("GET", "/redfish/v1/SessionService/Sessions", &auth, None)
        .await;
    assert_eq!(sessions["Members"], json!([{"@odata.id": location}]));
    let (s, _, v) = h.go("GET", "/redfish/v1/Nope", &auth, None).await;
    assert_eq!((s, message_id(&v).as_str()), (404, "ResourceMissingAtURI"));
    let (s, _, v) = h.go("GET", "/redfish/v1/Broken", &auth, None).await;
    assert_eq!(
        (s, message_id(&v).as_str()),
        (500, "GeneralError"),
        "a resource without @odata.type is never sent"
    );
    let (s, _, _) = h
        .go(
            "PATCH",
            "/redfish/v1/Systems/1",
            &[auth[0], ("Content-Type", "text/plain")],
            None,
        )
        .await;
    assert_eq!(s, 415);
    let (s, _, v) = h
        .go(
            "PATCH",
            "/redfish/v1/Systems/1",
            &auth,
            Some(json!({"PowerState": "Off"})),
        )
        .await;
    assert_eq!((s, message_id(&v).as_str()), (400, "PropertyNotWritable"));
    let reset = "/redfish/v1/Systems/1/Actions/ComputerSystem.Reset";
    let (s, _, v) = h.go("POST", reset, &auth, Some(json!({}))).await;
    assert_eq!(
        (s, message_id(&v).as_str()),
        (400, "ActionParameterMissing")
    );
    let (s, _, v) = h
        .go("POST", reset, &auth, Some(json!({"ResetType": "Explode"})))
        .await;
    assert_eq!(
        (s, message_id(&v).as_str()),
        (400, "ActionParameterValueNotInList")
    );
    let (s, hd, task) = h
        .go("POST", reset, &auth, Some(json!({"ResetType": "On"})))
        .await;
    assert_eq!((s, task["TaskState"].as_str()), (202, Some("Running")));
    let monitor = hd["location"].to_str().unwrap().to_owned();
    assert_eq!(h.go("GET", &monitor, &auth, None).await.0, 202);
    tokio::time::sleep(Duration::from_millis(1200)).await;
    assert_eq!(
        h.go("GET", &monitor, &auth, None).await.0,
        204,
        "the finished monitor returns the operation's answer"
    );
    let (_, _, done) = h
        .go("GET", task["@odata.id"].as_str().unwrap(), &auth, None)
        .await;
    assert_eq!(
        (
            done["TaskState"].as_str(),
            done["Messages"][0]["Message"].as_str()
        ),
        (Some("Completed"), Some("Reset On completed"))
    );
    let (s, hd, acct) = h
        .go(
            "POST",
            "/redfish/v1/AccountService/Accounts",
            &auth,
            Some(json!({"UserName": "ops", "Password": "p", "RoleId": "Operator"})),
        )
        .await;
    assert_eq!(
        (s, hd["location"].to_str().unwrap(), acct["RoleId"].as_str()),
        (
            201,
            "/redfish/v1/AccountService/Accounts/3",
            Some("Operator")
        )
    );
    assert_eq!(
        h.go(
            "DELETE",
            "/redfish/v1/AccountService/Accounts/3",
            &auth,
            None
        )
        .await
        .0,
        204
    );
    assert_eq!(
        h.go("PUT", "/redfish/v1/Systems/1", &auth, Some(json!({})))
            .await
            .0,
        405
    );
    assert_eq!(
        h.go(
            "GET",
            "/redfish/v1/Systems",
            &[auth[0], ("OData-Version", "3.0")],
            None
        )
        .await
        .0,
        412
    );
    assert_eq!(h.go("DELETE", &location, &auth, None).await.0, 204);
    assert_eq!(
        h.go("GET", "/redfish/v1/Systems", &auth, None).await.0,
        401,
        "a deleted session is gone"
    );
    let (s, _, sys) = h
        .go(
            "GET",
            "/redfish/v1/Systems/1",
            &[("Authorization", "Basic YWRtaW46c2VjcmV0")],
            None,
        )
        .await;
    assert_eq!((s, sys["Id"].as_str()), (200, Some("1")));
    let logins = logs(
        &state,
        AccessLogOwner::Server(sid.as_u32()),
        "redfish_login",
        3,
    )
    .await;
    assert_eq!(
        logins.len(),
        3,
        "one denied Basic, one session, one Basic accepted"
    );
    let (_, _, _) = h
        .go(
            "GET",
            "/redfish/v1/Chassis",
            &[("Authorization", "Basic YWRtaW46c2VjcmV0")],
            None,
        )
        .await;
    assert_eq!(
        logs(
            &state,
            AccessLogOwner::Server(sid.as_u32()),
            "redfish_login",
            3
        )
        .await
        .len(),
        3,
        "accepted Basic credentials are remembered"
    );
    state.remove_server(sid).await;

    // No handler and no model: logins are refused, and with auth off every resource is a 500.
    let state = crate::helpers::redfish::state();
    let (sid, addr) = server_in(&state, vec![], json!({})).await;
    let h = Http {
        base: format!("http://{addr}"),
        client: reqwest::Client::new(),
    };
    let (s, _, _) = h
        .go(
            "POST",
            "/redfish/v1/SessionService/Sessions",
            &[],
            Some(json!({"UserName": "admin", "Password": "secret"})),
        )
        .await;
    assert_eq!(s, 401, "a backend failure never logs anyone in");
    state.remove_server(sid).await;
    let (sid, addr) = server_in(&state, vec![], json!({"auth": "none"})).await;
    let h = Http {
        base: format!("http://{addr}"),
        client: reqwest::Client::new(),
    };
    let (s, _, v) = h.go("GET", "/redfish/v1/Systems", &[], None).await;
    assert_eq!((s, message_id(&v).as_str()), (500, "GeneralError"));
    state.remove_server(sid).await;
}

#[tokio::test(flavor = "multi_thread")]
async fn netget_client_and_service_agree() {
    let state = state();
    let dir = tempfile::tempdir().unwrap();
    let (sid, addr) = server_in(&state, bmc_policy(&dir.path().join("bmc.json")), json!({})).await;
    let cid = client_in(
        &state,
        addr.to_string(),
        json!({"scheme": "http", "username": "admin", "password": "secret"}),
    )
    .await
    .unwrap();
    let owner = AccessLogOwner::Client(cid.as_u32());
    let connected = logs(&state, owner, "redfish_connected", 1).await;
    assert_eq!(connected[0].request["authenticated"], "session");
    assert_eq!(
        connected[0].request["links"]["Systems"],
        "/redfish/v1/Systems"
    );
    for a in [
        json!({"type":"redfish_get","path":"/redfish/v1/Systems/1"}),
        json!({"type":"redfish_patch","path":"/redfish/v1/Systems/1","body":{"AssetTag":"pair-1"}}),
        json!({"type":"redfish_action","resource_path":"/redfish/v1/Systems/1","action":"ComputerSystem.Reset","parameters":{"ResetType":"ForceOff"}}),
        json!({"type":"redfish_post","path":"/redfish/v1/AccountService/Accounts","body":{"UserName":"ops","RoleId":"Operator"}}),
        json!({"type":"redfish_delete","path":"/redfish/v1/AccountService/Accounts/3"}),
        json!({"type":"redfish_get","path":"/redfish/v1/Missing"}),
        json!({"type":"redfish_get","path":"/redfish/v1/Systems/1"}),
    ] {
        assert!(matches!(
            state
                .send_to_client(cid, a, Duration::from_secs(20))
                .await
                .unwrap(),
            ClientSendOutcome::Sent { .. }
        ));
    }
    let rows = logs(&state, owner, "redfish_response", 7).await;
    let r: Vec<&Value> = rows.iter().map(|r| &r.request).collect();
    assert_eq!(
        (r[0]["status"].as_u64(), r[0]["body"]["Model"].as_str()),
        (Some(200), Some("NG-1"))
    );
    assert_eq!(r[1]["status"], 204);
    assert_eq!(
        (r[2]["path"].as_str(), r[2]["status"].as_u64()),
        (
            Some("/redfish/v1/Systems/1/Actions/ComputerSystem.Reset"),
            Some(204)
        )
    );
    assert_eq!(r[2]["task"]["state"], "Completed");
    assert_eq!(
        (r[3]["status"].as_u64(), r[3]["location"].as_str()),
        (Some(201), Some("/redfish/v1/AccountService/Accounts/3"))
    );
    assert_eq!(r[4]["status"], 204);
    assert_eq!(
        (
            r[5]["status"].as_u64(),
            r[5]["error"]["message_id"].as_str()
        ),
        (Some(404), Some("Base.1.19.0.ResourceMissingAtURI"))
    );
    assert_eq!(r[6]["body"]["AssetTag"], "pair-1");
    for bad in [
        json!({"type":"redfish_get","path":"/etc/passwd"}),
        json!({"type":"redfish_action","resource_path":"/redfish/v1/Systems/1","action":"Reset"}),
        json!({"type":"redfish_patch","path":"/redfish/v1/Systems/1","body":"x"}),
    ] {
        assert!(matches!(
            state
                .send_to_client(cid, bad, Duration::from_secs(5))
                .await
                .unwrap(),
            ClientSendOutcome::Rejected { .. }
        ));
    }
    // disconnect logs the session out before the client stops.
    assert!(matches!(
        state
            .send_to_client(cid, json!({"type":"disconnect"}), Duration::from_secs(5))
            .await
            .unwrap(),
        ClientSendOutcome::Disconnected
    ));
    let h = Http {
        base: format!("http://{addr}"),
        client: reqwest::Client::new(),
    };
    let gone = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let (_, _, s) = h
                .go(
                    "GET",
                    "/redfish/v1/SessionService/Sessions",
                    &[("Authorization", "Basic YWRtaW46c2VjcmV0")],
                    None,
                )
                .await;
            if s["Members@odata.count"] == 0 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await;
    assert!(gone.is_ok(), "the session was not deleted on disconnect");
    let refused = client_in(
        &state,
        addr.to_string(),
        json!({"scheme": "http", "username": "admin", "password": "bad"}),
    )
    .await;
    assert!(refused.unwrap_err().to_string().contains("401"));
    state.remove_server(sid).await;
}
