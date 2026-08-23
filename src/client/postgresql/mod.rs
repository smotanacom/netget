//! PostgreSQL client implementation
pub mod actions;

pub use actions::PostgresqlClientProtocol;

use anyhow::{Context, Result};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tracing::{error, info, trace};

use crate::client::llm_budget::call_llm_for_client;
use crate::client::postgresql::actions::{
    POSTGRESQL_CLIENT_CONNECTED_EVENT, POSTGRESQL_CLIENT_QUERY_RESULT_EVENT,
};
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::llm::ollama_client::OllamaClient;
use crate::llm::ClientLlmResult;
use crate::protocol::Event;
use crate::state::app_state::AppState;
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::{ClientId, ClientStatus};

/// What one executed action did to the PostgreSQL connection.
enum Applied {
    /// A query ran; the rows are the JSON result set reported back to the model.
    Query {
        query: String,
        rows: Vec<serde_json::Value>,
    },
    /// The session should end.
    Disconnect,
    /// The action executed but touched the connection in no way.
    Nothing(&'static str),
}

/// PostgreSQL client that connects to a PostgreSQL server
pub struct PostgresqlClient;

impl PostgresqlClient {
    /// Connect to a PostgreSQL server with integrated LLM actions
    pub async fn connect_with_llm_actions(
        remote_addr: String,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        client_id: ClientId,
        startup_params: Option<crate::protocol::StartupParams>,
    ) -> Result<SocketAddr> {
        // Extract connection parameters from startup_params if provided
        let (database, user, password) = if let Some(params) = &startup_params {
            let database = params
                .get_optional_string("database")?
                .unwrap_or_else(|| "postgres".to_string());
            let user = params
                .get_optional_string("user")?
                .unwrap_or_else(|| "postgres".to_string());
            let password = params
                .get_optional_string("password")?
                .unwrap_or_else(|| "".to_string());
            (database, user, password)
        } else {
            (
                "postgres".to_string(),
                "postgres".to_string(),
                "".to_string(),
            )
        };

        // Build connection string. `remote_addr` is NetGet's "host:port"; a libpq keyword
        // string has no such form — `host=127.0.0.1:5433` is read as a *hostname* containing
        // a colon and resolution fails, so the port has to be split out into its own keyword.
        let (host, port) = match remote_addr.rsplit_once(':') {
            Some((h, p)) => match p.parse::<u16>() {
                Ok(p) => (h.to_string(), p),
                Err(_) => (remote_addr.clone(), 5432),
            },
            None => (remote_addr.clone(), 5432),
        };
        let conn_str = if password.is_empty() {
            format!(
                "host={} port={} user={} dbname={}",
                host, port, user, database
            )
        } else {
            format!(
                "host={} port={} user={} password={} dbname={}",
                host, port, user, password, database
            )
        };

        info!(
            "PostgreSQL client {} connecting to {} (user={}, database={})",
            client_id, remote_addr, user, database
        );

        // Connect to PostgreSQL server
        let (client, connection) = tokio_postgres::connect(&conn_str, tokio_postgres::NoTls)
            .await
            .context(format!(
                "Failed to connect to PostgreSQL at {}",
                remote_addr
            ))?;

        // Get the local address from the connection's underlying socket
        // Note: tokio-postgres doesn't expose local_addr directly, so we parse from remote_addr
        let local_addr: SocketAddr = format!("{}:0", host)
            .parse()
            .unwrap_or_else(|_| "127.0.0.1:0".parse().unwrap());

        info!(
            "PostgreSQL client {} connected to {}",
            client_id, remote_addr
        );

        // Update client state
        app_state
            .update_client_status(client_id, ClientStatus::Connected)
            .await;
        let _ = status_tx.send(format!(
            "[CLIENT] PostgreSQL client {} connected",
            client_id
        ));
        let _ = status_tx.send("__UPDATE_UI__".to_string());

        // Spawn connection task
        let status_tx_clone = status_tx.clone();
        let connection_handle = tokio::spawn(async move {
            if let Err(e) = connection.await {
                error!("PostgreSQL client {} connection error: {}", client_id, e);
                let _ = status_tx_clone.send(format!(
                    "[CLIENT] PostgreSQL client {} connection error: {}",
                    client_id, e
                ));
            }
        });
        app_state
            .register_client_task(client_id, connection_handle)
            .await;

        // The driver's `Client` is shared: the connected-event LLM task and the injected
        // command loop both issue queries through it. Holding it in the command loop is
        // also what keeps the session alive for a client with no instruction.
        let client_arc = Arc::new(Mutex::new(client));
        let protocol = Arc::new(PostgresqlClientProtocol::new());

        // Command channel for injected actions (the dashboard's [ send ]).
        // Registered BEFORE the connected-event LLM call, which a manual `*` rule can park
        // for minutes — the operator must be able to reach the client while it waits.
        let command_rx =
            crate::client::command_support::register_command_channel(&app_state, client_id).await;
        let cmd_task = tokio::spawn(Self::command_loop(
            command_rx,
            protocol.clone(),
            client_arc.clone(),
            client_id,
            llm_client.clone(),
            app_state.clone(),
            status_tx.clone(),
        ));
        app_state.register_client_task(client_id, cmd_task).await;

        // Send connected event to LLM
        if let Some(instruction) = app_state.get_instruction_for_client(client_id).await {
            let event = Event::new(
                &POSTGRESQL_CLIENT_CONNECTED_EVENT,
                serde_json::json!({
                    "remote_addr": remote_addr,
                    "database": database,
                    "user": user,
                }),
            );

            let memory = app_state
                .get_memory_for_client(client_id)
                .await
                .unwrap_or_default();

            let protocol_clone = protocol.clone();
            let client_arc_clone = client_arc.clone();
            let llm_client_clone = llm_client.clone();
            let app_state_clone = app_state.clone();
            let status_tx_clone = status_tx.clone();

            let task_registrar = app_state.clone();
            let handle = tokio::spawn(async move {
                match call_llm_for_client(
                    &llm_client_clone,
                    &app_state_clone,
                    client_id.to_string(),
                    &instruction,
                    &memory,
                    Some(&event),
                    protocol_clone.as_ref(),
                    &status_tx_clone,
                )
                .await
                {
                    Ok(ClientLlmResult {
                        actions,
                        memory_updates,
                    }) => {
                        // Update memory
                        if let Some(mem) = memory_updates {
                            app_state_clone.set_memory_for_client(client_id, mem).await;
                        }

                        // Execute actions
                        for action in actions {
                            if let Err(e) = Self::execute_llm_action(
                                client_id,
                                action,
                                &protocol_clone,
                                &client_arc_clone,
                                &app_state_clone,
                                &llm_client_clone,
                                &status_tx_clone,
                            )
                            .await
                            {
                                error!("Error executing PostgreSQL action: {}", e);
                            }
                        }
                    }
                    Err(e) => {
                        error!("LLM error for PostgreSQL client {}: {}", client_id, e);
                    }
                }
            });
            task_registrar.register_client_task(client_id, handle).await;
        }

        Ok(local_addr)
    }

    /// Drain injected commands until the channel closes (the client was removed or stopped)
    /// or an injected `disconnect` ends the session.
    ///
    /// `tokio_postgres` owns the socket, so `execute_query` yields
    /// `ClientActionResult::Custom` and the generic split-stream arm cannot serve it; the
    /// effect goes through [`Self::apply_action`], the same function the LLM path uses.
    #[allow(clippy::too_many_arguments)]
    async fn command_loop(
        mut command_rx: mpsc::Receiver<ClientCommand>,
        protocol: Arc<PostgresqlClientProtocol>,
        pg_client: Arc<Mutex<tokio_postgres::Client>>,
        client_id: ClientId,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
    ) {
        use crate::llm::actions::protocol_trait::Protocol;
        use crate::state::AccessLogOwner;

        while let Some(command) = command_rx.recv().await {
            let action = command.action.clone();

            let mut follow_up: Option<(String, Vec<serde_json::Value>)> = None;
            let outcome = match protocol.execute_action(action.clone()) {
                Err(e) => Ok(ClientSendOutcome::Rejected {
                    error: e.to_string(),
                }),
                Ok(result) => match Self::apply_action(result, &pg_client, client_id).await {
                    Err(e) => Err(e),
                    Ok(Applied::Disconnect) => Ok(ClientSendOutcome::Disconnected),
                    Ok(Applied::Nothing(what)) => Ok(ClientSendOutcome::Executed {
                        detail: what.to_string(),
                    }),
                    Ok(Applied::Query { query, rows }) => {
                        let detail = format!(
                            "query executed by tokio_postgres ({} rows); \
                             no byte count — the driver owns the socket: {}",
                            rows.len(),
                            crate::utils::truncate::truncate_for_log(&query, 80)
                        );
                        follow_up = Some((query, rows));
                        Ok(ClientSendOutcome::Executed { detail })
                    }
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
                error!(
                    "PostgreSQL client {} injected action failed: {}",
                    client_id, e
                );
                let _ = status_tx.send(format!(
                    "[WARN] Client {} injected action failed: {}",
                    client_id, e
                ));
            }
            let _ = status_tx.send("__UPDATE_UI__".to_string());
            crate::client::command_support::reply(command, outcome);

            if disconnect {
                Self::mark_disconnected(client_id, &app_state, &status_tx).await;
                break;
            }

            // The model sees the result of a user-injected query exactly as it sees one it
            // asked for — after the reply, so the dashboard is not held for an LLM round-trip.
            if let Some((query, rows)) = follow_up {
                Self::report_query_result(
                    client_id,
                    &query,
                    rows,
                    &pg_client,
                    &protocol,
                    &app_state,
                    &llm_client,
                    &status_tx,
                )
                .await;
            }
        }

        app_state.remove_client_handle(client_id).await;
        let _ = status_tx.send("__UPDATE_UI__".to_string());
    }

    /// Execute one action produced by the LLM.
    async fn execute_llm_action(
        client_id: ClientId,
        action: serde_json::Value,
        protocol: &Arc<PostgresqlClientProtocol>,
        pg_client: &Arc<Mutex<tokio_postgres::Client>>,
        app_state: &Arc<AppState>,
        llm_client: &OllamaClient,
        status_tx: &mpsc::UnboundedSender<String>,
    ) -> Result<()> {
        match Self::apply_action(protocol.execute_action(action)?, pg_client, client_id).await {
            Ok(Applied::Query { query, rows }) => {
                Self::report_query_result(
                    client_id, &query, rows, pg_client, protocol, app_state, llm_client, status_tx,
                )
                .await;
            }
            Ok(Applied::Disconnect) => {
                info!("PostgreSQL client {} disconnecting", client_id);
                Self::mark_disconnected(client_id, app_state, status_tx).await;
            }
            Ok(Applied::Nothing(what)) => {
                trace!(
                    "PostgreSQL client {} action had no effect: {}",
                    client_id,
                    what
                );
            }
            Err(e) => {
                error!("PostgreSQL client {} query error: {}", client_id, e);
                let _ = status_tx.send(format!(
                    "[CLIENT] PostgreSQL client {} query error: {}",
                    client_id, e
                ));
            }
        }
        Ok(())
    }

    /// Run one executed action against the connection. Shared by the LLM path and injected
    /// commands so the query path exists exactly once.
    async fn apply_action(
        result: ClientActionResult,
        pg_client: &Arc<Mutex<tokio_postgres::Client>>,
        client_id: ClientId,
    ) -> Result<Applied> {
        match result {
            ClientActionResult::Custom { name, data } if name == "pg_query" => {
                let query = data
                    .get("query")
                    .and_then(|v| v.as_str())
                    .context("Missing 'query' in pg_query action data")?
                    .to_string();

                trace!("PostgreSQL client {} executing: {}", client_id, query);

                let rows = {
                    let guard = pg_client.lock().await;
                    guard
                        .query(query.as_str(), &[])
                        .await
                        .context("Failed to execute query")?
                };

                let result = rows
                    .iter()
                    .map(|row| {
                        let mut obj = serde_json::Map::new();
                        for (idx, col) in row.columns().iter().enumerate() {
                            let value: Option<String> = row.get(idx);
                            obj.insert(col.name().to_string(), serde_json::json!(value));
                        }
                        serde_json::Value::Object(obj)
                    })
                    .collect::<Vec<_>>();

                info!(
                    "PostgreSQL client {} query returned {} rows",
                    client_id,
                    result.len()
                );

                Ok(Applied::Query {
                    query,
                    rows: result,
                })
            }
            ClientActionResult::Custom { name, .. } => {
                trace!(
                    "PostgreSQL client {} ignoring custom result '{}'",
                    client_id,
                    name
                );
                Ok(Applied::Nothing("custom result not handled by this client"))
            }
            ClientActionResult::Disconnect => Ok(Applied::Disconnect),
            ClientActionResult::WaitForMore => Ok(Applied::Nothing("wait_for_more")),
            ClientActionResult::NoAction => Ok(Applied::Nothing("no_action")),
            ClientActionResult::SendData(_) => Ok(Applied::Nothing(
                "send_data is not meaningful for a tokio_postgres connection",
            )),
            ClientActionResult::Multiple(_) => Ok(Applied::Nothing(
                "multiple results not handled by this client",
            )),
        }
    }

    /// Raise `postgresql_query_result` with a query's rows and store any memory update.
    #[allow(clippy::too_many_arguments)]
    async fn report_query_result(
        client_id: ClientId,
        query: &str,
        rows: Vec<serde_json::Value>,
        pg_client: &Arc<Mutex<tokio_postgres::Client>>,
        protocol: &Arc<PostgresqlClientProtocol>,
        app_state: &Arc<AppState>,
        llm_client: &OllamaClient,
        status_tx: &mpsc::UnboundedSender<String>,
    ) {
        let Some(instruction) = app_state.get_instruction_for_client(client_id).await else {
            return;
        };

        let event = Event::new(
            &POSTGRESQL_CLIENT_QUERY_RESULT_EVENT,
            serde_json::json!({
                "query": query,
                "rows": rows,
                "row_count": rows.len(),
            }),
        );

        let memory = app_state
            .get_memory_for_client(client_id)
            .await
            .unwrap_or_default();

        // Execute what the model asked for.
        //
        // The result was matched as `actions: _, memory_updates: Some(mem)`, which threw
        // the actions away AND silently skipped the whole arm whenever the model returned
        // no memory update -- so on the common path nothing happened at all. A model that
        // read query results and wanted to issue the next query was ignored.
        //
        // `apply_action` runs a query and raises no event, so a follow-up cannot trigger
        // another result event and drive the model in circles, and the async type stays
        // non-recursive as tokio::spawn's Send bound requires.
        if let Ok(ClientLlmResult {
            actions,
            memory_updates,
        }) = call_llm_for_client(
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
            if let Some(mem) = memory_updates {
                app_state.set_memory_for_client(client_id, mem).await;
            }
            for action in actions {
                let decoded = match protocol.execute_action(action.clone()) {
                    Ok(d) => d,
                    Err(e) => {
                        error!(
                            "PostgreSQL client {} rejected its own follow-up action: {}",
                            client_id, e
                        );
                        continue;
                    }
                };
                match Self::apply_action(decoded, pg_client, client_id).await {
                    Ok(Applied::Query { query, rows }) => info!(
                        "PostgreSQL client {} follow-up query returned {} row(s): {}",
                        client_id,
                        rows.len(),
                        crate::utils::truncate_for_log(&query, 80)
                    ),
                    Ok(Applied::Nothing(detail)) => info!(
                        "PostgreSQL client {} follow-up produced no query: {}",
                        client_id, detail
                    ),
                    Ok(Applied::Disconnect) => {
                        info!(
                            "PostgreSQL client {} follow-up requested disconnect",
                            client_id
                        );
                        Self::mark_disconnected(client_id, app_state, status_tx).await;
                        break;
                    }
                    Err(e) => error!(
                        "PostgreSQL client {} follow-up action failed: {}",
                        client_id, e
                    ),
                }
            }
        }
    }

    async fn mark_disconnected(
        client_id: ClientId,
        app_state: &Arc<AppState>,
        status_tx: &mpsc::UnboundedSender<String>,
    ) {
        app_state
            .update_client_status(client_id, ClientStatus::Disconnected)
            .await;
        let _ = status_tx.send("__UPDATE_UI__".to_string());
    }
}
