//! Credential actions retain their wire values while shared client/model logs
//! hide them, including replies that cannot be parsed as actions.
use netget::{
    client::llm_budget::call_llm_for_client,
    llm::{
        actions::{
            client_trait::{Client, ClientActionResult},
            protocol_trait::Protocol,
            ActionDefinition, Parameter, StartupExamples,
        },
        rate_limiter::{RateLimiter, RateLimiterConfig},
        ConversationHandler, OllamaClient, RequestSource,
    },
    protocol::{ConnectContext, Event, EventType},
    scripting::{EventHandler, EventHandlerConfig, EventHandlerType, EventPattern},
    state::{app_state::WebSearchMode, AccessLogOwner, AppState, ClientId, ClientInstance},
};
use serde_json::{json, Value};
use std::{
    io::Write,
    sync::{Arc, Mutex},
    time::Duration,
};
use tokio::sync::mpsc;
use tracing::instrument::WithSubscriber;
const PASSWORD: &str = "fixture-password-do-not-log-73";
const ORDINARY: &str = "ordinary-visible-marker-86";
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);
impl Write for Capture {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
impl Capture {
    fn text(&self) -> String {
        String::from_utf8(self.0.lock().unwrap().clone()).unwrap()
    }
}
fn subscriber(capture: Capture) -> impl tracing::Subscriber + Send + Sync {
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::TRACE)
        .without_time()
        .with_ansi(false)
        .with_writer(move || capture.clone())
        .finish()
}
#[derive(Clone)]
struct TestProtocol {
    credential: bool,
}
fn action(credential: bool) -> ActionDefinition {
    let name = if credential {
        "credential_login"
    } else {
        "ordinary_action"
    };
    ActionDefinition {
        name: name.into(),
        description: "Test typed action".into(),
        parameters: vec![Parameter {
            name: if credential { "password" } else { "value" }.into(),
            type_hint: "string".into(),
            description: "value".into(),
            required: true,
        }],
        example: json!({"type":name}),
        log_template: None,
    }
}
impl Protocol for TestProtocol {
    fn protocol_name(&self) -> &'static str {
        "Test"
    }
    fn stack_name(&self) -> &'static str {
        "ETH>IP>TCP>Test"
    }
    fn keywords(&self) -> Vec<&'static str> {
        vec!["test"]
    }
    fn description(&self) -> &'static str {
        "test"
    }
    fn group_name(&self) -> &'static str {
        "Core"
    }
    fn example_prompt(&self) -> &'static str {
        "test"
    }
    fn get_async_actions(&self, _: &AppState) -> Vec<ActionDefinition> {
        vec![action(self.credential)]
    }
    fn get_sync_actions(&self) -> Vec<ActionDefinition> {
        vec![]
    }
    fn metadata(&self) -> netget::protocol::metadata::ProtocolMetadataV2 {
        netget::protocol::metadata::ProtocolMetadataV2::builder().build()
    }
    fn get_startup_examples(&self) -> StartupExamples {
        StartupExamples::new(json!({}), json!({}), json!({}))
    }
}
impl Client for TestProtocol {
    fn connect(
        &self,
        _: ConnectContext,
    ) -> std::pin::Pin<
        Box<dyn std::future::Future<Output = anyhow::Result<std::net::SocketAddr>> + Send>,
    > {
        Box::pin(async { anyhow::bail!("test does not connect") })
    }
    fn execute_action(&self, value: Value) -> anyhow::Result<ClientActionResult> {
        Ok(ClientActionResult::SendData(
            value[if self.credential { "password" } else { "value" }]
                .as_str()
                .unwrap()
                .as_bytes()
                .to_vec(),
        ))
    }
}
async fn instance(state: &AppState, handler: Option<EventHandlerType>) -> ClientId {
    let mut client = ClientInstance::new(
        ClientId::new(0),
        "127.0.0.1:1".into(),
        "test".into(),
        "test".into(),
    );
    if let Some(handler) = handler {
        let mut config = EventHandlerConfig::new();
        config.add_handler(EventHandler::new(EventPattern::wildcard(), handler));
        client.event_handler_config = Some(config);
    }
    state.add_client(client).await
}
fn event() -> Event {
    static EVENT: std::sync::LazyLock<EventType> =
        std::sync::LazyLock::new(|| EventType::new("test_connected", "ready", json!({})));
    Event::new(&EVENT, json!({"ready":true}))
}
async fn routed(
    state: &AppState,
    client: &OllamaClient,
    id: ClientId,
    credential: bool,
    tx: &mpsc::UnboundedSender<String>,
) -> anyhow::Result<Vec<Value>> {
    Ok(call_llm_for_client(
        client,
        state,
        id.to_string(),
        if credential { "credential" } else { "ordinary" },
        "",
        Some(&event()),
        &TestProtocol { credential },
        tx,
    )
    .await?
    .actions)
}
async fn mock(mode: &str, openai: bool) -> (u16, tokio::task::JoinHandle<()>) {
    use axum::{extract::State, response::IntoResponse, routing::post, Json, Router};
    async fn reply(
        State((mode, openai)): State<(String, bool)>,
        Json(request): Json<Value>,
    ) -> axum::response::Response {
        let credential = request.to_string().contains("credential_login");
        let narrowed = request.to_string().contains("narrowed-followup");
        let (name, field, value) = if narrowed {
            ("ordinary_action", "value", PASSWORD)
        } else if credential {
            ("credential_login", "password", PASSWORD)
        } else {
            ("ordinary_action", "value", ORDINARY)
        };
        let mut args = json!({});
        args[field] = json!(value);
        let mut returned = args.clone();
        returned["type"] = json!(name);
        if mode == "error" {
            return (
                axum::http::StatusCode::BAD_REQUEST,
                Json(if openai {
                    json!({"error":{"message":PASSWORD}})
                } else {
                    json!({"error":PASSWORD})
                }),
            )
                .into_response();
        }
        let content = match mode.as_str() {
            "malformed" => format!("{{\"actions\":[{{\"password\":\"{PASSWORD}\""),
            "plain" => format!("unstructured secret {PASSWORD}"),
            "malformed_common" => {
                json!({"actions":[{"type":"show_message","message":null,"password":PASSWORD}]})
                    .to_string()
            }
            "unknown" => json!({"actions":[{"type":PASSWORD,"password":PASSWORD}]}).to_string(),
            "diagnostics" if !request.to_string().contains("Tool execution results") => {
                json!({"actions":[returned, {"type":"read_file","path":PASSWORD},
                    {"type":"show_message","message":PASSWORD}]})
                .to_string()
            }
            "diagnostics" => {
                // The tool round contains draft actions. Explicitly offer the display
                // action again in the final response whose actions are committed.
                json!({"actions":[returned, {"type":"show_message","message":PASSWORD}]})
                    .to_string()
            }
            _ => json!({"actions":[returned]}).to_string(),
        };
        if openai {
            let message = if mode == "native" || mode == "followup_native" {
                json!({"role":"assistant","tool_calls":[{"id":"1","type":"function","function":{"name":name,"arguments":args.to_string()}}],"reasoning_content":value})
            } else {
                json!({"role":"assistant","content":content,"reasoning_content":value})
            };
            Json(json!({"choices":[{"message":message}],"usage":{"prompt_tokens":1,"completion_tokens":1}})).into_response()
        } else {
            let message = if mode == "native" || mode == "followup_native" {
                json!({"role":"assistant","tool_calls":[{"function":{"name":name,"arguments":args}}],"thinking":value})
            } else {
                json!({"role":"assistant","content":content,"thinking":value})
            };
            Json(json!({"model":"test-model","message":message,"response":content,"thinking":value,"done":true})).into_response()
        }
    }
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let app = Router::new()
        .route("/api/generate", post(reply))
        .route("/api/chat", post(reply))
        .route("/v1/chat/completions", post(reply))
        .with_state((mode.into(), openai));
    let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    (port, task)
}
fn drain(rx: &mut mpsc::UnboundedReceiver<String>) -> String {
    let mut lines = Vec::new();
    while let Ok(line) = rx.try_recv() {
        lines.push(line)
    }
    lines.join("\n")
}
fn assert_hidden(logs: &str, status: &str) {
    assert!(
        !logs.contains(PASSWORD),
        "file logs exposed fixture credential: {}",
        logs.lines()
            .filter(|line| line.contains(PASSWORD))
            .map(|line| line.replace(PASSWORD, "<fixture credential>"))
            .collect::<Vec<_>>()
            .join("\n")
    );
    assert!(
        !status.contains(PASSWORD),
        "status exposed fixture credential"
    );
}
#[tokio::test]
async fn static_and_script_actions_execute_original_password_but_store_redacted_copies() {
    static BACKGROUND: std::sync::LazyLock<Capture> = std::sync::LazyLock::new(|| {
        let capture = Capture::default();
        tracing::subscriber::set_global_default(subscriber(capture.clone())).unwrap();
        capture
    });
    let background = BACKGROUND.clone();

    let capture = Capture::default();
    let observed = capture.clone();
    async move {
        let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
        let client = OllamaClient::new("http://127.0.0.1:1");
        let (tx, _) = mpsc::unbounded_channel();
        let a = json!({"type":"credential_login","password":PASSWORD,"username":"alice"});
        for handler in [
            EventHandlerType::static_response(vec![a.clone()]),
            EventHandlerType::script(
                "python",
                format!(
                    "import json,sys\nsys.stderr.write({:?})\njson.dump({{\"actions\":[{}]}},sys.stdout)",
                    PASSWORD, a
                ),
            ),
            EventHandlerType::script_resident(
                "python",
                format!("import sys\ndef handle(event_type,event,message):\n sys.stderr.write({:?}+'\\n')\n return {{'actions':[{}]}}", PASSWORD, a),
                None,
            ),
        ] {
            let id = instance(&state, Some(handler)).await;
            let actions = routed(&state, &client, id, true, &tx).await.unwrap();
            assert_eq!(actions, vec![a.clone()]);
            assert_eq!(
                TestProtocol { credential: true }
                    .execute_action(actions[0].clone())
                    .unwrap()
                    .get_all_data(),
                vec![PASSWORD.as_bytes().to_vec()]
            );
            let logs = state
                .list_access_logs_for(Some(AccessLogOwner::Client(id.as_u32())), None)
                .await;
            assert_eq!(logs.len(), 1);
            assert_eq!(logs[0].response[0]["password"], "<redacted>");
            assert_eq!(logs[0].response[0]["username"], "alice");
            assert!(!serde_json::to_string(&logs).unwrap().contains(PASSWORD));
            netget::scripting::ResidentScriptManager::shutdown_client(id.as_u32()).await;
        }
        tokio::time::timeout(Duration::from_secs(5), async {
            while !background.text().contains("resident script client #") {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        }).await.expect("resident stderr must reach the background log oracle");
        assert!(!background.text().contains(PASSWORD), "resident stderr exposed credential");
        assert!(background.text().contains("<redacted>"));
        assert!(
            !observed.text().contains(PASSWORD),
            "script code/stdout exposed a credential at TRACE"
        );
    }
    .with_subscriber(subscriber(capture))
    .await;
}
#[tokio::test]
async fn concurrent_credential_and_ordinary_requests_keep_privacy_local_to_the_request() {
    let capture = Capture::default();
    let observed = capture.clone();
    async move {
        let (port, peer) = mock("valid", false).await;
        let url = format!("http://127.0.0.1:{port}");
        let (tx, mut rx) = mpsc::unbounded_channel();
        let client = OllamaClient::new(&url).with_status_tx(tx.clone());
        let state = AppState::new_with_options(false, url);
        state.set_ollama_model(Some("test-model".into())).await;
        let secret_id = instance(&state, None).await;
        let ordinary_id = instance(&state, None).await;
        let (secret, ordinary) = tokio::join!(
            routed(&state, &client, secret_id, true, &tx),
            routed(&state, &client, ordinary_id, false, &tx)
        );
        assert_eq!(secret.unwrap()[0]["password"], PASSWORD);
        assert_eq!(ordinary.unwrap()[0]["value"], ORDINARY);
        let logs = state
            .list_access_logs_for(Some(AccessLogOwner::Client(secret_id.as_u32())), None)
            .await;
        assert_eq!(logs[0].response[0]["password"], "<redacted>");
        assert_hidden(&observed.text(), &drain(&mut rx));
        assert!(
            observed.text().contains(ORDINARY),
            "control proves ordinary payload logging remained enabled"
        );
        peer.abort();
        let _ = peer.await;
    }
    .with_subscriber(subscriber(capture))
    .await;
}
#[tokio::test]
async fn native_tool_arguments_and_reasoning_are_private_on_ollama_and_openai() {
    for openai in [false, true] {
        let capture = Capture::default();
        let observed = capture.clone();
        async move {
            let (port, peer) = mock("native", openai).await;
            let (tx, mut rx) = mpsc::unbounded_channel();
            let url = format!("http://127.0.0.1:{port}");
            let client = if openai {
                OllamaClient::new_openai(url, "fixture-api-key")
            } else {
                OllamaClient::new(url)
            }
            .with_status_tx(tx.clone());
            let offered = vec![
                action(true),
                netget::llm::actions::common::show_message_action(),
            ];
            let mut conversation = ConversationHandler::new(
                "credential request".into(),
                Arc::new(client),
                "test-model".into(),
                RateLimiter::new(RateLimiterConfig::default()),
                RequestSource::Network,
            )
            .with_native_tools(&offered)
            .with_status_tx(tx);
            conversation.add_user_message("login".into());
            let actions = conversation
                .generate_with_tools_and_retry(None, WebSearchMode::Off, offered)
                .await
                .unwrap();
            assert_eq!(actions[0]["password"], PASSWORD);
            assert_hidden(&observed.text(), &drain(&mut rx));
            peer.abort();
            let _ = peer.await;
        }
        .with_subscriber(subscriber(capture))
        .await;
    }
}
#[tokio::test]
async fn malformed_plain_text_and_reflected_backend_errors_never_expose_credentials() {
    for native in [false, true] {
        for openai in [false, true] {
            for mode in ["malformed", "plain", "malformed_common", "unknown", "error"] {
                let capture = Capture::default();
                let observed = capture.clone();
                async move {
                    let (port, peer) = mock(mode, openai).await;
                    let (tx, mut rx) = mpsc::unbounded_channel();
                    let url = format!("http://127.0.0.1:{port}");
                    let client = if openai {
                        OllamaClient::new_openai(url, "fixture-api-key")
                    } else {
                        OllamaClient::new(url)
                    }
                    .with_status_tx(tx.clone());
                    let offered = vec![
                        action(true),
                        netget::llm::actions::common::show_message_action(),
                    ];
                    let mut conversation = ConversationHandler::new(
                        "credential request".into(),
                        Arc::new(client),
                        "test-model".into(),
                        RateLimiter::new(RateLimiterConfig::default()),
                        RequestSource::Network,
                    )
                    .with_status_tx(tx);
                    if native {
                        conversation = conversation.with_native_tools(&offered);
                    }
                    conversation.add_user_message("login".into());
                    let e = tokio::time::timeout(
                        Duration::from_secs(5),
                        conversation.generate_with_tools_and_retry(
                            None,
                            WebSearchMode::Off,
                            offered,
                        ),
                    )
                    .await
                    .unwrap()
                    .expect_err("peer reply must fail");
                    assert!(!format!("{e:#}").contains(PASSWORD));
                    assert_hidden(&observed.text(), &drain(&mut rx));
                    peer.abort();
                    let _ = peer.await;
                }
                .with_subscriber(subscriber(capture))
                .await;
            }
        }
    }
}

#[tokio::test]
async fn narrowed_followups_keep_credential_history_private_on_both_backends() {
    for native in [false, true] {
        for openai in [false, true] {
            let capture = Capture::default();
            let observed = capture.clone();
            async move {
                let (port, peer) = mock(
                    if native {
                        "followup_native"
                    } else {
                        "followup_prompt"
                    },
                    openai,
                )
                .await;
                let (tx, mut rx) = mpsc::unbounded_channel();
                let url = format!("http://127.0.0.1:{port}");
                let client = if openai {
                    OllamaClient::new_openai(url, "fixture-api-key")
                } else {
                    OllamaClient::new(url)
                }
                .with_status_tx(tx.clone());
                let credentials = vec![action(true)];
                let mut conversation = ConversationHandler::new(
                    "Offer credential_login for login, then ordinary_action for the follow-up"
                        .into(),
                    Arc::new(client),
                    "test-model".into(),
                    RateLimiter::new(RateLimiterConfig::default()),
                    RequestSource::Network,
                )
                .with_status_tx(tx);
                if native {
                    conversation = conversation.with_native_tools(&credentials);
                }
                conversation.add_user_message("login".into());
                let first = conversation
                    .generate_with_tools_and_retry(None, WebSearchMode::Off, credentials)
                    .await
                    .unwrap();
                assert_eq!(first[0]["password"], PASSWORD);

                let narrowed = vec![action(false)];
                if native {
                    conversation = conversation.with_native_tools(&narrowed);
                }
                conversation.add_user_message("narrowed-followup".into());
                let next = conversation
                    .generate_with_tools_and_retry(None, WebSearchMode::Off, narrowed)
                    .await
                    .unwrap();
                assert_eq!(next[0]["type"], "ordinary_action");
                assert_eq!(next[0]["value"], PASSWORD);
                assert_eq!(
                    TestProtocol { credential: false }
                        .execute_action(next[0].clone(),)
                        .unwrap()
                        .get_all_data(),
                    vec![PASSWORD.as_bytes().to_vec()]
                );
                assert_hidden(&observed.text(), &drain(&mut rx));
                peer.abort();
                let _ = peer.await;
            }
            .with_subscriber(subscriber(capture))
            .await;
        }
    }
}

#[tokio::test]
async fn tool_and_action_diagnostics_hide_credentials_without_changing_display_actions() {
    for openai in [false, true] {
        let capture = Capture::default();
        let observed = capture.clone();
        async move {
            let (port, peer) = mock("diagnostics", openai).await;
            let (tx, mut rx) = mpsc::unbounded_channel();
            let url = format!("http://127.0.0.1:{port}");
            let client = if openai {
                OllamaClient::new_openai(url, "fixture-api-key")
            } else {
                OllamaClient::new(url)
            }
            .with_status_tx(tx.clone());
            let offered = vec![
                action(true),
                netget::llm::actions::common::show_message_action(),
                netget::llm::actions::tools::read_file_action(),
            ];
            let mut conversation = ConversationHandler::new(
                "Offer credential_login, read_file and show_message".into(),
                Arc::new(client),
                "test-model".into(),
                RateLimiter::new(RateLimiterConfig::default()),
                RequestSource::Network,
            )
            .with_status_tx(tx);
            conversation.add_user_message("login".into());
            let actions = conversation
                .generate_with_tools_and_retry(None, WebSearchMode::Off, offered)
                .await
                .unwrap();
            assert_eq!(actions[0]["password"], PASSWORD);
            assert!(
                actions
                    .iter()
                    .any(|a| a["type"] == "show_message" && a["message"] == PASSWORD),
                "intentional display action must preserve its original value"
            );
            assert_hidden(&observed.text(), &drain(&mut rx));
            peer.abort();
            let _ = peer.await;
        }
        .with_subscriber(subscriber(capture))
        .await;
    }
}
