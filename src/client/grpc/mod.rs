//! gRPC client implementation
pub mod actions;

pub use actions::GrpcClientProtocol;

use anyhow::{Context, Result};
use bytes::Bytes;
use http::Request;
use http_body_util::BodyExt;
use prost::Message as ProstMessage;
use prost_reflect::{
    DescriptorPool, DynamicMessage, MapKey, MessageDescriptor, ReflectMessage, Value as ProtoValue,
};
use prost_types::FileDescriptorSet;
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tonic::transport::{Channel, Endpoint};
use tower::{Service, ServiceExt};
use tracing::{debug, error, info};

use crate::client::grpc::actions::{
    GRPC_CLIENT_CONNECTED_EVENT, GRPC_CLIENT_ERROR_EVENT, GRPC_CLIENT_RESPONSE_RECEIVED_EVENT,
};
use crate::client::llm_budget::call_llm_for_client;
use crate::llm::actions::client_trait::{Client as ClientTrait, ClientActionResult};
use crate::llm::ollama_client::OllamaClient;
use crate::llm::ClientLlmResult;
use crate::protocol::{Event, StartupParams};
use crate::state::app_state::AppState;
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::{AccessLogOwner, ClientId, ClientStatus};

/// gRPC client connection state
#[derive(Debug, Clone)]
enum ConnectionState {
    Idle,
    Processing,
}

/// Shared client data
struct GrpcClientData {
    channel: Channel,
    descriptor_pool: Arc<DescriptorPool>,
    state: ConnectionState,
}

/// gRPC client that connects to remote gRPC servers
pub struct GrpcClient;

impl GrpcClient {
    /// Connect to a gRPC server with integrated LLM actions
    pub async fn connect_with_llm_actions(
        remote_addr: String,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        client_id: ClientId,
        startup_params: Option<StartupParams>,
    ) -> Result<SocketAddr> {
        info!("gRPC client {} connecting to {}", client_id, remote_addr);

        // Parse startup parameters
        let proto_schema = startup_params
            .as_ref()
            .map(|p| p.get_string("proto_schema"))
            .transpose()?
            .context("Missing required startup parameter: proto_schema")?;

        let use_tls = startup_params
            .as_ref()
            .map(|p| p.get_optional_bool("use_tls"))
            .transpose()?
            .flatten()
            .unwrap_or(false);

        // Load protobuf schema
        let descriptor_pool = load_schema(&proto_schema)
            .await
            .context("Failed to load protobuf schema")?;

        // List available services
        let services: Vec<String> = descriptor_pool
            .services()
            .map(|s| s.full_name().to_string())
            .collect();

        info!(
            "gRPC client {} loaded schema with services: {:?}",
            client_id, services
        );

        // Build gRPC channel
        let uri = if use_tls {
            format!("https://{}", remote_addr)
        } else {
            format!("http://{}", remote_addr)
        };

        let channel = Endpoint::from_shared(uri.clone())
            .context("Invalid gRPC endpoint")?
            .connect()
            .await
            .context("Failed to connect to gRPC server")?;

        info!("gRPC client {} connected to {}", client_id, remote_addr);

        let grpc_client_data = Arc::new(Mutex::new(GrpcClientData {
            channel,
            descriptor_pool: Arc::new(descriptor_pool),
            state: ConnectionState::Idle,
        }));

        // Store client in protocol_data
        app_state
            .with_client_mut(client_id, |client| {
                client.set_protocol_field(
                    "grpc_client".to_string(),
                    serde_json::json!("initialized"),
                );
                client
                    .set_protocol_field("server_addr".to_string(), serde_json::json!(remote_addr));
            })
            .await;

        // Update status
        app_state
            .update_client_status(client_id, ClientStatus::Connected)
            .await;
        let _ = status_tx.send(format!(
            "[CLIENT] gRPC client {} ready for {} (services: {})",
            client_id,
            remote_addr,
            services.join(", ")
        ));
        let _ = status_tx.send("__UPDATE_UI__".to_string());

        // Command channel for injected actions (the dashboard's [ send ] / composer).
        // Registered BEFORE the connected-event LLM call below, which a manual `*` routing
        // rule can park for minutes - the operator must be able to make an RPC while it
        // waits.
        //
        // This task also replaces the old "poll get_client() every 5s" idle task:
        // `remove_client` drops the command sender, so `recv()` returns `None` the moment
        // the client goes away.
        let command_rx =
            crate::client::command_support::register_command_channel(&app_state, client_id).await;
        let cmd_task = tokio::spawn(grpc_command_loop(
            command_rx,
            client_id,
            grpc_client_data.clone(),
            app_state.clone(),
            llm_client.clone(),
            status_tx.clone(),
        ));
        app_state.register_client_task(client_id, cmd_task).await;

        // Call LLM with connected event
        let protocol = Arc::new(GrpcClientProtocol::new());
        if let Some(instruction) = app_state.get_instruction_for_client(client_id).await {
            let event = Event::new(
                &GRPC_CLIENT_CONNECTED_EVENT,
                serde_json::json!({
                    "server_addr": remote_addr,
                    "services": services,
                }),
            );

            let memory = app_state
                .get_memory_for_client(client_id)
                .await
                .unwrap_or_default();

            match call_llm_for_client(
                &llm_client,
                &app_state,
                client_id.to_string(),
                &instruction,
                &memory,
                Some(&event),
                protocol.as_ref(),
                &status_tx,
            )
            .await
            {
                Ok(ClientLlmResult {
                    actions,
                    memory_updates,
                }) => {
                    // Update memory
                    if let Some(mem) = memory_updates {
                        app_state.set_memory_for_client(client_id, mem).await;
                    }

                    // Execute actions through the same path injected commands use, so
                    // the `grpc_call` decoding exists exactly once.
                    for action in actions {
                        let grpc_data = grpc_client_data.clone();
                        let proto = protocol.clone();
                        match Box::pin(execute_grpc_action(
                            client_id,
                            action,
                            grpc_data,
                            &app_state,
                            &llm_client,
                            &status_tx,
                            &proto,
                            Dispatch::Inline,
                        ))
                        .await
                        {
                            Ok(Applied::Disconnect) => break,
                            Ok(_) => {}
                            Err(e) => error!("Failed to execute gRPC action: {}", e),
                        }
                    }
                }
                Err(e) => {
                    error!("LLM error for gRPC client {}: {}", client_id, e);
                }
            }
        }

        // No idle-poll task: the command loop above is this client's only long-lived task
        // and it ends when the client is removed.

        // Return a dummy local address (gRPC manages connections internally)
        Ok("0.0.0.0:0".parse().unwrap())
    }
}

/// Load protobuf schema from various formats
async fn load_schema(schema_input: &str) -> Result<DescriptorPool> {
    use base64::{engine::general_purpose, Engine as _};

    // Try to decode as base64 FileDescriptorSet
    if let Ok(bytes) = general_purpose::STANDARD.decode(schema_input) {
        if let Ok(fds) = FileDescriptorSet::decode(&bytes[..]) {
            return DescriptorPool::from_file_descriptor_set(fds)
                .context("Failed to create descriptor pool from FileDescriptorSet");
        }
    }

    // Try as file path
    if std::path::Path::new(schema_input).exists() {
        let proto_content = tokio::fs::read_to_string(schema_input)
            .await
            .context("Failed to read .proto file")?;
        return compile_proto_text(&proto_content).await;
    }

    // Try as inline proto text
    if schema_input.contains("syntax") && schema_input.contains("proto") {
        return compile_proto_text(schema_input).await;
    }

    Err(anyhow::anyhow!(
        "Invalid proto_schema format. Expected base64 FileDescriptorSet, .proto file path, or inline .proto text"
    ))
}

/// Compile .proto text to descriptor pool using protoc
async fn compile_proto_text(proto_text: &str) -> Result<DescriptorPool> {
    // Write proto to temp file
    let temp_dir = tempfile::tempdir()?;
    let proto_path = temp_dir.path().join("schema.proto");
    tokio::fs::write(&proto_path, proto_text).await?;

    // Run protoc to compile.
    //
    // `--proto_path` is not optional here even though the file is named absolutely and
    // the child's cwd is the same directory: protoc requires every input to sit under
    // some `-I` root and compares the strings literally, so an absolute filename against
    // an implicit `-I.` fails with "File does not reside within any path specified using
    // --proto_path". Without it, the documented "inline .proto text" schema form could
    // never load. The server side (`src/server/grpc/mod.rs`) always passed one.
    let output = tokio::process::Command::new("protoc")
        .arg("--descriptor_set_out=/dev/stdout")
        .arg("--include_imports")
        .arg(format!("--proto_path={}", temp_dir.path().display()))
        .arg("schema.proto")
        .current_dir(temp_dir.path())
        .output()
        .await
        .context("Failed to run protoc (is it installed?)")?;

    if !output.status.success() {
        return Err(anyhow::anyhow!(
            "protoc failed: {}",
            String::from_utf8_lossy(&output.stderr)
        ));
    }

    let fds =
        FileDescriptorSet::decode(&output.stdout[..]).context("Failed to decode protoc output")?;

    DescriptorPool::from_file_descriptor_set(fds).context("Failed to create descriptor pool")
}

/// What one executed action did. Shared vocabulary between the connected-event handler
/// and the injected-command loop.
enum Applied {
    /// A gRPC request really went out; `bytes_sent` is the length of the framed
    /// message (5-byte gRPC header + protobuf payload) that was written to the
    /// channel and answered.
    ///
    /// `pending_notify` is the `grpc_response_received` payload when the caller asked
    /// for [`Dispatch::Defer`] - it has not been given to the LLM yet, and the caller
    /// must run [`notify_grpc_response`] with it once it has replied.
    Sent {
        bytes_sent: usize,
        pending_notify: Option<serde_json::Value>,
    },
    /// The action ran but put nothing on the wire; `detail` says why.
    Ran(String),
    /// The action asked to end the session.
    Disconnect,
}

/// How the `grpc_response_received` event that follows a call is delivered.
#[derive(Clone, Copy)]
enum Dispatch {
    /// Raise it - and run whatever the LLM answers - before returning. Used by the
    /// connected-event handler, which is where that recursion has always lived.
    Inline,
    /// Do the call and raise nothing at all -- neither the success event nor the error
    /// event. Used for a retry the model asks for in answer to `grpc_client_error`: that
    /// retry must not itself raise another error event, or a persistently failing call
    /// would drive the model round the same loop forever.
    Silent,
    /// Hand the event payload back to the caller instead. The injected-command loop
    /// uses this so it can reply with the truthful byte count **first** and only then
    /// raise the event: a client whose events are routed to a manual handler would
    /// otherwise hold `[ send ]`'s answer hostage for the length of a human's think
    /// time, and the operator would see a timeout for a call that in fact succeeded.
    Defer,
}

/// Execute a gRPC client action
async fn execute_grpc_action(
    client_id: ClientId,
    action: serde_json::Value,
    grpc_client_data: Arc<Mutex<GrpcClientData>>,
    app_state: &Arc<AppState>,
    llm_client: &OllamaClient,
    status_tx: &mpsc::UnboundedSender<String>,
    protocol: &Arc<GrpcClientProtocol>,
    dispatch: Dispatch,
) -> Result<Applied> {
    // Parse action using the protocol's execute_action method
    let action_result = protocol.as_ref().execute_action(action.clone())?;
    apply_grpc_action(
        client_id,
        action_result,
        grpc_client_data,
        app_state,
        llm_client,
        status_tx,
        protocol,
        dispatch,
    )
    .await
}

/// Run one already-executed action. Shared by the connected-event handler and the
/// injected-command loop so the `grpc_call` decoding exists exactly once.
#[allow(clippy::too_many_arguments)]
async fn apply_grpc_action(
    client_id: ClientId,
    action_result: ClientActionResult,
    grpc_client_data: Arc<Mutex<GrpcClientData>>,
    app_state: &Arc<AppState>,
    llm_client: &OllamaClient,
    status_tx: &mpsc::UnboundedSender<String>,
    protocol: &Arc<GrpcClientProtocol>,
    dispatch: Dispatch,
) -> Result<Applied> {
    match action_result {
        ClientActionResult::Custom { name, data } if name == "grpc_call" => {
            let service = data["service"]
                .as_str()
                .context("Missing service in grpc_call")?;
            let method = data["method"]
                .as_str()
                .context("Missing method in grpc_call")?;
            let request = &data["request"];
            let metadata = data.get("metadata").and_then(|v| v.as_object());

            let sent = make_grpc_call(
                client_id,
                service,
                method,
                request.clone(),
                metadata.cloned(),
                grpc_client_data,
                app_state,
                llm_client,
                status_tx,
                protocol,
                dispatch,
            )
            .await?;

            match sent {
                Some(report) => Ok(Applied::Sent {
                    bytes_sent: report.bytes_sent,
                    pending_notify: report.pending_notify,
                }),
                // The per-connection state machine refused the call; nothing was
                // written, and saying "Sent" here would be a lie.
                None => Ok(Applied::Ran(format!(
                    "grpc_call {}/{} skipped: the client is already processing a call",
                    service, method
                ))),
            }
        }
        ClientActionResult::Disconnect => {
            info!("gRPC client {} disconnecting", client_id);
            app_state
                .update_client_status(client_id, ClientStatus::Disconnected)
                .await;
            let _ = status_tx.send(format!("[CLIENT] gRPC client {} disconnected", client_id));
            Ok(Applied::Disconnect)
        }
        ClientActionResult::WaitForMore => {
            debug!("gRPC client {} waiting", client_id);
            Ok(Applied::Ran("wait_for_more".to_string()))
        }
        ClientActionResult::NoAction => Ok(Applied::Ran("no_action".to_string())),
        // Not swallowed: an action this client cannot carry out says so, rather than
        // looking identical to success.
        ClientActionResult::Custom { name, .. } => Ok(Applied::Ran(format!(
            "custom result '{name}' is not handled by the gRPC client"
        ))),
        ClientActionResult::SendData(_) => Ok(Applied::Ran(
            "send_data has no meaning for a gRPC client (tonic owns the HTTP/2 channel)"
                .to_string(),
        )),
        ClientActionResult::Multiple(_) => Ok(Applied::Ran(
            "multiple results are not produced by the gRPC client".to_string(),
        )),
    }
}

/// Drain injected commands until the channel closes (the client was removed) or an
/// injected `disconnect` ends the session.
///
/// `command_support::handle_stream_client_command` cannot serve this client: there is no
/// write half NetGet owns - tonic holds the HTTP/2 channel - and `grpc_call` yields
/// `ClientActionResult::Custom`. The shared `Arc<Mutex<GrpcClientData>>` the connect path
/// already built is what makes this loop possible: the channel and the descriptor pool are
/// reachable from outside the connect task, so an injected action runs on the same
/// connection and the same schema as an LLM-produced one.
async fn grpc_command_loop(
    mut command_rx: mpsc::Receiver<ClientCommand>,
    client_id: ClientId,
    grpc_client_data: Arc<Mutex<GrpcClientData>>,
    app_state: Arc<AppState>,
    llm_client: OllamaClient,
    status_tx: mpsc::UnboundedSender<String>,
) {
    use crate::llm::actions::protocol_trait::Protocol;

    let protocol = Arc::new(GrpcClientProtocol::new());

    while let Some(command) = command_rx.recv().await {
        let action = command.action.clone();
        // Held until after the reply: see `Dispatch::Defer`.
        let mut pending_notify = None;
        let outcome = match protocol.as_ref().execute_action(action.clone()) {
            Err(e) => Ok(ClientSendOutcome::Rejected {
                error: e.to_string(),
            }),
            Ok(action_result) => match Box::pin(apply_grpc_action(
                client_id,
                action_result,
                grpc_client_data.clone(),
                &app_state,
                &llm_client,
                &status_tx,
                &protocol,
                Dispatch::Defer,
            ))
            .await
            {
                Ok(Applied::Sent {
                    bytes_sent,
                    pending_notify: pending,
                }) => {
                    pending_notify = pending;
                    Ok(ClientSendOutcome::Sent { bytes_sent })
                }
                Ok(Applied::Ran(detail)) => Ok(ClientSendOutcome::Executed { detail }),
                Ok(Applied::Disconnect) => Ok(ClientSendOutcome::Disconnected),
                Err(e) => Err(e),
            },
        };

        let outcome_json = match &outcome {
            Ok(outcome) => serde_json::to_value(outcome).unwrap_or(serde_json::Value::Null),
            Err(e) => serde_json::json!({"error": e.to_string()}),
        };
        app_state
            .record_access_log(
                AccessLogOwner::Client(client_id.as_u32()),
                protocol.protocol_name(),
                None,
                "injected_action",
                action,
                vec![outcome_json],
            )
            .await;

        let disconnect = matches!(outcome, Ok(ClientSendOutcome::Disconnected));
        if let Err(e) = &outcome {
            error!("gRPC client {} injected action failed: {}", client_id, e);
            let _ = status_tx.send(format!(
                "[WARN] Client {} injected action failed: {}",
                client_id, e
            ));
        }
        let _ = status_tx.send("__UPDATE_UI__".to_string());
        crate::client::command_support::reply(command, outcome);

        // Only now, with the caller already holding its answer, raise the response
        // event. A manual routing rule parking here costs the operator nothing but a
        // "client busy" on a *second* send, which is what the bounded channel is for.
        if let Some(event_data) = pending_notify {
            notify_grpc_response(
                client_id,
                event_data,
                grpc_client_data.clone(),
                app_state.clone(),
                llm_client.clone(),
                status_tx.clone(),
                protocol.clone(),
            )
            .await;
        }

        if disconnect {
            break;
        }
    }

    // Nothing can be injected any more: stop the dashboard offering [ send ].
    app_state.remove_client_handle(client_id).await;
    let _ = status_tx.send("__UPDATE_UI__".to_string());
    info!("gRPC client {} command loop ended", client_id);
}

/// One completed gRPC call.
struct GrpcCallReport {
    /// Framed bytes written to the channel: 5-byte gRPC header + protobuf payload.
    bytes_sent: usize,
    /// `grpc_response_received` payload not yet given to the LLM (see [`Dispatch`]).
    pending_notify: Option<serde_json::Value>,
}

/// Make a gRPC call.
///
/// Returns a [`GrpcCallReport`] when the call went out and was answered, or `None` when
/// the per-connection state machine refused it because another call is in flight. A
/// caller reporting a [`ClientSendOutcome`] must not turn `None` into `Sent`.
#[allow(clippy::too_many_arguments)]
/// Returns a boxed future rather than being a plain `async fn`.
///
/// The error path can retry the call the model asks for, which means this function calls
/// itself. rustc cannot infer the type of a directly self-referential `async fn` -- it is
/// an infinitely nested opaque type -- so the body is boxed once here and the recursion
/// becomes an ordinary dynamic call.
#[allow(clippy::too_many_arguments)]
fn make_grpc_call<'a>(
    client_id: ClientId,
    service: &'a str,
    method: &'a str,
    request_json: serde_json::Value,
    metadata: Option<serde_json::Map<String, serde_json::Value>>,
    grpc_client_data: Arc<Mutex<GrpcClientData>>,
    app_state: &'a Arc<AppState>,
    llm_client: &'a OllamaClient,
    status_tx: &'a mpsc::UnboundedSender<String>,
    protocol: &'a Arc<GrpcClientProtocol>,
    dispatch: Dispatch,
) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<Option<GrpcCallReport>>> + Send + 'a>>
{
    Box::pin(async move {
        // Check if client is in idle state
        {
            let data = grpc_client_data.lock().await;
            if matches!(data.state, ConnectionState::Processing) {
                info!("gRPC client {} is busy, skipping request", client_id);
                return Ok(None);
            }
        }

        // Set state to Processing
        {
            let mut data = grpc_client_data.lock().await;
            data.state = ConnectionState::Processing;
        }

        info!("gRPC client {} calling {}/{}", client_id, service, method);

        // Get descriptor pool and find method
        let (input_desc, output_desc) = {
            let data = grpc_client_data.lock().await;
            let method_desc = data
                .descriptor_pool
                .get_service_by_name(service)
                .and_then(|s| s.methods().find(|m| m.name() == method))
                .context(format!("Method {}/{} not found in schema", service, method))?;

            let input_desc = method_desc.input();
            let output_desc = method_desc.output();
            (input_desc, output_desc)
        };

        // Convert JSON request to protobuf
        let request_msg = json_to_dynamic_message(&request_json, &input_desc)
            .context("Failed to convert request JSON to protobuf")?;

        // Encode request
        let request_bytes = request_msg.encode_to_vec();

        info!(
            "gRPC client {} sending {}-byte request to {}/{}",
            client_id,
            request_bytes.len(),
            service,
            method
        );

        // Build gRPC request path
        let path = format!("/{}/{}", service, method);

        // Get channel
        let channel = {
            let data = grpc_client_data.lock().await;
            data.channel.clone()
        };

        // Create HTTP request with gRPC framing
        use http::HeaderValue;

        let mut request_builder = Request::builder()
            .method("POST")
            .uri(path.clone())
            .header("content-type", "application/grpc")
            .header("te", "trailers")
            .header("grpc-accept-encoding", "identity");

        // Add custom metadata
        if let Some(meta) = metadata {
            for (key, value) in meta {
                if let Some(val_str) = value.as_str() {
                    if let Ok(header_value) = HeaderValue::from_str(val_str) {
                        request_builder = request_builder.header(key.as_str(), header_value);
                    }
                }
            }
        }

        // Encode gRPC message with 5-byte header (compression flag + length)
        let mut grpc_message = Vec::with_capacity(5 + request_bytes.len());
        grpc_message.push(0); // No compression
        grpc_message.extend_from_slice(&(request_bytes.len() as u32).to_be_bytes());
        grpc_message.extend_from_slice(&request_bytes);

        // Create body using UnsyncBoxBody which is compatible with tonic
        use http_body_util::combinators::UnsyncBoxBody;
        let full_body = http_body_util::Full::new(Bytes::from(grpc_message));
        let body =
            UnsyncBoxBody::new(full_body.map_err(|_: std::convert::Infallible| {
                tonic::Status::internal("infallible error")
            }));
        let http_request = request_builder
            .body(body)
            .context("Failed to build HTTP request")?;

        // Make the call using the channel
        let result = call_grpc_unary(&channel, http_request).await;

        // Reset to idle
        {
            let mut data = grpc_client_data.lock().await;
            data.state = ConnectionState::Idle;
        }

        let framed_len = 5 + request_bytes.len();

        match result {
            Ok(response_bytes) => {
                // Decode response
                let response_msg = DynamicMessage::decode(output_desc.clone(), &response_bytes[..])
                    .context("Failed to decode gRPC response")?;

                // Convert to JSON
                let response_json = dynamic_message_to_json(&response_msg)?;

                info!(
                    "gRPC client {} received response for {}/{}",
                    client_id, service, method
                );

                let event_data = serde_json::json!({
                    "service": service,
                    "method": method,
                    "response": response_json,
                });

                let pending_notify = match dispatch {
                    Dispatch::Inline => {
                        notify_grpc_response(
                            client_id,
                            event_data,
                            grpc_client_data,
                            app_state.clone(),
                            llm_client.clone(),
                            status_tx.clone(),
                            protocol.clone(),
                        )
                        .await;
                        None
                    }
                    // A retry raises nothing: the caller already reported the original
                    // failure, and re-raising would restart the error loop this bound exists
                    // to prevent.
                    Dispatch::Silent => None,
                    Dispatch::Defer => Some(event_data),
                };

                Ok(Some(GrpcCallReport {
                    bytes_sent: framed_len,
                    pending_notify,
                }))
            }
            Err(e) => {
                error!("gRPC client {} call failed: {}", client_id, e);

                // Tell the model, and DO what it answers.
                //
                // The answer used to be dropped (`let _ = call_llm_for_client(...)`), so a
                // model that saw a call fail and wanted to retry it, call a fallback method,
                // or hang up was ignored -- on the one event whose entire purpose is to let it
                // react.
                //
                // A retry runs with `Dispatch::Silent`, which raises neither the success event
                // nor another error event. That bound is essential: without it a persistently
                // failing call would raise an error, be retried, fail, raise another error,
                // and drive the model round the same loop forever.
                if matches!(dispatch, Dispatch::Silent) {
                    // Already a retry. Do not ask again.
                    return Err(e);
                }
                if let Some(instruction) = app_state.get_instruction_for_client(client_id).await {
                    let event = Event::new(
                        &GRPC_CLIENT_ERROR_EVENT,
                        serde_json::json!({
                            "service": service,
                            "method": method,
                            "code": "UNKNOWN",
                            "message": e.to_string(),
                        }),
                    );

                    let memory = app_state
                        .get_memory_for_client(client_id)
                        .await
                        .unwrap_or_default();

                    match call_llm_for_client(
                        llm_client,
                        app_state,
                        client_id.to_string(),
                        &instruction,
                        &memory,
                        Some(&event),
                        protocol.as_ref(),
                        status_tx,
                    )
                    .await
                    {
                        Ok(result) => {
                            if let Some(mem) = result.memory_updates {
                                app_state.set_memory_for_client(client_id, mem).await;
                            }
                            for action in result.actions {
                                use crate::llm::actions::client_trait::Client;
                                match protocol.execute_action(action.clone()) {
                                    Ok(ClientActionResult::Custom { name, data })
                                        if name == "grpc_call" =>
                                    {
                                        let retry_service =
                                            data["service"].as_str().unwrap_or(service).to_string();
                                        let retry_method =
                                            data["method"].as_str().unwrap_or(method).to_string();
                                        info!(
                                            "gRPC client {} retrying as {}/{} on the model's \
                                         request",
                                            client_id, retry_service, retry_method
                                        );
                                        let retry = make_grpc_call(
                                            client_id,
                                            &retry_service,
                                            &retry_method,
                                            data.get("request").cloned().unwrap_or_default(),
                                            data.get("metadata")
                                                .and_then(|m| m.as_object())
                                                .cloned(),
                                            grpc_client_data.clone(),
                                            app_state,
                                            llm_client,
                                            status_tx,
                                            protocol,
                                            Dispatch::Silent,
                                        )
                                        .await;
                                        if let Err(re) = retry {
                                            error!(
                                                "gRPC client {} retry also failed: {}",
                                                client_id, re
                                            );
                                        }
                                    }
                                    Ok(ClientActionResult::Disconnect) => {
                                        info!(
                                            "gRPC client {} disconnecting after error, as the \
                                         model asked",
                                            client_id
                                        );
                                        app_state
                                            .update_client_status(
                                                client_id,
                                                crate::state::ClientStatus::Disconnected,
                                            )
                                            .await;
                                        let _ = status_tx.send("__UPDATE_UI__".to_string());
                                    }
                                    Ok(_) => {}
                                    Err(ae) => error!(
                                        "gRPC client {} rejected its own error-path action: {}",
                                        client_id, ae
                                    ),
                                }
                            }
                        }
                        Err(le) => error!(
                            "gRPC client {} LLM error on grpc_client_error: {}",
                            client_id, le
                        ),
                    }
                }

                Err(e)
            }
        }
    })
}

/// Raise `grpc_response_received` for a completed call and run whatever the LLM answers.
///
/// Split out of [`make_grpc_call`] so the injected-command loop can await the network
/// round-trip - and report a truthful byte count - without also awaiting the LLM.
#[allow(clippy::too_many_arguments)]
async fn notify_grpc_response(
    client_id: ClientId,
    event_data: serde_json::Value,
    grpc_client_data: Arc<Mutex<GrpcClientData>>,
    app_state: Arc<AppState>,
    llm_client: OllamaClient,
    status_tx: mpsc::UnboundedSender<String>,
    protocol: Arc<GrpcClientProtocol>,
) {
    let Some(instruction) = app_state.get_instruction_for_client(client_id).await else {
        return;
    };

    let event = Event::new(&GRPC_CLIENT_RESPONSE_RECEIVED_EVENT, event_data);
    let memory = app_state
        .get_memory_for_client(client_id)
        .await
        .unwrap_or_default();

    match call_llm_for_client(
        &llm_client,
        &app_state,
        client_id.to_string(),
        &instruction,
        &memory,
        Some(&event),
        protocol.as_ref(),
        &status_tx,
    )
    .await
    {
        Ok(ClientLlmResult {
            actions,
            memory_updates,
        }) => {
            // Update memory
            if let Some(mem) = memory_updates {
                app_state.set_memory_for_client(client_id, mem).await;
            }

            // Execute actions
            for action in actions {
                if let Err(e) = Box::pin(execute_grpc_action(
                    client_id,
                    action,
                    grpc_client_data.clone(),
                    &app_state,
                    &llm_client,
                    &status_tx,
                    &protocol,
                    Dispatch::Inline,
                ))
                .await
                {
                    error!("Failed to execute gRPC action: {}", e);
                }
            }
        }
        Err(e) => {
            error!("LLM error for gRPC client {}: {}", client_id, e);
        }
    }
}

/// Make a unary gRPC call using tonic channel
async fn call_grpc_unary(
    channel: &Channel,
    request: Request<http_body_util::combinators::UnsyncBoxBody<Bytes, tonic::Status>>,
) -> Result<Vec<u8>> {
    // Clone the channel to get a service we can call
    let mut client = channel.clone();

    // Call the service
    let response = client
        .ready()
        .await
        .context("gRPC channel not ready")?
        .call(request)
        .await
        .context("gRPC call failed")?;

    // Check gRPC status in headers
    let status_code = response
        .headers()
        .get("grpc-status")
        .and_then(|v| v.to_str().ok())
        .and_then(|s| s.parse::<i32>().ok())
        .unwrap_or(-1);

    if status_code != 0 && status_code != -1 {
        let status_message = response
            .headers()
            .get("grpc-message")
            .and_then(|v| v.to_str().ok())
            .unwrap_or("Unknown error");

        return Err(anyhow::anyhow!(
            "gRPC error: status={}, message={}",
            status_code,
            status_message
        ));
    }

    // Read response body
    let body = response.into_body();
    let body_bytes = body
        .collect()
        .await
        .context("Failed to read response body")?
        .to_bytes();

    // Decode gRPC framing (skip 5-byte header)
    if body_bytes.len() < 5 {
        return Err(anyhow::anyhow!("Response too short"));
    }

    let message_bytes = body_bytes.slice(5..);
    Ok(message_bytes.to_vec())
}

/// Convert JSON to dynamic protobuf message
fn json_to_dynamic_message(
    json: &serde_json::Value,
    descriptor: &MessageDescriptor,
) -> Result<DynamicMessage> {
    let mut msg = DynamicMessage::new(descriptor.clone());

    if let Some(obj) = json.as_object() {
        for (field_name, value) in obj {
            if let Some(field) = descriptor.get_field_by_name(field_name) {
                let proto_value = json_to_proto_value(value, &field)?;
                msg.set_field(&field, proto_value);
            }
        }
    }

    Ok(msg)
}

/// Convert JSON value to protobuf value
fn json_to_proto_value(
    json: &serde_json::Value,
    field: &prost_reflect::FieldDescriptor,
) -> Result<ProtoValue> {
    use prost_reflect::Kind;

    match field.kind() {
        Kind::Double => Ok(ProtoValue::F64(json.as_f64().unwrap_or(0.0))),
        Kind::Float => Ok(ProtoValue::F32(json.as_f64().unwrap_or(0.0) as f32)),
        Kind::Int32 | Kind::Sint32 | Kind::Sfixed32 => {
            Ok(ProtoValue::I32(json.as_i64().unwrap_or(0) as i32))
        }
        Kind::Int64 | Kind::Sint64 | Kind::Sfixed64 => {
            Ok(ProtoValue::I64(json.as_i64().unwrap_or(0)))
        }
        Kind::Uint32 | Kind::Fixed32 => Ok(ProtoValue::U32(json.as_u64().unwrap_or(0) as u32)),
        Kind::Uint64 | Kind::Fixed64 => Ok(ProtoValue::U64(json.as_u64().unwrap_or(0))),
        Kind::Bool => Ok(ProtoValue::Bool(json.as_bool().unwrap_or(false))),
        Kind::String => Ok(ProtoValue::String(json.as_str().unwrap_or("").to_string())),
        Kind::Bytes => {
            use base64::{engine::general_purpose, Engine as _};
            let s = json.as_str().unwrap_or("");
            let bytes = general_purpose::STANDARD.decode(s).unwrap_or_default();
            Ok(ProtoValue::Bytes(bytes.into()))
        }
        Kind::Message(msg_desc) => {
            let msg = json_to_dynamic_message(json, &msg_desc)?;
            Ok(ProtoValue::Message(msg))
        }
        Kind::Enum(enum_desc) => {
            if let Some(number) = json.as_i64() {
                Ok(ProtoValue::EnumNumber(number as i32))
            } else if let Some(name) = json.as_str() {
                if let Some(value) = enum_desc.get_value_by_name(name) {
                    Ok(ProtoValue::EnumNumber(value.number()))
                } else {
                    Ok(ProtoValue::EnumNumber(0))
                }
            } else {
                Ok(ProtoValue::EnumNumber(0))
            }
        }
    }
}

/// Convert dynamic protobuf message to JSON
fn dynamic_message_to_json(msg: &DynamicMessage) -> Result<serde_json::Value> {
    let mut map = serde_json::Map::new();

    for field in msg.descriptor().fields() {
        if msg.has_field(&field) {
            let value = msg.get_field(&field);
            let json_value = proto_value_to_json(&value)?;
            map.insert(field.name().to_string(), json_value);
        }
    }

    Ok(serde_json::Value::Object(map))
}

/// Convert protobuf value to JSON
fn proto_value_to_json(value: &ProtoValue) -> Result<serde_json::Value> {
    use base64::{engine::general_purpose, Engine as _};

    Ok(match value {
        ProtoValue::Bool(b) => serde_json::Value::Bool(*b),
        ProtoValue::I32(i) => serde_json::Value::Number((*i).into()),
        ProtoValue::I64(i) => serde_json::Value::Number((*i).into()),
        ProtoValue::U32(u) => serde_json::Value::Number((*u).into()),
        ProtoValue::U64(u) => serde_json::Value::Number((*u).into()),
        ProtoValue::F32(f) => serde_json::Value::Number(
            serde_json::Number::from_f64(*f as f64).unwrap_or(serde_json::Number::from(0)),
        ),
        ProtoValue::F64(f) => serde_json::Value::Number(
            serde_json::Number::from_f64(*f).unwrap_or(serde_json::Number::from(0)),
        ),
        ProtoValue::String(s) => serde_json::Value::String(s.clone()),
        ProtoValue::Bytes(b) => serde_json::Value::String(general_purpose::STANDARD.encode(b)),
        ProtoValue::EnumNumber(n) => serde_json::Value::Number((*n).into()),
        ProtoValue::Message(msg) => dynamic_message_to_json(msg)?,
        ProtoValue::List(list) => {
            let items: Result<Vec<_>> = list.iter().map(proto_value_to_json).collect();
            serde_json::Value::Array(items?)
        }
        ProtoValue::Map(map) => {
            let mut json_map = serde_json::Map::new();
            for (k, v) in map.iter() {
                let key_str = map_key_to_string(k);
                json_map.insert(key_str, proto_value_to_json(v)?);
            }
            serde_json::Value::Object(json_map)
        }
    })
}

/// Convert MapKey to string
fn map_key_to_string(key: &MapKey) -> String {
    match key {
        MapKey::Bool(b) => b.to_string(),
        MapKey::I32(i) => i.to_string(),
        MapKey::I64(i) => i.to_string(),
        MapKey::U32(u) => u.to_string(),
        MapKey::U64(u) => u.to_string(),
        MapKey::String(s) => s.clone(),
    }
}
