//! The dashboard rendered into ratatui's `TestBackend`: deterministic frames
//! without a pty, so the layout can be asserted on and read.
//!
//! The pty snapshots in `tests/terminal_snapshot/` prove the real binary
//! paints; these prove what it paints, for states a fresh process never
//! reaches in the two seconds a pty test waits (a running server with peers,
//! a client with verbs, a request parked for the human).

#![cfg(feature = "tcp")]

use ratatui::backend::TestBackend;
use ratatui::Terminal;

use netget::cli::theme::ColorPalette;
use netget::privilege::SystemCapabilities;
use netget::scripting::event_handler::EventPattern;
use netget::scripting::{EventHandler, EventHandlerConfig, EventHandlerType};
use netget::state::app_state::AccessLogEntry;
use netget::state::client::ClientStatus;
use netget::state::intercepts::{InterceptOwner, InterceptView};
use netget::state::server::ServerStatus;
use netget::state::{ClientId, ServerId};
use netget::tui::app::{DashboardApp, Focus, UiKey};
use netget::tui::cards::{Activate, InstanceAction};
use netget::tui::projection::{ClientRow, ConnRow, RailSnapshot, SendState, SendVerb, ServerRow};
use netget::tui::render;
use netget::tui::theme::Styles;

fn app() -> DashboardApp {
    let (status_tx, _status_rx) = tokio::sync::mpsc::unbounded_channel();
    let (ui_tx, _ui_rx) = tokio::sync::mpsc::unbounded_channel();
    let core = netget::ui::App::new(SystemCapabilities::detect());
    DashboardApp::new(
        core,
        Styles::from_palette(&ColorPalette::dark()),
        status_tx,
        ui_tx,
        netget::llm::OllamaClient::new("http://127.0.0.1:1"),
    )
}

fn manual() -> EventHandlerConfig {
    EventHandlerConfig {
        handlers: vec![EventHandler {
            event_pattern: EventPattern::Wildcard,
            handler: EventHandlerType::Manual { timeout_secs: 300 },
        }],
    }
}

fn populated() -> RailSnapshot {
    let requests: Vec<AccessLogEntry> = (1..=3)
        .map(|id| AccessLogEntry {
            id,
            unix_ms: 1_700_000_000_000 + id * 1000,
            server_id: Some(1),
            client_id: None,
            protocol: "HTTP".into(),
            connection_id: Some(7),
            event_type: "http_request".into(),
            request: serde_json::json!({"method": "GET", "path": format!("/{id}")}),
            response: vec![serde_json::json!({"type": "send_http_response", "status": 200})],
        })
        .collect();
    let server = ServerRow {
        id: ServerId::new(1),
        protocol: "HTTP".into(),
        port: 8080,
        local_addr: Some("127.0.0.1:8080".into()),
        status: ServerStatus::Running,
        instruction: "Serve a tiny site.".into(),
        memory_len: 0,
        startup_params: None,
        routing: Some(manual()),
        conns: vec![ConnRow {
            id: 7,
            remote_addr: "127.0.0.1:53121".into(),
            bytes_received: 1_234,
            bytes_sent: 45_678,
            active: true,
            can_message: false,
        }],
        recent: Vec::new(),
        requests,
        task_count: 0,
        uptime_secs: 133,
        client_counterpart: Some("HTTP".into()),
        intercepts: vec![InterceptView {
            id: 4,
            owner: InterceptOwner::Server(ServerId::new(1)),
            connection_id: Some(7),
            event_type: "http_request".into(),
            description: "GET /admin".into(),
            event_data: None,
            created_unix_ms: 1_700_000_004_000,
        }],
    };
    let broken = ServerRow {
        id: ServerId::new(2),
        protocol: "DNS".into(),
        port: 53,
        local_addr: None,
        status: ServerStatus::Error("bind 0.0.0.0:53: permission denied".into()),
        instruction: String::new(),
        memory_len: 0,
        startup_params: None,
        routing: None,
        conns: Vec::new(),
        recent: Vec::new(),
        requests: Vec::new(),
        task_count: 0,
        uptime_secs: 3,
        client_counterpart: Some("DNS".into()),
        intercepts: Vec::new(),
    };
    let client = ClientRow {
        id: ClientId::new(3),
        protocol: "telnet".into(),
        remote_addr: "127.0.0.1:2323".into(),
        status: ClientStatus::Connected,
        instruction: String::new(),
        memory_len: 0,
        startup_params: None,
        routing: None,
        connection: None,
        history: Vec::new(),
        requests: Vec::new(),
        task_count: 0,
        uptime_secs: 40,
        send_state: SendState::Ready,
        send_actions: vec![
            SendVerb {
                name: "send_command".into(),
                description: "Send a command line and wait for the prompt".into(),
            },
            SendVerb {
                name: "send_text".into(),
                description: "Send raw text".into(),
            },
        ],
        intercepts: Vec::new(),
    };
    RailSnapshot {
        servers: vec![server, broken],
        clients: vec![client],
        ..Default::default()
    }
}

/// Render one frame at `width`×`height` and return it as text lines.
fn frame(app: &mut DashboardApp, width: u16, height: u16) -> Vec<String> {
    let backend = TestBackend::new(width, height);
    let mut terminal = Terminal::new(backend).expect("test terminal");
    terminal.draw(|f| render::draw(f, app)).expect("draw");
    let buffer = terminal.backend().buffer().clone();
    (0..height)
        .map(|y| {
            (0..width)
                .map(|x| buffer[(x, y)].symbol().to_string())
                .collect::<String>()
                .trim_end()
                .to_string()
        })
        .collect()
}

fn dump(lines: &[String]) -> String {
    lines.join("\n")
}

#[test]
fn an_empty_dashboard_says_what_to_do_at_the_minimum_size() {
    let mut app = app();
    app.push_system("NetGet — a starts a server or client");
    app.absorb_snapshot(RailSnapshot::default());
    let lines = frame(&mut app, 80, 24);
    let text = dump(&lines);
    println!("{text}");
    assert!(text.contains("SERVERS 0 · CLIENTS 0"));
    assert!(text.contains("+ new server or client"));
    assert!(text.contains("ACTIVITY & CHAT"));
    assert!(text.contains("a starts a server or client"));
    assert!(
        text.contains("F1 keys"),
        "the status bar keeps its help hint at 80 columns"
    );
    assert!(!text.contains("╭ CHAT"), "one stream, not two panes");
    assert!(lines.iter().all(|l| l.chars().count() <= 80));
}

#[test]
fn a_populated_dashboard_shows_every_instance_with_its_buttons_and_sections() {
    let mut app = app();
    // The first snapshot only seeds the stream; the second is the change.
    app.absorb_snapshot(RailSnapshot::default());
    app.absorb_snapshot(populated());
    app.sample_metrics();
    app.snapshot.servers[0].conns[0].bytes_sent += 5_000;
    app.sample_metrics();
    app.focus = Focus::Cards;

    let lines = frame(&mut app, 120, 40);
    let text = dump(&lines);
    println!("{text}");

    // Every instance is there, with its summary line.
    let http = lines
        .iter()
        .find(|l| l.contains("#1") && l.contains("http"))
        .expect("http row");
    assert!(http.contains("●"), "{http}");
    assert!(http.contains(":8080"), "{http}");
    assert!(http.contains("1⇄"), "{http}");
    assert!(
        http.contains("⚠1"),
        "the parked request is flagged on the row: {http}"
    );
    assert!(http.contains("MANUAL"), "{http}");
    let dns = lines
        .iter()
        .find(|l| l.contains("#2") && l.contains("dns"))
        .expect("dns row");
    assert!(dns.contains("✗"), "{dns}");
    assert!(dns.contains("permission denied"), "{dns}");
    let telnet = lines
        .iter()
        .find(|l| l.contains("#3") && l.contains("telnet"))
        .expect("telnet row");
    assert!(telnet.contains("→127.0.0.1:2323"), "{telnet}");

    // Without selecting anything: the question, the buttons, the peer, its
    // request, and the client's verbs are all on screen.
    assert!(text.contains("YOUR answer needed · http_request from :53121"));
    assert!(text.contains("[ stop"), "{text}");
    assert!(text.contains("[ driver → LLM"), "{text}");
    assert!(text.contains("127.0.0.1:53121"), "the peer");
    assert!(
        text.contains("http_request → send_http_response"),
        "its request"
    );
    assert!(text.contains("send_command"), "the client's verbs");
    assert!(text.contains("up 2m13s"));
    assert!(text.contains("↓1.2K ↑50.7K"), "{text}");

    // The stream got the diff: the instances, the peer, the question.
    assert!(text.contains("listening on 127.0.0.1:8080"));
    assert!(text.contains("⇐ 127.0.0.1:53121 connected"));
    assert!(text.contains("http_request from :53121 needs YOUR"));
    assert!(
        text.contains("1 waiting for you"),
        "the status bar counts it too"
    );
    assert!(
        lines.last().unwrap().ends_with("F1 keys"),
        "{}",
        lines.last().unwrap()
    );
    assert!(lines.iter().all(|l| l.chars().count() <= 120));
}

#[test]
fn every_section_renders_unfolded_at_the_minimum_size() {
    use netget::tui::cards::{Group, NodeId};
    let mut app = app();
    app.absorb_snapshot(populated());
    for key in [
        UiKey::Server(ServerId::new(1)),
        UiKey::Client(ClientId::new(3)),
    ] {
        for group in [
            Group::Peers,
            Group::Connections,
            Group::Send,
            Group::Rules,
            Group::Config,
        ] {
            app.cards.state.open(&NodeId::Group(key, group));
        }
    }
    app.focus = Focus::Cards;
    let rows = app.rows();
    // Walk the whole column: every stop renders within the width.
    for (index, row) in rows.iter().enumerate() {
        if row.positions() == 0 {
            continue;
        }
        app.cards.row = index;
        app.cards.col = row.positions() - 1;
        let lines = frame(&mut app, 80, 24);
        assert!(lines.iter().all(|l| l.chars().count() <= 80), "row {index}");
    }
    let text = dump(&frame(&mut app, 100, 60));
    println!("{text}");
    assert!(text.contains("[ delete"), "rule rows carry their buttons");
    assert!(text.contains("[ + add rule"), "{text}");
    assert!(text.contains("instruction"), "config rows");
    assert!(text.contains("Send a command line"), "verbs");
}

#[test]
fn the_cursor_survives_its_card_vanishing() {
    let mut app = app();
    app.absorb_snapshot(populated());
    app.focus = Focus::Cards;
    app.focus_card(UiKey::Server(ServerId::new(1)));
    assert_eq!(app.cursor_key(), Some(UiKey::Server(ServerId::new(1))));
    let mut without_first = populated();
    without_first.servers.remove(0);
    app.absorb_snapshot(without_first);
    assert!(
        app.cursor_key().is_some(),
        "the cursor lands on whatever now sits there"
    );
    let _ = frame(&mut app, 80, 24);

    // Everything gone: the cursor sits on + new server or client.
    app.absorb_snapshot(RailSnapshot::default());
    let rows = app.rows();
    assert_eq!(rows[app.cards.row].on_enter, Activate::NewInstance);
    let text = dump(&frame(&mut app, 80, 24));
    assert!(text.contains("+ new server or client"));
}

#[test]
fn the_stream_keeps_its_newest_line_visible_when_older_ones_wrap() {
    let mut app = app();
    app.absorb_snapshot(RailSnapshot::default());
    for i in 0..12 {
        app.push_system(format!(
            "line {i}: a sentence long enough to wrap twice inside a forty column pane, easily"
        ));
    }
    app.push_system("THE NEWEST LINE");
    let text = dump(&frame(&mut app, 80, 24));
    println!("{text}");
    assert!(
        text.contains("THE NEWEST LINE"),
        "the tail must be visible while following:\n{text}"
    );
}

#[test]
fn an_instance_that_just_appeared_gets_the_cursor() {
    let mut app = app();
    app.focus = Focus::Cards;
    app.absorb_snapshot(RailSnapshot::default());
    app.absorb_snapshot(populated());
    assert_eq!(
        app.cursor_key(),
        Some(UiKey::Client(ClientId::new(3))),
        "the newest arrival"
    );
    let rows = app.rows();
    assert!(
        rows[app.cards.row].header.is_some(),
        "on its header, buttons one ↓ away"
    );
}

#[test]
fn the_stream_holds_the_conversation_and_the_activity_together() {
    let mut app = app();
    app.absorb_snapshot(RailSnapshot::default());
    app.push_chat(netget::tui::chat::EntryKind::User, "start an http server");
    app.absorb_snapshot(populated());
    app.push_system("Started server #1 (http)");
    let lines = frame(&mut app, 100, 30);
    let text = dump(&lines);
    println!("{text}");
    assert!(text.contains("ACTIVITY & CHAT"));
    let asked = lines
        .iter()
        .position(|l| l.contains("▶ start an http server"))
        .expect("what was typed");
    let listening = lines
        .iter()
        .position(|l| l.contains("listening on 127.0.0.1:8080"))
        .expect("what happened");
    let answered = lines
        .iter()
        .position(|l| l.contains("Started server #1 (http)"))
        .expect("what came back");
    assert!(
        asked < listening && listening < answered,
        "one timeline, in order"
    );
}

#[test]
fn the_button_grid_walks_with_the_arrows() {
    let mut app = app();
    app.absorb_snapshot(populated());
    app.focus = Focus::Cards;
    app.focus_card(UiKey::Server(ServerId::new(1)));
    let rows = app.rows();
    let header = app.cards.row;
    let first_buttons = (header..rows.len())
        .find(|i| !rows[*i].buttons.is_empty())
        .expect("a button row");
    let row = &rows[first_buttons];
    assert!(row.spans.is_empty(), "a grid row is buttons only");
    assert_eq!(
        row.button_at(0).map(|b| b.action),
        Some(InstanceAction::Stop)
    );
    assert_eq!(row.positions(), row.buttons.len());
    app.cards.row = first_buttons;
    app.cards.col = 1;
    let lines = frame(&mut app, 80, 24);
    assert!(lines.iter().all(|l| l.chars().count() <= 80));
    assert!(dump(&lines).contains("[ edit"));
}
