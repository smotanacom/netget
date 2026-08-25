//! PyPI (Python Package Index) client implementation
pub mod actions;

pub use actions::PypiClientProtocol;

use anyhow::{Context, Result};
use std::net::SocketAddr;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{error, info};

use crate::client::llm_budget::call_llm_for_client;
use crate::client::pypi::actions::{
    PYPI_FILE_DOWNLOADED_EVENT, PYPI_PACKAGE_INFO_EVENT, PYPI_SEARCH_RESULTS_EVENT,
};
use crate::llm::actions::client_trait::{Client, ClientActionResult};
use crate::llm::ollama_client::OllamaClient;
use crate::llm::ClientLlmResult;
use crate::logging::emit::Log;
use crate::protocol::Event;
use crate::state::app_state::AppState;
use crate::state::client_handles::{ClientCommand, ClientSendOutcome};
use crate::state::{AccessLogOwner, ClientId, ClientStatus};

/// One completed PyPI metadata fetch.
///
/// Split out of the operations below so the injected-command loop can await the index
/// round-trip - and report a truthful outcome - without also awaiting the LLM call the
/// response event triggers.
pub struct PypiPackageInfo {
    pub package_name: String,
    pub info: serde_json::Value,
}

/// One completed file download, split from its event for the same reason.
pub struct PypiDownload {
    pub package_name: String,
    pub version: String,
    pub filename: String,
    pub size: usize,
}

/// One completed search. PyPI retired its search API, so this reaches no network at all.
pub struct PypiSearchResults {
    pub query: String,
    pub results: serde_json::Value,
}

/// What one executed action did.
enum Applied {
    /// The action ran; `detail` says what it did.
    Executed(String),
    /// The action asked to end the session.
    Disconnect,
}

/// PyPI client that interacts with Python Package Index
pub struct PypiClient;

impl PypiClient {
    /// Connect to PyPI with integrated LLM actions
    pub async fn connect_with_llm_actions(
        remote_addr: String,
        llm_client: OllamaClient,
        app_state: Arc<AppState>,
        status_tx: mpsc::UnboundedSender<String>,
        client_id: ClientId,
    ) -> Result<SocketAddr> {
        info!("PyPI client {} initialized for {}", client_id, remote_addr);

        // Build reqwest client
        let _http_client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .user_agent("NetGet-PyPI-Client/1.0")
            .build()
            .context("Failed to build HTTP client")?;

        // Parse index URL, default to pypi.org
        let index_url = if remote_addr.starts_with("http://") || remote_addr.starts_with("https://")
        {
            remote_addr.clone()
        } else {
            "https://pypi.org".to_string()
        };

        // Store client data
        app_state
            .with_client_mut(client_id, |client| {
                client.set_protocol_field(
                    "pypi_client".to_string(),
                    serde_json::json!("initialized"),
                );
                client.set_protocol_field("index_url".to_string(), serde_json::json!(index_url));
            })
            .await;

        // Update status
        app_state
            .update_client_status(client_id, ClientStatus::Connected)
            .await;
        Log::new(Some(&status_tx))
            .info(format!("PyPI client {} ready for {}", client_id, index_url));
        let _ = status_tx.send("__UPDATE_UI__".to_string());

        // Command channel for injected actions (the dashboard's [ send ] row).
        // Registered as soon as the client is usable and *before* anything that can
        // block for a human: a dashboard-created client defaults to a `*` -> manual
        // routing rule, and [ send ] must work while such an event is parked.
        let command_rx =
            crate::client::command_support::register_command_channel(&app_state, client_id).await;

        // The command loop replaces the old 5s "is the client gone yet?" poll: when the
        // client is removed its handle is dropped, the channel closes and `recv()`
        // returns None, so the loop notices removal immediately instead of up to 5s later.
        // Registered with AppState so stop_client can abort it —
        // dropping a JoinHandle only detaches it in Tokio.
        let task_registrar = app_state.clone();
        let connected_llm_client = llm_client.clone();
        let connected_status_tx = status_tx.clone();
        let task_handle = tokio::spawn(Self::command_loop(
            command_rx, client_id, app_state, llm_client, status_tx,
        ));
        task_registrar
            .register_client_task(client_id, task_handle)
            .await;

        // Raise the connected event.
        //
        // It was declared and never emitted, so the model was never consulted when a
        // PyPI client came up -- and this protocol's own
        // `get_startup_examples()` shows a `pypi_connected` handler, which could not
        // possibly have fired.
        //
        // Raised from its own registered task rather than inline: a dashboard-created
        // client defaults to a `*` -> manual rule, and awaiting a parked answer here would
        // block client creation itself.
        let connected_state = task_registrar.clone();
        let connected_llm = connected_llm_client;
        let connected_status = connected_status_tx;
        let connected = tokio::spawn(async move {
            let Some(instruction) = connected_state.get_instruction_for_client(client_id).await
            else {
                return;
            };
            let protocol = crate::client::pypi::actions::PypiClientProtocol::new();
            let event = Event::new(
                &crate::client::pypi::actions::PYPI_CLIENT_CONNECTED_EVENT,
                serde_json::json!({}),
            );
            match crate::client::llm_budget::call_llm_for_client(
                &connected_llm,
                &connected_state,
                client_id.to_string(),
                &instruction,
                "",
                Some(&event),
                &protocol,
                &connected_status,
            )
            .await
            {
                Ok(result) => {
                    if let Some(mem) = result.memory_updates {
                        connected_state.set_memory_for_client(client_id, mem).await;
                    }
                    Self::run_follow_ups(
                        client_id,
                        result.actions,
                        &connected_state,
                        &connected_status,
                    )
                    .await;
                }
                Err(e) => error!(
                    "PyPI client {} LLM error on connected event: {}",
                    client_id, e
                ),
            }
        });
        task_registrar
            .register_client_task(client_id, connected)
            .await;

        // Return a dummy local address (PyPI is connectionless HTTP)
        Ok("0.0.0.0:0".parse().unwrap())
    }

    /// Get package information from PyPI and hand it to the LLM.
    pub async fn get_package_info(
        client_id: ClientId,
        package_name: String,
        app_state: Arc<AppState>,
        llm_client: OllamaClient,
        status_tx: mpsc::UnboundedSender<String>,
    ) -> Result<()> {
        let info =
            Self::perform_get_package_info(client_id, package_name, &app_state, &status_tx).await?;
        Self::notify_package_info(client_id, info, app_state, llm_client, status_tx).await;
        Ok(())
    }

    /// Fetch package metadata only. No LLM involvement, so a caller can await this and
    /// know exactly what the index answered.
    pub async fn perform_get_package_info(
        client_id: ClientId,
        package_name: String,
        app_state: &AppState,
        status_tx: &mpsc::UnboundedSender<String>,
    ) -> Result<PypiPackageInfo> {
        let index_url = Self::index_url(app_state, client_id).await?;
        let url = format!("{}/pypi/{}/json", index_url, package_name);

        info!(
            "PyPI client {} fetching package info: {}",
            client_id, package_name
        );

        let http_client = Self::http_client()?;

        match http_client.get(&url).send().await {
            Ok(response) => {
                if !response.status().is_success() {
                    let status = response.status();
                    Log::new(Some(status_tx)).error(format!(
                        "PyPI client {} failed to get package info: {} {}",
                        client_id,
                        status.as_u16(),
                        status
                    ));
                    return Err(anyhow::anyhow!("Package not found: {}", status));
                }

                let json: serde_json::Value = response.json().await?;

                info!(
                    "PyPI client {} received package info for {}",
                    client_id, package_name
                );

                Ok(PypiPackageInfo {
                    package_name,
                    info: json,
                })
            }
            Err(e) => {
                Log::new(Some(status_tx))
                    .error(format!("PyPI client {} request failed: {}", client_id, e));
                Err(e.into())
            }
        }
    }

    /// Raise `pypi_package_info` for a completed metadata fetch.
    /// Run the actions the model returned for a response event.
    ///
    /// They were discarded (`actions: _`), so answering pypi_package_info_received or its siblings did nothing at all --
    /// the whole point of raising the event.
    ///
    /// Everything goes through the `perform_*` round-trips, which raise no event. That
    /// bounds the loop -- a follow-up cannot trigger another response event and drive the
    /// model in circles -- and is the only shape that compiles, since routing back through
    /// the notifying entry points makes notify -> perform -> notify a self-referential
    /// async chain rustc cannot prove `Send`.
    async fn run_follow_ups(
        client_id: ClientId,
        actions: Vec<serde_json::Value>,
        app_state: &AppState,
        status_tx: &mpsc::UnboundedSender<String>,
    ) {
        use crate::llm::actions::client_trait::{Client, ClientActionResult};
        let protocol = crate::client::pypi::actions::PypiClientProtocol::new();
        for action in actions {
            let Ok(ClientActionResult::Custom { name, data }) =
                protocol.execute_action(action.clone())
            else {
                continue;
            };
            let outcome: Result<()> = match name.as_str() {
                "pypi_get_package_info" => Self::perform_get_package_info(
                    client_id,
                    data["package_name"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                    app_state,
                    status_tx,
                )
                .await
                .map(|_| ()),
                "pypi_search_packages" => Self::perform_search_packages(
                    client_id,
                    data["query"].as_str().unwrap_or_default().to_string(),
                    data["limit"].as_u64().unwrap_or(10),
                    status_tx,
                )
                .await
                .map(|_| ()),
                "pypi_download_package" => Self::perform_download_package(
                    client_id,
                    data["package_name"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                    data["version"].as_str().map(|s| s.to_string()),
                    data["filename"].as_str().map(|s| s.to_string()),
                    app_state,
                    status_tx,
                )
                .await
                .map(|_| ()),
                "pypi_list_package_files" => Self::perform_list_package_files(
                    client_id,
                    data["package_name"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                    data["version"].as_str().map(|s| s.to_string()),
                    app_state,
                )
                .await
                .map(|_| ()),
                other => {
                    info!(
                        "PyPI client {} follow-up '{}' has no non-notifying path; skipped",
                        client_id, other
                    );
                    Ok(())
                }
            };
            if let Err(e) = outcome {
                error!("PyPI client {} follow-up action failed: {}", client_id, e);
            }
        }
    }

    async fn notify_package_info(
        client_id: ClientId,
        info: PypiPackageInfo,
        app_state: Arc<AppState>,
        llm_client: OllamaClient,
        status_tx: mpsc::UnboundedSender<String>,
    ) {
        Self::notify(
            client_id,
            &PYPI_PACKAGE_INFO_EVENT,
            serde_json::json!({
                "package_name": info.package_name,
                "info": info.info,
            }),
            app_state,
            llm_client,
            status_tx,
        )
        .await;
    }

    /// The client's configured index URL.
    async fn index_url(app_state: &AppState, client_id: ClientId) -> Result<String> {
        app_state
            .with_client_mut(client_id, |client| {
                client
                    .get_protocol_field("index_url")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string())
            })
            .await
            .flatten()
            .context("No index URL found")
    }

    fn http_client() -> Result<reqwest::Client> {
        Ok(reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .user_agent("NetGet-PyPI-Client/1.0")
            .build()?)
    }

    /// Raise one response event and hand it to the LLM. The single LLM entry point for
    /// every PyPI operation, so the split between "network" and "event" exists once.
    async fn notify(
        client_id: ClientId,
        event_type: &'static crate::protocol::EventType,
        event_data: serde_json::Value,
        app_state: Arc<AppState>,
        llm_client: OllamaClient,
        status_tx: mpsc::UnboundedSender<String>,
    ) {
        let Some(instruction) = app_state.get_instruction_for_client(client_id).await else {
            return;
        };

        let protocol = Arc::new(crate::client::pypi::actions::PypiClientProtocol::new());
        let event = Event::new(event_type, event_data);

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
                if let Some(mem) = memory_updates {
                    app_state.set_memory_for_client(client_id, mem).await;
                }
                Self::run_follow_ups(client_id, actions, &app_state, &status_tx).await;
            }
            Err(e) => {
                error!("LLM error for PyPI client {}: {}", client_id, e);
            }
        }
    }

    /// Search for packages on PyPI and hand the (non-)results to the LLM.
    pub async fn search_packages(
        client_id: ClientId,
        query: String,
        limit: u64,
        app_state: Arc<AppState>,
        llm_client: OllamaClient,
        status_tx: mpsc::UnboundedSender<String>,
    ) -> Result<()> {
        let results = Self::perform_search_packages(client_id, query, limit, &status_tx).await?;
        Self::notify(
            client_id,
            &PYPI_SEARCH_RESULTS_EVENT,
            serde_json::json!({
                "query": results.query,
                "results": results.results,
            }),
            app_state,
            llm_client,
            status_tx,
        )
        .await;
        Ok(())
    }

    /// Build the search result only.
    ///
    /// PyPI deprecated its XML-RPC search API and the replacement is HTML, so this
    /// deliberately contacts **nothing** and returns an explanatory payload. Callers must
    /// not report it as bytes on the wire.
    pub async fn perform_search_packages(
        client_id: ClientId,
        query: String,
        _limit: u64,
        status_tx: &mpsc::UnboundedSender<String>,
    ) -> Result<PypiSearchResults> {
        let url = format!("https://pypi.org/search/?q={}", urlencoding::encode(&query));

        Log::new(Some(status_tx)).info(format!(
            "PyPI client {} searching for: {}",
            client_id, query
        ));

        let results = serde_json::json!({
            "message": "PyPI search API is deprecated. Use 'get_package_info' for specific packages.",
            "query": query,
            "search_url": url,
            "suggestion": "Try using package names directly with get_package_info action",
        });

        Ok(PypiSearchResults { query, results })
    }

    /// Download a package file from PyPI and hand the result to the LLM.
    pub async fn download_package(
        client_id: ClientId,
        package_name: String,
        version: Option<String>,
        filename: Option<String>,
        app_state: Arc<AppState>,
        llm_client: OllamaClient,
        status_tx: mpsc::UnboundedSender<String>,
    ) -> Result<()> {
        let download = Self::perform_download_package(
            client_id,
            package_name,
            version,
            filename,
            &app_state,
            &status_tx,
        )
        .await?;
        Self::notify(
            client_id,
            &PYPI_FILE_DOWNLOADED_EVENT,
            serde_json::json!({
                "filename": download.filename,
                "size": download.size,
                "package": download.package_name,
                "version": download.version,
            }),
            app_state,
            llm_client,
            status_tx,
        )
        .await;
        Ok(())
    }

    /// Download the file only. No LLM involvement.
    pub async fn perform_download_package(
        client_id: ClientId,
        package_name: String,
        version: Option<String>,
        filename: Option<String>,
        app_state: &AppState,
        status_tx: &mpsc::UnboundedSender<String>,
    ) -> Result<PypiDownload> {
        let index_url = Self::index_url(app_state, client_id).await?;

        // First, get package info to find download URLs
        let info_url = format!("{}/pypi/{}/json", index_url, package_name);

        let http_client = Self::http_client()?;

        let json: serde_json::Value = http_client.get(&info_url).send().await?.json().await?;

        // Get the appropriate version
        let target_version =
            version.unwrap_or_else(|| json["info"]["version"].as_str().unwrap_or("").to_string());

        // Get URLs for this version
        let urls = json["urls"].as_array().context("No URLs found")?;

        // Find the file to download
        let file_info = if let Some(fname) = filename {
            urls.iter().find(|u| u["filename"].as_str() == Some(&fname))
        } else {
            // Default to first wheel, or first sdist
            urls.iter()
                .find(|u| u["packagetype"].as_str() == Some("bdist_wheel"))
                .or_else(|| {
                    urls.iter()
                        .find(|u| u["packagetype"].as_str() == Some("sdist"))
                })
        }
        .context("No suitable file found")?;

        let download_url = file_info["url"].as_str().context("No download URL")?;
        let file_name = file_info["filename"]
            .as_str()
            .context("No filename")?
            .to_string();

        Log::new(Some(status_tx)).info(format!(
            "PyPI client {} downloading: {}",
            client_id, file_name
        ));

        // Download the file
        let response = http_client.get(download_url).send().await?;
        let bytes = response.bytes().await?;

        info!(
            "PyPI client {} downloaded {} ({} bytes)",
            client_id,
            file_name,
            bytes.len()
        );

        Ok(PypiDownload {
            package_name,
            version: target_version,
            filename: file_name,
            size: bytes.len(),
        })
    }

    /// List available files for a package version and hand them to the LLM.
    pub async fn list_package_files(
        client_id: ClientId,
        package_name: String,
        version: Option<String>,
        app_state: Arc<AppState>,
        llm_client: OllamaClient,
        status_tx: mpsc::UnboundedSender<String>,
    ) -> Result<()> {
        let info =
            Self::perform_list_package_files(client_id, package_name, version, &app_state).await?;
        Self::notify_package_info(client_id, info, app_state, llm_client, status_tx).await;
        Ok(())
    }

    /// Fetch the file list only. No LLM involvement.
    pub async fn perform_list_package_files(
        client_id: ClientId,
        package_name: String,
        version: Option<String>,
        app_state: &AppState,
    ) -> Result<PypiPackageInfo> {
        let index_url = Self::index_url(app_state, client_id).await?;
        let info_url = format!("{}/pypi/{}/json", index_url, package_name);

        let http_client = Self::http_client()?;

        let json: serde_json::Value = http_client.get(&info_url).send().await?.json().await?;

        let urls = json["urls"].as_array().context("No URLs found")?;

        let files: Vec<serde_json::Value> = urls
            .iter()
            .map(|u| {
                serde_json::json!({
                    "filename": u["filename"],
                    "packagetype": u["packagetype"],
                    "size": u["size"],
                    "python_version": u["python_version"],
                    "url": u["url"],
                })
            })
            .collect();

        info!(
            "PyPI client {} listed {} files for {}",
            client_id,
            files.len(),
            package_name
        );

        Ok(PypiPackageInfo {
            package_name,
            info: serde_json::json!({
                "files": files,
                "version": version
                    .unwrap_or_else(|| json["info"]["version"].as_str().unwrap_or("").to_string()),
            }),
        })
    }

    /// Apply one already-parsed action against the live PyPI client.
    ///
    /// The single place PyPI actions are turned into index traffic, so an action injected
    /// from the dashboard behaves exactly like one the model produced.
    ///
    /// Only the **network** half is awaited. The response event - and the LLM call it makes -
    /// is raised from its own registered task afterwards, so a `*` -> manual routing rule
    /// parking that event cannot wedge the command loop for the length of a human's think
    /// time. The outcome stays truthful because it describes what the index actually
    /// answered, which is known before the event is raised.
    async fn apply_action(
        client_id: ClientId,
        result: ClientActionResult,
        app_state: &Arc<AppState>,
        llm_client: &OllamaClient,
        status_tx: &mpsc::UnboundedSender<String>,
    ) -> Result<Applied> {
        match result {
            ClientActionResult::Custom { name, data } => match name.as_str() {
                "pypi_get_package_info" => {
                    let package_name = data["package_name"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string();
                    let requested = package_name.clone();

                    let info = Self::perform_get_package_info(
                        client_id,
                        package_name,
                        app_state,
                        status_tx,
                    )
                    .await?;
                    let detail = format!(
                        "get_package_info {requested} -> version {}",
                        info.info["info"]["version"].as_str().unwrap_or("unknown")
                    );
                    Self::spawn_notify(
                        client_id,
                        app_state,
                        Self::notify_package_info(
                            client_id,
                            info,
                            app_state.clone(),
                            llm_client.clone(),
                            status_tx.clone(),
                        ),
                    )
                    .await;
                    Ok(Applied::Executed(detail))
                }
                "pypi_list_package_files" => {
                    let package_name = data["package_name"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string();
                    let version = data["version"].as_str().map(|s| s.to_string());
                    let requested = package_name.clone();

                    let info = Self::perform_list_package_files(
                        client_id,
                        package_name,
                        version,
                        app_state,
                    )
                    .await?;
                    let detail = format!(
                        "list_package_files {requested} -> {} file(s)",
                        info.info["files"].as_array().map(|a| a.len()).unwrap_or(0)
                    );
                    Self::spawn_notify(
                        client_id,
                        app_state,
                        Self::notify_package_info(
                            client_id,
                            info,
                            app_state.clone(),
                            llm_client.clone(),
                            status_tx.clone(),
                        ),
                    )
                    .await;
                    Ok(Applied::Executed(detail))
                }
                "pypi_download_package" => {
                    let package_name = data["package_name"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string();
                    let version = data["version"].as_str().map(|s| s.to_string());
                    let filename = data["filename"].as_str().map(|s| s.to_string());

                    let download = Self::perform_download_package(
                        client_id,
                        package_name,
                        version,
                        filename,
                        app_state,
                        status_tx,
                    )
                    .await?;
                    let detail = format!(
                        "download_package {} {} -> {} ({} bytes)",
                        download.package_name, download.version, download.filename, download.size
                    );
                    let event_data = serde_json::json!({
                        "filename": download.filename,
                        "size": download.size,
                        "package": download.package_name,
                        "version": download.version,
                    });
                    Self::spawn_notify(
                        client_id,
                        app_state,
                        Self::notify(
                            client_id,
                            &PYPI_FILE_DOWNLOADED_EVENT,
                            event_data,
                            app_state.clone(),
                            llm_client.clone(),
                            status_tx.clone(),
                        ),
                    )
                    .await;
                    Ok(Applied::Executed(detail))
                }
                "pypi_search_packages" => {
                    let query = data["query"].as_str().unwrap_or_default().to_string();
                    let limit = data["limit"].as_u64().unwrap_or(20);
                    let requested = query.clone();

                    let results =
                        Self::perform_search_packages(client_id, query, limit, status_tx).await?;
                    let event_data = serde_json::json!({
                        "query": results.query,
                        "results": results.results,
                    });
                    Self::spawn_notify(
                        client_id,
                        app_state,
                        Self::notify(
                            client_id,
                            &PYPI_SEARCH_RESULTS_EVENT,
                            event_data,
                            app_state.clone(),
                            llm_client.clone(),
                            status_tx.clone(),
                        ),
                    )
                    .await;
                    // Deliberately not `Sent`: PyPI retired its search API, so this puts
                    // *nothing* on the wire - it only raises pypi_search_results with an
                    // explanatory payload.
                    Ok(Applied::Executed(format!(
                        "search_packages {requested:?} raised pypi_search_results without \
                         contacting the index: PyPI's search API is retired"
                    )))
                }
                other => Ok(Applied::Executed(format!(
                    "unknown PyPI custom result '{other}' was not executed"
                ))),
            },
            ClientActionResult::Disconnect => Ok(Applied::Disconnect),
            ClientActionResult::NoAction => Ok(Applied::Executed("no_action".to_string())),
            ClientActionResult::WaitForMore => Ok(Applied::Executed("wait_for_more".to_string())),
            ClientActionResult::SendData(_) => Ok(Applied::Executed(
                "PyPI owns no socket; raw send_data cannot be put on the wire".to_string(),
            )),
            ClientActionResult::Multiple(_) => Ok(Applied::Executed(
                "PyPI produces no Multiple results; nothing executed".to_string(),
            )),
        }
    }

    /// Raise a response event from its own task, registered so `stop_client` aborts it.
    async fn spawn_notify(
        client_id: ClientId,
        app_state: &Arc<AppState>,
        notify: impl std::future::Future<Output = ()> + Send + 'static,
    ) {
        let handle = tokio::spawn(notify);
        app_state.register_client_task(client_id, handle).await;
    }

    /// Serve injected commands until the client goes away.
    async fn command_loop(
        mut command_rx: mpsc::Receiver<ClientCommand>,
        client_id: ClientId,
        app_state: Arc<AppState>,
        llm_client: OllamaClient,
        status_tx: mpsc::UnboundedSender<String>,
    ) {
        use crate::llm::actions::protocol_trait::Protocol;
        let protocol = crate::client::pypi::actions::PypiClientProtocol::new();

        while let Some(command) = command_rx.recv().await {
            let action = command.action.clone();
            let outcome = match protocol.execute_action(action.clone()) {
                Err(e) => Ok(ClientSendOutcome::Rejected {
                    error: e.to_string(),
                }),
                // Never `Sent`: reqwest owns the socket and does not report how many bytes
                // the request serialised to, so a byte count here would be invented.
                Ok(result) => {
                    match Self::apply_action(client_id, result, &app_state, &llm_client, &status_tx)
                        .await
                    {
                        Ok(Applied::Executed(detail)) => Ok(ClientSendOutcome::Executed { detail }),
                        Ok(Applied::Disconnect) => Ok(ClientSendOutcome::Disconnected),
                        Err(e) => Err(e),
                    }
                }
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
                error!("PyPI client {} injected action failed: {}", client_id, e);
                let _ = status_tx.send(format!(
                    "[WARN] Client {} injected action failed: {}",
                    client_id, e
                ));
            }
            let _ = status_tx.send("__UPDATE_UI__".to_string());
            crate::client::command_support::reply(command, outcome);

            if disconnect {
                app_state
                    .update_client_status(client_id, ClientStatus::Disconnected)
                    .await;
                break;
            }
        }

        info!("PyPI client {} command loop finished", client_id);
        app_state.remove_client_handle(client_id).await;
        let _ = status_tx.send("__UPDATE_UI__".to_string());
    }
}
