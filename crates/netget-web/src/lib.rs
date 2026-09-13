//! NetGet in the browser.
//!
//! This crate is the wasm-bindgen surface between the page and NetGet. It owns nothing the
//! native program does not have — the same `AppState`, the same protocol servers, the same
//! dashboard — and wires them to what a page can offer instead of an OS:
//!
//! - **Terminal.** The dashboard renders through [`backend::WebBackend`], which emits ANSI
//!   for xterm.js; keys and mouse reports come back through [`NetGet::key`],
//!   [`NetGet::mouse`] and [`NetGet::text`].
//! - **Network.** Servers bind on the virtual loopback in `netget-tokio-wasm`. The page's own
//!   clients — a Telnet terminal, a fake browser — reach them through [`NetGet::connect`],
//!   which is a `TcpStream::connect` on that network with the bytes relayed to JS.
//! - **Model.** Every LLM request goes to the page through the handler set with
//!   [`NetGet::set_llm_handler`] as JSON — full messages, tools and all — and the page's
//!   answer (WebLLM, a local Ollama, or the visitor typing) comes back the same way.
//!
//! Everything runs on the JS event loop: `tokio::spawn` here is `spawn_local`.

#![cfg(target_arch = "wasm32")]

extern crate netget_tokio_wasm as tokio;

mod backend;
mod input;

use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use js_sys::{Function, Uint8Array};
use netget::cli::theme::{ColorPalette, Theme};
use netget::cli::Args;
use netget::events::EventHandler;
use netget::llm::{BridgeReply, BridgeRequest, LlmBridge, OllamaClient};
use netget::privilege::SystemCapabilities;
use netget::settings::Settings;
use netget::state::app_state::AppState;
use netget::ui::App;
use netget_crossterm_wasm::event::Event;
use ratatui::Terminal;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::mpsc;
use tokio_stream::wrappers::UnboundedReceiverStream;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::{spawn_local, JsFuture};

use backend::WebBackend;

/// How long a bridged LLM request may wait for the page. Generous: the "you are the model"
/// mode has a person typing the answer.
const LLM_TIMEOUT: Duration = Duration::from_secs(900);

/// The backend URL `Args` carries. Nothing dials it; it selects the code path in
/// `model_select::resolve_startup_model` that trusts `--model` instead of asking Ollama.
const BRIDGE_URL: &str = "browser://model";

struct Inner {
    events: mpsc::UnboundedSender<std::io::Result<Event>>,
    size: Rc<Cell<(u16, u16)>>,
    state: AppState,
    bridge: Arc<LlmBridge>,
    llm_handler: RefCell<Option<Function>>,
    /// Outbound byte queues of the page's open connections, by id.
    conns: RefCell<HashMap<u32, mpsc::UnboundedSender<Vec<u8>>>>,
    next_conn: Cell<u32>,
    /// The dashboard's status channel, once the loop hands it over.
    status_tx: RefCell<Option<mpsc::UnboundedSender<String>>>,
}

/// One running NetGet.
#[wasm_bindgen]
pub struct NetGet {
    inner: Rc<Inner>,
}

fn opt_str(options: &JsValue, key: &str) -> Option<String> {
    js_sys::Reflect::get(options, &JsValue::from_str(key))
        .ok()
        .and_then(|v| v.as_string())
}

fn opt_u16(options: &JsValue, key: &str) -> Option<u16> {
    js_sys::Reflect::get(options, &JsValue::from_str(key))
        .ok()
        .and_then(|v| v.as_f64())
        .map(|n| n as u16)
}

fn opt_fn(options: &JsValue, key: &str) -> Option<Function> {
    js_sys::Reflect::get(options, &JsValue::from_str(key))
        .ok()
        .and_then(|v| v.dyn_into::<Function>().ok())
}

fn init_logging(level: &str) {
    use tracing_subscriber::prelude::*;
    let fmt_layer = tracing_subscriber::fmt::layer()
        .with_ansi(false)
        .without_time()
        .with_writer(tracing_web::MakeWebConsoleWriter::new());
    let filter = tracing_subscriber::EnvFilter::try_new(level)
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info"));
    let _ = tracing_subscriber::registry()
        .with(filter)
        .with(fmt_layer)
        .try_init();
}

#[wasm_bindgen]
impl NetGet {
    /// Start NetGet. `options`:
    ///
    /// - `cols`, `rows`: the terminal size xterm.js reports.
    /// - `onOutput(bytes: Uint8Array)`: write these to xterm.js.
    /// - `onLlm(request: string) => Promise<string | object>`: answer a model request.
    ///   Optional here; see [`NetGet::set_llm_handler`].
    /// - `model`: the model name shown in the status bar and sent with every request.
    /// - `theme`: `"dark"` (default) or `"light"`.
    /// - `log`: a `tracing` filter for the browser console, default `"info"`.
    #[wasm_bindgen(constructor)]
    pub fn new(options: JsValue) -> Result<NetGet, JsValue> {
        console_error_panic_hook::set_once();
        init_logging(opt_str(&options, "log").as_deref().unwrap_or("info"));

        let on_output = opt_fn(&options, "onOutput")
            .ok_or_else(|| JsValue::from_str("options.onOutput is required"))?;
        let cols = opt_u16(&options, "cols").unwrap_or(120);
        let rows = opt_u16(&options, "rows").unwrap_or(36);
        let model = opt_str(&options, "model").unwrap_or_else(|| "web-llm".to_string());
        let theme = match opt_str(&options, "theme").as_deref() {
            Some("light") => Theme::Light,
            _ => Theme::Dark,
        };

        let state = AppState::new_with_options(false, BRIDGE_URL.to_string());
        let (bridge, bridge_rx) = LlmBridge::new();
        bridge.set_models(vec![model.clone()]);
        let llm =
            OllamaClient::new_bridge(bridge.clone(), LLM_TIMEOUT).with_app_state(state.clone());

        let (events_tx, events_rx) = mpsc::unbounded_channel();
        let size = Rc::new(Cell::new((cols, rows)));
        let inner = Rc::new(Inner {
            events: events_tx,
            size: size.clone(),
            state: state.clone(),
            bridge,
            llm_handler: RefCell::new(opt_fn(&options, "onLlm")),
            conns: RefCell::new(HashMap::new()),
            next_conn: Cell::new(1),
            status_tx: RefCell::new(None),
        });

        // The model bridge: every request the LLM client makes is handed to the page.
        spawn_local(pump_llm_requests(inner.clone(), bridge_rx));

        // The dashboard, exactly as `cli::run` builds it, on the xterm.js backend.
        let backend = WebBackend::new(size, on_output);
        let ready_inner = inner.clone();
        spawn_local(async move {
            state.set_llm_client(llm.clone()).await;
            let event_handler = EventHandler::new(state.clone(), llm.clone());
            let core = App::new(SystemCapabilities::detect());
            let settings = Settings::default();
            let args = Args::parse_from([
                "netget",
                "--openai-url",
                BRIDGE_URL,
                "--api-key",
                "browser",
                "--model",
                &model,
            ]);
            let palette = ColorPalette::from_theme(theme);
            let mut terminal = match Terminal::new(backend) {
                Ok(t) => t,
                Err(e) => {
                    web_sys::console::error_1(&JsValue::from_str(&format!(
                        "netget: terminal setup failed: {e}"
                    )));
                    return;
                }
            };
            let events = UnboundedReceiverStream::new(events_rx);
            if let Err(e) = netget::tui::run_dashboard_on(
                &mut terminal,
                events,
                state,
                core,
                event_handler,
                llm,
                settings,
                &args,
                palette,
                Some(Box::new(move |status_tx| {
                    *ready_inner.status_tx.borrow_mut() = Some(status_tx);
                })),
            )
            .await
            {
                web_sys::console::error_1(&JsValue::from_str(&format!(
                    "netget: dashboard stopped: {e:#}"
                )));
            }
        });

        Ok(NetGet { inner })
    }

    // ----- terminal -----------------------------------------------------------------------

    /// A key press, as the fields of a DOM `KeyboardEvent`:
    /// `{"key":"a","ctrl":false,"alt":false,"shift":false,"meta":false}`.
    /// Returns whether the dashboard took it (so the page can `preventDefault`).
    pub fn key(&self, json: &str) -> bool {
        let Ok(parsed) = serde_json::from_str::<input::KeyInput>(json) else {
            return false;
        };
        match input::key_event(&parsed) {
            Some(event) => {
                let _ = self.inner.events.send(Ok(event));
                true
            }
            None => false,
        }
    }

    /// A mouse report, from xterm.js's SGR sequence already split by the page:
    /// `{"kind":"down","button":"left","col":10,"row":3}` (0-based cell coordinates).
    pub fn mouse(&self, json: &str) {
        if let Ok(parsed) = serde_json::from_str::<input::MouseInput>(json) {
            if let Some(event) = input::mouse_event(&parsed) {
                let _ = self.inner.events.send(Ok(event));
            }
        }
    }

    /// Text typed or pasted into the terminal: one key per character, newline as Enter.
    pub fn text(&self, text: &str) {
        for event in input::text_events(text) {
            let _ = self.inner.events.send(Ok(event));
        }
    }

    /// The terminal was resized.
    pub fn resize(&self, cols: u16, rows: u16) {
        self.inner.size.set((cols, rows));
        let _ = self.inner.events.send(Ok(Event::Resize(cols, rows)));
    }

    // ----- model --------------------------------------------------------------------------

    /// Install the page's model. `handler(request: string)` receives one JSON request —
    /// `{id, kind, model, messages:[{role,content}], tools:[...]}` — and resolves to a JSON
    /// reply, as a string or an object: `{content?, tool_calls?: [{name, arguments}],
    /// prompt_tokens?, completion_tokens?}` or `{error: "..."}`.
    pub fn set_llm_handler(&self, handler: Function) {
        *self.inner.llm_handler.borrow_mut() = Some(handler);
    }

    /// Remove the page's model; requests fail until one is installed again.
    pub fn clear_llm_handler(&self) {
        *self.inner.llm_handler.borrow_mut() = None;
    }

    /// What `/model` lists: a JSON array of names the page can run.
    pub fn set_models(&self, json: &str) -> bool {
        match serde_json::from_str::<Vec<String>>(json) {
            Ok(models) => {
                self.inner.bridge.set_models(models);
                true
            }
            Err(_) => false,
        }
    }

    // ----- virtual network ----------------------------------------------------------------

    /// Ports with a server listening on the virtual network.
    pub fn listening_ports(&self) -> Vec<u16> {
        tokio::net::listening_ports()
    }

    /// The servers NetGet has, as a JSON array of
    /// `{id, protocol, port, status, connections}`, delivered to `callback`.
    pub fn servers(&self, callback: Function) {
        let state = self.inner.state.clone();
        spawn_local(async move {
            let servers = state.get_all_servers().await;
            let rows: Vec<serde_json::Value> = servers
                .iter()
                .map(|s| {
                    serde_json::json!({
                        "id": s.id.as_u32(),
                        "protocol": s.protocol_name,
                        "port": s.local_addr.map(|a| a.port()).unwrap_or(s.port),
                        "status": format!("{:?}", s.status),
                        "connections": s.connections.len(),
                    })
                })
                .collect();
            let json = serde_json::to_string(&rows).unwrap_or_else(|_| "[]".to_string());
            let _ = callback.call1(&JsValue::NULL, &JsValue::from_str(&json));
        });
    }

    /// Start a server the way the dashboard's own form does — through
    /// `cli::management::ServerForm`, so validation and defaults are the ones every other
    /// front end gets. `json`: `{"protocol":"telnet","port":2323,"instruction":"..."}`.
    /// `callback` receives `{"id": n}` or `{"error": "..."}`.
    pub fn start_server(&self, json: &str, callback: Function) {
        #[derive(serde::Deserialize)]
        struct Req {
            protocol: String,
            #[serde(default)]
            port: Option<u16>,
            #[serde(default)]
            host: Option<String>,
            #[serde(default)]
            instruction: Option<String>,
            #[serde(default)]
            event_handlers: Option<Vec<serde_json::Value>>,
        }
        let req = match serde_json::from_str::<Req>(json) {
            Ok(r) => r,
            Err(e) => {
                let _ = callback.call1(
                    &JsValue::NULL,
                    &JsValue::from_str(&serde_json::json!({ "error": e.to_string() }).to_string()),
                );
                return;
            }
        };
        let Some(status_tx) = self.inner.status_tx.borrow().clone() else {
            let _ = callback.call1(
                &JsValue::NULL,
                &JsValue::from_str(
                    &serde_json::json!({ "error": "the dashboard is not running yet" }).to_string(),
                ),
            );
            return;
        };
        let state = self.inner.state.clone();
        spawn_local(async move {
            let form = netget::cli::management::ServerForm {
                protocol: req.protocol,
                port: req.port,
                host: req.host,
                instruction: req.instruction,
                event_handlers: req.event_handlers,
                ..Default::default()
            };
            let result = match form.create(&state, status_tx).await {
                Ok(id) => serde_json::json!({ "id": id.as_u32() }),
                Err(e) => serde_json::json!({ "error": format!("{e:#}") }),
            };
            let _ = callback.call1(&JsValue::NULL, &JsValue::from_str(&result.to_string()));
        });
    }

    /// Open a connection to `port` on the virtual network. `on_data(bytes: Uint8Array)` is
    /// called for everything the server sends; `on_close(reason: string | null)` once, when
    /// the connection ends (a string is a connect error). Returns the connection id for
    /// [`NetGet::send`] and [`NetGet::close`].
    pub fn connect(&self, port: u16, on_data: Function, on_close: Function) -> u32 {
        let id = self.inner.next_conn.get();
        self.inner.next_conn.set(id.wrapping_add(1).max(1));
        let (tx, rx) = mpsc::unbounded_channel::<Vec<u8>>();
        self.inner.conns.borrow_mut().insert(id, tx);
        spawn_local(relay_connection(
            self.inner.clone(),
            id,
            port,
            rx,
            on_data,
            on_close,
        ));
        id
    }

    /// Send bytes on a connection opened with [`NetGet::connect`].
    pub fn send(&self, id: u32, data: &[u8]) -> bool {
        match self.inner.conns.borrow().get(&id) {
            Some(tx) => tx.send(data.to_vec()).is_ok(),
            None => false,
        }
    }

    /// Close the page's end of a connection. The server sees EOF; whatever it still sends
    /// is delivered until it closes too.
    pub fn close(&self, id: u32) {
        self.inner.conns.borrow_mut().remove(&id);
    }
}

/// Drain the bridge: each request becomes one call into the page's handler.
async fn pump_llm_requests(inner: Rc<Inner>, mut rx: mpsc::UnboundedReceiver<BridgeRequest>) {
    while let Some(request) = rx.recv().await {
        let handler = inner.llm_handler.borrow().clone();
        let Some(handler) = handler else {
            let _ = request
                .reply
                .send(Err("no model is attached to the page".to_string()));
            continue;
        };
        let json = match serde_json::to_string(&request) {
            Ok(j) => j,
            Err(e) => {
                let _ = request
                    .reply
                    .send(Err(format!("request not serialisable: {e}")));
                continue;
            }
        };
        let reply = request.reply;
        // Each request is its own task so a slow answer (a person typing) does not hold
        // up the next one.
        spawn_local(async move {
            let outcome = call_llm_handler(&handler, &json).await;
            let _ = reply.send(outcome);
        });
    }
}

async fn call_llm_handler(handler: &Function, request_json: &str) -> Result<BridgeReply, String> {
    let returned = handler
        .call1(&JsValue::NULL, &JsValue::from_str(request_json))
        .map_err(|e| format!("model handler threw: {}", describe_js(&e)))?;
    let value = match returned.dyn_into::<js_sys::Promise>() {
        Ok(promise) => JsFuture::from(promise)
            .await
            .map_err(|e| format!("model handler rejected: {}", describe_js(&e)))?,
        Err(value) => value,
    };
    let text = match value.as_string() {
        Some(s) => s,
        None => js_sys::JSON::stringify(&value)
            .map_err(|e| format!("model reply not serialisable: {}", describe_js(&e)))?
            .into(),
    };
    let parsed: serde_json::Value =
        serde_json::from_str(&text).map_err(|e| format!("model reply is not JSON: {e}"))?;
    if let Some(err) = parsed.get("error").and_then(|e| e.as_str()) {
        return Err(err.to_string());
    }
    serde_json::from_value::<BridgeReply>(parsed)
        .map_err(|e| format!("model reply has the wrong shape: {e}"))
}

fn describe_js(value: &JsValue) -> String {
    value
        .as_string()
        .or_else(|| {
            js_sys::Reflect::get(value, &JsValue::from_str("message"))
                .ok()
                .and_then(|m| m.as_string())
        })
        .unwrap_or_else(|| format!("{value:?}"))
}

/// One page-side connection: bytes from the server to `on_data`, bytes from the page to
/// the server, EOF in both directions honoured.
async fn relay_connection(
    inner: Rc<Inner>,
    id: u32,
    port: u16,
    mut rx: mpsc::UnboundedReceiver<Vec<u8>>,
    on_data: Function,
    on_close: Function,
) {
    let stream = match tokio::net::TcpStream::connect(("127.0.0.1", port)).await {
        Ok(s) => s,
        Err(e) => {
            inner.conns.borrow_mut().remove(&id);
            let _ = on_close.call1(&JsValue::NULL, &JsValue::from_str(&e.to_string()));
            return;
        }
    };
    let (mut reader, mut writer) = stream.into_split();
    let mut buf = vec![0u8; 16 * 1024];
    let mut page_open = true;
    loop {
        tokio::select! {
            read = reader.read(&mut buf) => match read {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let chunk = Uint8Array::from(&buf[..n]);
                    let _ = on_data.call1(&JsValue::NULL, &chunk);
                }
            },
            outbound = rx.recv(), if page_open => match outbound {
                Some(bytes) => {
                    if writer.write_all(&bytes).await.is_err() {
                        break;
                    }
                    let _ = writer.flush().await;
                }
                None => {
                    // The page closed its end: half-close, keep delivering what the
                    // server still has to say.
                    page_open = false;
                    let _ = writer.shutdown().await;
                }
            },
        }
    }
    inner.conns.borrow_mut().remove(&id);
    let _ = on_close.call1(&JsValue::NULL, &JsValue::NULL);
}
