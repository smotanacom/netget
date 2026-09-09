//! Snowflake server implementation
//!
//! Serves the subset of Snowflake's REST/JSON client protocol that a driver needs
//! to log in and run a query:
//!
//! | Endpoint | Event | Model answers with |
//! |---|---|---|
//! | `POST /session/v1/login-request` | `snowflake_login` | a session token (or a refusal) |
//! | `POST /queries/v1/query-request` | `snowflake_query` | a rowset (or an error) |
//! | `POST /session/logout-request` | `snowflake_session` (logout) | an ack |
//! | `POST /session/token-request` | `snowflake_session` (token_renew) | renewed tokens |
//!
//! hyper owns HTTP; the LLM owns the JSON. **No storage**: there is no session
//! table and no row store — the model answers every request.
//!
//! **Fail closed.** When the model gives no usable answer, or the LLM call fails,
//! every endpoint replies with a Snowflake error envelope (`success:false`), never
//! a success-shaped empty result. On the login endpoint that means no token is ever
//! issued on an outage — an LLM failure is a refusal, and it is distinguishable in
//! the logs (and by message) from a deliberate model denial via `snowflake_error`.

pub mod actions;

use std::convert::Infallible;
use std::net::SocketAddr;
use std::sync::Arc;

use bytes::Bytes;
use http_body_util::{BodyExt, Full, Limited};
use hyper::body::{Body, Incoming};
use hyper::server::conn::http1;
use hyper::service::service_fn;
use hyper::{Method, Request, Response};
use hyper_util::rt::TokioIo;
use serde_json::{json, Value};
use tokio::sync::mpsc;
use tracing::{debug, error, warn};

use crate::llm::action_helper::call_llm;
use crate::llm::actions::protocol_trait::ActionResult;
use crate::llm::ollama_client::OllamaClient;
use crate::logging::emit::Log;
use crate::protocol::Event;
use crate::server::connection::ConnectionId;
use crate::server::snowflake::actions::{
    SnowflakeProtocol, SNOWFLAKE_LOGIN_EVENT, SNOWFLAKE_QUERY_EVENT, SNOWFLAKE_SESSION_EVENT,
};
use crate::state::app_state::AppState;

/// Largest request body this server will buffer.
///
/// `read_json_body` used an unbounded `req.into_body().collect()`, so an unauthenticated
/// POST to `/session/v1/login-request` could grow the process without limit. A login body
/// is a few hundred bytes and a query body is one SQL statement; 4 MiB is far beyond what
/// any driver sends.
const MAX_REQUEST_BYTES: usize = 4 * 1024 * 1024;

/// Snowflake error code sent when the authentication backend (the LLM) is
/// unavailable. 390100 is Snowflake's "incorrect username or password" — a
/// refusal a driver understands as an auth failure. Using it on an outage keeps
/// the login path fail-closed: no token is issued.
const CODE_AUTH_UNAVAILABLE: &str = "390100";
/// Generic internal error code used for a failed query/session request.
const CODE_INTERNAL: &str = "000603";
/// Server-busy style code used when the failure is LLM overload (retryable).
const CODE_REQUEST_TIMEOUT: &str = "000629";

/// Snowflake server.
pub struct SnowflakeServer;

impl SnowflakeServer {
    /// Spawn the Snowflake server. Binds with `?` so a bind failure surfaces as
    /// `ServerStatus::Error`, and registers the accept-loop `JoinHandle` so
    /// `stop_server` can release the socket.
    pub async fn spawn_with_llm_actions(
        listen_addr: SocketAddr,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        server_id: crate::state::ServerId,
    ) -> anyhow::Result<SocketAddr> {
        let listener =
            crate::server::socket_helpers::create_reusable_tcp_listener(listen_addr).await?;
        let local_addr = listener.local_addr()?;

        Log::new(Some(&status_tx)).info(format!("Snowflake server listening on {}", local_addr));

        let protocol = Arc::new(SnowflakeProtocol::new(
            ConnectionId::new(0),
            app_state.clone(),
            status_tx.clone(),
        ));

        let task_registrar = app_state.clone();
        let accept_handle = tokio::spawn(async move {
            loop {
                match listener.accept().await {
                    Ok((stream, remote_addr)) => {
                        let connection_id =
                            ConnectionId::new(app_state.get_next_unified_id().await);
                        let local_addr_conn = stream.local_addr().unwrap_or(local_addr);
                        debug!(
                            "Snowflake connection {} from {}",
                            connection_id, remote_addr
                        );

                        use crate::state::server::{
                            ConnectionState as ServerConnectionState, ConnectionStatus,
                            ProtocolConnectionInfo,
                        };
                        let now = std::time::Instant::now();
                        let conn_state = ServerConnectionState {
                            id: connection_id,
                            remote_addr,
                            local_addr: local_addr_conn,
                            bytes_sent: 0,
                            bytes_received: 0,
                            packets_sent: 0,
                            packets_received: 0,
                            last_activity: now,
                            status: ConnectionStatus::Active,
                            status_changed_at: now,
                            protocol_info: ProtocolConnectionInfo::empty(),
                        };
                        app_state
                            .add_connection_to_server(server_id, conn_state)
                            .await;
                        let _ = status_tx.send("__UPDATE_UI__".to_string());

                        let llm_client_clone = llm_client.clone();
                        let app_state_clone = app_state.clone();
                        let status_tx_clone = status_tx.clone();
                        let protocol_clone = protocol.clone();

                        tokio::spawn(async move {
                            let io = TokioIo::new(stream);
                            let status_for_service = status_tx_clone.clone();
                            let app_state_for_service = app_state_clone.clone();

                            let service = service_fn(move |req: Request<Incoming>| {
                                let llm_clone = llm_client_clone.clone();
                                let state_clone = app_state_for_service.clone();
                                let status_clone = status_for_service.clone();
                                let protocol_clone = protocol_clone.clone();
                                handle_snowflake_request(
                                    req,
                                    connection_id,
                                    server_id,
                                    llm_clone,
                                    state_clone,
                                    status_clone,
                                    protocol_clone,
                                )
                            });

                            if let Err(err) =
                                http1::Builder::new().serve_connection(io, service).await
                            {
                                error!("Error serving Snowflake connection: {:?}", err);
                            }

                            app_state_clone
                                .close_connection_on_server(server_id, connection_id)
                                .await;
                            Log::new(Some(&status_tx_clone))
                                .info(format!("Snowflake connection {connection_id} closed"));
                            let _ = status_tx_clone.send("__UPDATE_UI__".to_string());
                        });
                    }
                    Err(e) => {
                        Log::new(Some(&status_tx))
                            .error(format!("Failed to accept Snowflake connection: {}", e));
                        break;
                    }
                }
            }
        });

        task_registrar
            .register_server_task(server_id, accept_handle)
            .await;

        Ok(local_addr)
    }
}

/// Build a `200 OK application/json` response from a server-generated JSON body.
///
/// The body is our own JSON (the model influences only values inside it, which are
/// serialized safely), the status is a constant, and the headers are constant, so
/// this cannot fail — but it still falls back to a bare 500 rather than `unwrap()`
/// on the builder, per the codebase's no-panic-on-response rule.
fn json_200(body: String) -> Response<Full<Bytes>> {
    match Response::builder()
        .status(200)
        .header(hyper::header::CONTENT_TYPE, "application/json")
        .body(Full::new(Bytes::from(body)))
    {
        Ok(resp) => resp,
        Err(e) => {
            error!("Snowflake: failed to build response ({e}), sending bare 500");
            let mut fallback = Response::new(Full::new(Bytes::from_static(b"{\"success\":false}")));
            *fallback.status_mut() = hyper::StatusCode::INTERNAL_SERVER_ERROR;
            fallback
        }
    }
}

/// A Snowflake error envelope: `{"data":null,"code":..,"message":..,"success":false}`.
fn error_envelope(code: &str, message: &str) -> String {
    json!({
        "data": Value::Null,
        "code": code,
        "message": message,
        "success": false,
    })
    .to_string()
}

async fn handle_snowflake_request(
    req: Request<Incoming>,
    connection_id: ConnectionId,
    server_id: crate::state::ServerId,
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
    protocol: Arc<SnowflakeProtocol>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let method = req.method().clone();
    let uri = req.uri().clone();
    let path = uri.path().to_string();
    let query = uri.query().unwrap_or("").to_string();

    Log::new(Some(&status_tx)).debug(format!("Snowflake request: {} {}", method, path));

    app_state
        .update_connection_stats(server_id, connection_id, None, None, Some(1), None)
        .await;

    // Whether the client presented an Authorization session token.
    let has_auth_token = req
        .headers()
        .get(hyper::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())
        .map(|s| s.contains("Token=") && !s.contains("Token=\"\""))
        .unwrap_or(false);

    let response = match (method.clone(), path.as_str()) {
        (Method::POST, "/session/v1/login-request")
        | (Method::POST, "/session/authenticator-request") => {
            handle_login(
                req,
                connection_id,
                server_id,
                llm_client,
                app_state.clone(),
                status_tx.clone(),
                protocol,
            )
            .await
        }
        (Method::POST, "/queries/v1/query-request") => {
            handle_query(
                req,
                connection_id,
                server_id,
                llm_client,
                app_state.clone(),
                status_tx.clone(),
                protocol,
                has_auth_token,
                &query,
            )
            .await
        }
        (Method::POST, "/session/logout-request") => {
            handle_session(
                connection_id,
                server_id,
                llm_client,
                app_state.clone(),
                status_tx.clone(),
                protocol,
                "logout",
                has_auth_token,
            )
            .await
        }
        (Method::POST, "/session/token-request") => {
            handle_session(
                connection_id,
                server_id,
                llm_client,
                app_state.clone(),
                status_tx.clone(),
                protocol,
                "token_renew",
                has_auth_token,
            )
            .await
        }
        _ => {
            debug!("Snowflake: unknown endpoint {} {}", method, path);
            Ok(json_200(error_envelope(
                "390318",
                "Unknown Snowflake endpoint",
            )))
        }
    };

    let body_size = response
        .as_ref()
        .ok()
        .and_then(|resp| resp.body().size_hint().exact())
        .unwrap_or(0);
    app_state
        .update_connection_stats(
            server_id,
            connection_id,
            None,
            Some(body_size),
            None,
            Some(1),
        )
        .await;
    let _ = status_tx.send("__UPDATE_UI__".to_string());
    response
}

/// Read a request body into a JSON value, capped at [`MAX_REQUEST_BYTES`].
///
/// `Value::Null` on failure, which every caller treats as "no fields present" — and since
/// each endpoint fails closed when the model cannot answer from those fields, an oversized
/// or unreadable body can only ever lose a login, never grant one.
async fn read_json_body(req: Request<Incoming>) -> Value {
    match Limited::new(req.into_body(), MAX_REQUEST_BYTES)
        .collect()
        .await
    {
        Ok(collected) => {
            let bytes = collected.to_bytes();
            serde_json::from_slice(&bytes).unwrap_or(Value::Null)
        }
        Err(e) => {
            error!(
                "Snowflake: request body rejected (limit {} bytes): {}",
                MAX_REQUEST_BYTES, e
            );
            Value::Null
        }
    }
}

/// Find the first `snowflake_*` custom payload the executor produced.
fn first_custom(results: Vec<ActionResult>) -> Option<(String, Value)> {
    for r in results {
        if let ActionResult::Custom { name, data } = r {
            return Some((name, data));
        }
    }
    None
}

#[allow(clippy::too_many_arguments)]
async fn handle_login(
    req: Request<Incoming>,
    connection_id: ConnectionId,
    server_id: crate::state::ServerId,
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
    protocol: Arc<SnowflakeProtocol>,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let body = read_json_body(req).await;
    // Login body shape: {"data": {"LOGIN_NAME":..,"PASSWORD":..,"ACCOUNT_NAME":..,
    //                             "CLIENT_APP_ID":..,"CLIENT_APP_VERSION":..}}
    let data = body.get("data").cloned().unwrap_or(Value::Null);
    let login_name = data
        .get("LOGIN_NAME")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let account = data
        .get("ACCOUNT_NAME")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let has_password = data
        .get("PASSWORD")
        .and_then(|v| v.as_str())
        .map(|s| !s.is_empty())
        .unwrap_or(false);

    let event = Event::new(
        &SNOWFLAKE_LOGIN_EVENT,
        json!({
            "login_name": login_name,
            "account": account,
            "client_app_id": data.get("CLIENT_APP_ID").and_then(|v| v.as_str()).unwrap_or(""),
            "client_app_version": data.get("CLIENT_APP_VERSION").and_then(|v| v.as_str()).unwrap_or(""),
            "has_password": has_password,
        }),
    );

    match call_llm(
        &llm_client,
        &app_state,
        server_id,
        Some(connection_id),
        &event,
        &*protocol,
    )
    .await
    {
        Ok(execution_result) => match first_custom(execution_result.protocol_results) {
            Some((name, payload)) if name == "snowflake_login_success" => {
                let token = payload
                    .get("token")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                if token.is_empty() {
                    // Fail closed: a "success" with no token is unusable — refuse.
                    warn!("Snowflake login_success carried no token; refusing the login");
                    return Ok(json_200(error_envelope(
                        CODE_AUTH_UNAVAILABLE,
                        "netget: login backend returned no token",
                    )));
                }
                let validity = payload
                    .get("validity_seconds")
                    .and_then(|v| v.as_u64())
                    .unwrap_or(3600);
                let master = payload
                    .get("master_token")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| token.clone());
                let session_id = payload.get("session_id").cloned().unwrap_or(Value::Null);

                let out = json!({
                    "data": {
                        "token": token,
                        "masterToken": master,
                        "sessionId": session_id,
                        "validityInSeconds": validity,
                        "masterValidityInSeconds": validity * 4,
                        "displayUserName": login_name,
                        "serverVersion": "8.0.0",
                        "firstLogin": false,
                        "healthCheckInterval": 45,
                        "sessionInfo": {
                            "databaseName": Value::Null,
                            "schemaName": Value::Null,
                            "warehouseName": Value::Null,
                            "roleName": "PUBLIC"
                        },
                        "parameters": []
                    },
                    "message": Value::Null,
                    "code": Value::Null,
                    "success": true
                });
                Log::new(Some(&status_tx)).info(format!("Snowflake login OK for {login_name}"));
                Ok(json_200(out.to_string()))
            }
            Some((name, payload)) if name == "snowflake_error" => {
                // Deliberate model denial — carries the model's code/message.
                let code = payload
                    .get("code")
                    .and_then(|v| v.as_str())
                    .unwrap_or(CODE_AUTH_UNAVAILABLE);
                let message = payload
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Login refused");
                Log::new(Some(&status_tx)).info(format!("Snowflake login denied: {code}"));
                Ok(json_200(error_envelope(code, message)))
            }
            _ => {
                // No usable answer → refuse (fail closed), do not fabricate a token.
                warn!("Snowflake: no login action produced; refusing the login");
                Ok(json_200(error_envelope(
                    CODE_AUTH_UNAVAILABLE,
                    "netget: no login decision produced",
                )))
            }
        },
        Err(e) => {
            // LLM outage: refuse. No token is ever issued on failure, and the
            // message marks this as a backend outage, distinct from a model denial.
            let overloaded = crate::llm::is_overload_error(&e);
            Log::new(Some(&status_tx)).warn(format!(
                "Snowflake login LLM error (overload={}): {} - refusing",
                overloaded, e
            ));
            Ok(json_200(error_envelope(
                CODE_AUTH_UNAVAILABLE,
                crate::utils::WireFailure::classify(&e).prefixed_text(),
            )))
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_query(
    req: Request<Incoming>,
    connection_id: ConnectionId,
    server_id: crate::state::ServerId,
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
    protocol: Arc<SnowflakeProtocol>,
    has_auth_token: bool,
    query_string: &str,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let body = read_json_body(req).await;
    let sql_text = body
        .get("sqlText")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let request_id = query_string
        .split('&')
        .find_map(|kv| kv.strip_prefix("requestId="))
        .unwrap_or("")
        .to_string();

    let event = Event::new(
        &SNOWFLAKE_QUERY_EVENT,
        json!({
            "sql_text": sql_text,
            "has_auth_token": has_auth_token,
            "request_id": request_id,
        }),
    );

    match call_llm(
        &llm_client,
        &app_state,
        server_id,
        Some(connection_id),
        &event,
        &*protocol,
    )
    .await
    {
        Ok(execution_result) => match first_custom(execution_result.protocol_results) {
            Some((name, payload)) if name == "snowflake_query_response" => {
                let rowtype = normalize_rowtype(payload.get("rowtype"));
                let rowset = normalize_rowset(payload.get("rowset"));
                let returned = rowset.len();
                let query_id = payload
                    .get("query_id")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| "00000000-0000-0000-0000-000000000000".to_string());

                let out = json!({
                    "data": {
                        "parameters": [],
                        "rowtype": rowtype,
                        "rowsetBase64": Value::Null,
                        "rowset": rowset,
                        "total": returned,
                        "returned": returned,
                        "queryId": query_id,
                        "queryResultFormat": "json",
                        "finalDatabaseName": Value::Null,
                        "finalSchemaName": Value::Null,
                    },
                    "message": Value::Null,
                    "code": Value::Null,
                    "success": true
                });
                Log::new(Some(&status_tx)).info(format!("Snowflake query OK ({returned} rows)"));
                Ok(json_200(out.to_string()))
            }
            Some((name, payload)) if name == "snowflake_error" => {
                let code = payload
                    .get("code")
                    .and_then(|v| v.as_str())
                    .unwrap_or(CODE_INTERNAL);
                let message = payload
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Query failed");
                Log::new(Some(&status_tx)).info(format!("Snowflake query error: {code}"));
                Ok(json_200(error_envelope(code, message)))
            }
            _ => {
                warn!("Snowflake: no query action produced; returning error envelope");
                Ok(json_200(error_envelope(
                    CODE_INTERNAL,
                    "netget: no query result produced",
                )))
            }
        },
        Err(e) => {
            let overloaded = crate::llm::is_overload_error(&e);
            Log::new(Some(&status_tx)).warn(format!(
                "Snowflake query LLM error (overload={}): {}",
                overloaded, e
            ));
            let code = if overloaded {
                CODE_REQUEST_TIMEOUT
            } else {
                CODE_INTERNAL
            };
            Ok(json_200(error_envelope(
                code,
                crate::utils::WireFailure::classify(&e).prefixed_text(),
            )))
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_session(
    connection_id: ConnectionId,
    server_id: crate::state::ServerId,
    llm_client: OllamaClient,
    app_state: Arc<AppState>,
    status_tx: mpsc::UnboundedSender<String>,
    protocol: Arc<SnowflakeProtocol>,
    operation: &str,
    has_auth_token: bool,
) -> Result<Response<Full<Bytes>>, Infallible> {
    let event = Event::new(
        &SNOWFLAKE_SESSION_EVENT,
        json!({
            "operation": operation,
            "has_auth_token": has_auth_token,
        }),
    );

    match call_llm(
        &llm_client,
        &app_state,
        server_id,
        Some(connection_id),
        &event,
        &*protocol,
    )
    .await
    {
        Ok(execution_result) => match first_custom(execution_result.protocol_results) {
            Some((name, payload)) if name == "snowflake_session_response" => {
                let data = payload.get("data").cloned().unwrap_or(json!({}));
                let out = json!({
                    "data": data,
                    "message": Value::Null,
                    "code": Value::Null,
                    "success": true
                });
                Log::new(Some(&status_tx)).info(format!("Snowflake session OK ({operation})"));
                Ok(json_200(out.to_string()))
            }
            Some((name, payload)) if name == "snowflake_error" => {
                let code = payload
                    .get("code")
                    .and_then(|v| v.as_str())
                    .unwrap_or(CODE_INTERNAL);
                let message = payload
                    .get("message")
                    .and_then(|v| v.as_str())
                    .unwrap_or("Session request failed");
                Ok(json_200(error_envelope(code, message)))
            }
            _ => {
                // A logout with no answer is harmless to ack; a token renewal with no
                // answer must fail closed (no renewed token). Distinguish by operation.
                if operation == "logout" {
                    Ok(json_200(
                        json!({"data": {}, "message": Value::Null, "code": Value::Null, "success": true})
                            .to_string(),
                    ))
                } else {
                    warn!("Snowflake: no session action produced for token_renew; refusing");
                    Ok(json_200(error_envelope(
                        CODE_INTERNAL,
                        "netget: no session decision produced",
                    )))
                }
            }
        },
        Err(e) => {
            Log::new(Some(&status_tx)).warn(format!("Snowflake session LLM error: {}", e));
            Ok(json_200(error_envelope(
                CODE_INTERNAL,
                crate::utils::WireFailure::classify(&e).prefixed_text(),
            )))
        }
    }
}

/// Ensure each rowtype descriptor has the fields a driver expects. The model
/// supplies at least `name`/`type`; sensible defaults fill in the rest.
fn normalize_rowtype(rowtype: Option<&Value>) -> Vec<Value> {
    let arr = rowtype
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    arr.into_iter()
        .map(|col| {
            let name = col.get("name").and_then(|v| v.as_str()).unwrap_or("COL");
            let ty = col.get("type").and_then(|v| v.as_str()).unwrap_or("text");
            let nullable = col
                .get("nullable")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            json!({
                "name": name,
                "type": ty,
                "nullable": nullable,
                "length": col.get("length").cloned().unwrap_or(Value::Null),
                "precision": col.get("precision").cloned().unwrap_or(Value::Null),
                "scale": col.get("scale").cloned().unwrap_or(Value::Null),
                "byteLength": Value::Null,
                "collation": Value::Null
            })
        })
        .collect()
}

/// Snowflake's JSON result format transmits every cell as a string (or null).
/// Convert whatever the model produced into that shape.
fn normalize_rowset(rowset: Option<&Value>) -> Vec<Value> {
    let arr = rowset
        .and_then(|v| v.as_array())
        .cloned()
        .unwrap_or_default();
    arr.into_iter()
        .map(|row| {
            let cells = row.as_array().cloned().unwrap_or_default();
            Value::Array(
                cells
                    .into_iter()
                    .map(|c| match c {
                        Value::Null => Value::Null,
                        Value::String(s) => Value::String(s),
                        other => Value::String(other.to_string()),
                    })
                    .collect(),
            )
        })
        .collect()
}
