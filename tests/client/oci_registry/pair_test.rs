use super::common::*;
use netget::{
    cli::management::ServerForm,
    llm::actions::protocol_trait::Protocol,
    state::{AppState, ServerId},
};
use serde_json::{json, Value};
use std::time::Duration;

async fn server(state: &AppState, handlers: Vec<Value>) -> (ServerId, String) {
    server_with_params(state, handlers, None).await
}
async fn server_with_params(
    state: &AppState,
    handlers: Vec<Value>,
    startup_params: Option<Value>,
) -> (ServerId, String) {
    state
        .set_llm_client(netget::llm::OllamaClient::new("http://127.0.0.1:1"))
        .await;
    let (tx, _) = tokio::sync::mpsc::unbounded_channel();
    let id = ServerForm {
        protocol: "oci-registry".into(),
        port: Some(0),
        instruction: Some(String::new()),
        event_handlers: Some(handlers),
        startup_params,
        ..Default::default()
    }
    .create(state, tx)
    .await
    .unwrap();
    let addr = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(addr) = state.get_server(id).await.and_then(|s| s.local_addr) {
                break addr;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    (id, format!("http://{addr}"))
}
fn tags() -> Value {
    static_handler(
        "oci_tags_request",
        json!([{"type":"send_oci_tags","tags":["latest"]}]),
    )
}
async fn result(state: &AppState, id: netget::state::ClientId, action: Value) -> Value {
    let before = latest(state, id).await;
    send(state, id, action).await;
    event(state, id, "oci_result", before).await.1
}
#[tokio::test]
async fn advertised_static_and_script_examples_run_against_existing_server() {
    let state = state();
    let (sid, origin) = server(&state, vec![tags()]).await;
    let examples =
        netget::client::oci_registry::OciRegistryClientProtocol::new().get_startup_examples();
    for example in [&examples.static_mode, &examples.script_mode] {
        let id = client(
            &state,
            origin.clone(),
            json!({}),
            example["event_handlers"].as_array().unwrap().clone(),
        )
        .await;
        assert_eq!(
            event(&state, id, "oci_result", 0).await.1["data"]["tags"],
            json!(["latest"])
        );
        state.remove_client(id).await;
    }
    state.remove_server(sid).await;
}
#[tokio::test]
async fn pair_manifest_blob_and_head_preserve_native_digest_semantics() {
    let config = netget::server::oci_registry::actions::sha256_digest(b"{}");
    let body = "pair layer";
    let blob = netget::server::oci_registry::actions::sha256_digest(body.as_bytes());
    let manifest = json!({"schemaVersion":2,"mediaType":"application/vnd.oci.image.manifest.v1+json","config":{"mediaType":"application/vnd.oci.image.config.v1+json","digest":config,"size":2},"layers":[{"mediaType":"application/vnd.oci.image.layer.v1.tar","digest":blob,"size":body.len()}]});
    let state = state();
    let (sid, origin) = server(
        &state,
        vec![
            tags(),
            static_handler(
                "oci_manifest_request",
                json!([{"type":"send_oci_manifest","manifest":manifest}]),
            ),
            static_handler(
                "oci_blob_request",
                json!([{"type":"send_oci_blob","content":body,"encoding":"utf8"}]),
            ),
        ],
    )
    .await;
    let id = connected_client(&state, origin).await;
    let m=result(&state,id,json!({"type":"oci_request","operation":"manifest","repository":"library/demo","reference":"latest"})).await;
    assert_eq!(m["data"]["manifest"], manifest);
    assert_eq!(m["data"]["digest_verified"], true);
    let m2=result(&state,id,json!({"type":"oci_request","operation":"manifest","repository":"library/demo","reference":m["data"]["digest"]})).await;
    assert_eq!(m2["data"]["digest"], m["data"]["digest"]);
    let b=result(&state,id,json!({"type":"oci_request","operation":"blob","repository":"library/demo","reference":blob,"expected_size":body.len()})).await;
    assert_eq!(b["data"]["text"], body);
    assert_eq!(b["data"]["digest_verified"], true);
    let h=result(&state,id,json!({"type":"oci_request","operation":"blob_head","repository":"library/demo","reference":blob})).await;
    assert_eq!(h["data"]["exists"], true);
    assert_eq!(h["data"]["digest_verified"], false);
    state.remove_client(id).await;
    state.remove_server(sid).await;
}
#[tokio::test]
async fn mocked_model_native_requests_use_common_memory() -> crate::helpers::E2EResult<()> {
    use crate::helpers::{mock_builder::MockLlmBuilder, mock_ollama::MockOllamaServer};
    let peer = super::real_server_test::daemon().await?;
    let origin = format!("http://{}", peer.addr());
    super::real_server_test::seed(&origin).await;
    let mock=MockOllamaServer::start(MockLlmBuilder::new().on_event("oci_connected").respond_with_actions(json!([{"type":"set_memory","value":"typed registry session"},{"type":"oci_request","operation":"tags","repository":"library/demo"}])).expect_calls(1).and().on_event("oci_result").and_prompt_containing("typed registry session").respond_with_actions(json!([])).expect_calls(1).and().build()).await?;
    let state = AppState::new_with_options(false, mock.base_url());
    state.set_ollama_model(Some("mock".into())).await;
    let (tx, _) = tokio::sync::mpsc::unbounded_channel();
    let id = netget::cli::management::ClientForm {
        protocol: "oci-registry".into(),
        remote_addr: Some(origin),
        instruction: Some("Read one typed native page".into()),
        ..Default::default()
    }
    .create(&state, netget::llm::OllamaClient::new(mock.base_url()), tx)
    .await?;
    assert_eq!(
        event(&state, id, "oci_result", 0).await.1["data"]["tags"],
        json!(["latest", "second"])
    );
    tokio::time::timeout(Duration::from_secs(5), async {
        while mock.call_count().await < 2 {
            tokio::time::sleep(Duration::from_millis(10)).await
        }
    })
    .await
    .unwrap();
    mock.verify_calls().await?;
    assert_eq!(
        state.get_memory_for_client(id).await.as_deref(),
        Some("typed registry session")
    );
    state.remove_client(id).await;
    Ok(())
}
#[tokio::test]
async fn independent_crane_sdk_exchanges_token_and_retries_netget_tags(
) -> crate::helpers::E2EResult<()> {
    use super::auth_test::{PASSWORD, TOKEN};
    use base64::Engine;
    let token_peer = super::auth_test::pair_peer().await?;
    let realm = format!("http://{}/token", token_peer.addr());
    // The unchanged SDK owns retry and credential exchange. NetGet's server
    // remains model-as-registry and asks its handler to approve every tag read.
    let code=format!("import json,sys\ne=json.load(sys.stdin)['event']\na={{'type':'send_oci_tags','tags':['latest']}} if e.get('authorization')=='Bearer {TOKEN}' else {{'type':'send_oci_auth_challenge','realm':'{realm}','service':'registry.test','scope':'repository:library/demo:pull'}}\njson.dump({{'actions':[a]}},sys.stdout)");
    let state = state();
    let(sid,origin)=server_with_params(&state,vec![json!({"event_pattern":"oci_version_check","handler":{"type":"static","actions":[{"type":"send_oci_auth_challenge","realm":realm,"service":"registry.test","scope":"repository:library/demo:pull"}]}}),json!({"event_pattern":"oci_tags_request","handler":{"type":"script","language":"python","code":code}})],Some(json!({"version_check":"llm"}))).await;
    std::fs::write(
        token_peer.dir().join("backend.txt"),
        origin.strip_prefix("http://").unwrap(),
    )?;
    let address = token_peer.addr().to_string();
    let home = tempfile::TempDir::new()?;
    let auth = base64::engine::general_purpose::STANDARD.encode(format!("reader:{PASSWORD}"));
    let config = json!({"auths":{&address:{"auth":auth}}});
    let config_path = home.path().join("config.json");
    std::fs::write(&config_path, serde_json::to_vec(&config)?)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&config_path, std::fs::Permissions::from_mode(0o600))?;
    }
    let crane = crate::helpers::real_server::find_binary("crane")
        .expect("required independent crane0.22.1 SDK");
    let output = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::process::Command::new(crane)
            .args(["ls", &format!("{address}/library/demo")])
            .env("HOME", home.path())
            .env("DOCKER_CONFIG", home.path())
            .kill_on_drop(true)
            .output(),
    )
    .await??;
    let mut diagnostic = Value::String(
        String::from_utf8_lossy(&output.stderr)
            .chars()
            .take(2048)
            .collect(),
    );
    netget::client::oci_registry::api::redact(
        &mut diagnostic,
        &[PASSWORD.into(), TOKEN.into(), auth],
    );
    let seen = super::auth_test::observations(&token_peer).await;
    let access = state
        .list_access_logs_for(
            Some(netget::state::AccessLogOwner::Server(sid.as_u32())),
            None,
        )
        .await;
    let observed: Vec<_> = access
        .iter()
        .map(|e| {
            (
                &e.event_type,
                e.request
                    .get("authorization")
                    .and_then(Value::as_str)
                    .map(|s| s == format!("Bearer {TOKEN}")),
            )
        })
        .collect();
    assert!(
        output.status.success(),
        "independent SDK token exchange failed: {diagnostic}, token peer {seen}, native authorization checks {observed:?}"
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "latest");
    assert_eq!(seen["basic_verified"], true);
    assert_eq!(seen["last_scope"], json!(["repository:library/demo:pull"]));
    assert!(seen["token_requests"].as_u64().unwrap() >= 1);
    state.remove_server(sid).await;
    Ok(())
}
