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
use netget::tui::inspector::InspectorTab;
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
    app.push_system("NetGet — a starts a server");
    app.absorb_snapshot(RailSnapshot::default());
    let lines = frame(&mut app, 80, 24);
    let text = dump(&lines);
    println!("{text}");
    assert!(text.contains("SERVERS 0 · CLIENTS 0"));
    assert!(text.contains("+ new server or client"));
    assert!(text.contains("Nothing selected yet"));
    assert!(text.contains("start a server"));
    assert!(text.contains("ACTIVITY"));
    assert!(text.contains("Nothing has happened yet"));
    assert!(text.contains("CHAT"));
    assert!(
        text.contains("F1 keys"),
        "the status bar keeps its help hint at 80 columns"
    );
    // Every pane box closes at the right edge: no row wider than the terminal.
    assert!(lines.iter().all(|l| l.chars().count() <= 80));
}

#[test]
fn a_populated_dashboard_shows_every_instance_and_the_selected_one_in_depth() {
    let mut app = app();
    // The first snapshot only seeds the feed; the second is the change.
    app.absorb_snapshot(RailSnapshot::default());
    app.absorb_snapshot(populated());
    // Sample twice so a sparkline exists.
    app.sample_metrics();
    app.snapshot.servers[0].conns[0].bytes_sent += 5_000;
    app.sample_metrics();
    app.focus = Focus::Instances;
    app.select(UiKey::Server(ServerId::new(1)));

    let lines = frame(&mut app, 120, 36);
    let text = dump(&lines);
    println!("{text}");

    // The list: status glyph, id, protocol, port, live peers, driver.
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
    assert!(
        dns.contains("permission denied"),
        "an error row spends its width on the reason: {dns}"
    );
    let telnet = lines
        .iter()
        .find(|l| l.contains("#3") && l.contains("telnet"))
        .expect("telnet row");
    assert!(telnet.contains("→127.0.0.1:2323"), "{telnet}");
    assert!(telnet.contains("LLM"), "{telnet}");

    // The inspector: title, tabs, bar, and the waiting line first.
    assert!(text.contains("#1 http 127.0.0.1:8080"));
    assert!(text.contains("overview"));
    assert!(
        text.contains("Space actions"),
        "the inspector says how to act:\n{text}"
    );
    assert!(text.contains("YOUR answer needed · http_request from :53121"));
    assert!(text.contains("up 2m13s"));
    assert!(text.contains("↓1.2K ↑50.7K"), "{text}");

    // The feed got the diff: the instances, the peer, the requests, the question.
    assert!(text.contains("listening on 127.0.0.1:8080"));
    assert!(text.contains("permission denied"));
    assert!(text.contains("⇐ 127.0.0.1:53121 connected"));
    assert!(text.contains(":53121 http_request → send_http"));
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
fn every_tab_renders_for_both_kinds_at_the_minimum_size() {
    let mut app = app();
    app.absorb_snapshot(populated());
    for key in [
        UiKey::Server(ServerId::new(1)),
        UiKey::Client(ClientId::new(3)),
    ] {
        app.select(key);
        app.focus = Focus::Inspector;
        for tab in InspectorTab::for_key(key) {
            app.inspector.tab = tab;
            app.inspector.item = 0;
            let lines = frame(&mut app, 80, 24);
            let text = dump(&lines);
            assert!(
                text.contains(tab.label(key)),
                "tab {:?} for {:?} not rendered:\n{text}",
                tab,
                key
            );
            // The strip scrolls to keep the selected tab visible; the
            // status bar always ends with the help hint.
            assert!(lines.last().unwrap().ends_with("F1 keys"));
            assert!(lines.iter().all(|l| l.chars().count() <= 80));
        }
    }
    // The send tab lists the client's verbs.
    app.select(UiKey::Client(ClientId::new(3)));
    app.inspector.tab = InspectorTab::Send;
    let text = dump(&frame(&mut app, 100, 30));
    println!("{text}");
    assert!(text.contains("send_command"));
    // The traffic tab lists requests newest first with the peer's port.
    app.select(UiKey::Server(ServerId::new(1)));
    app.inspector.tab = InspectorTab::Traffic;
    let lines = frame(&mut app, 100, 30);
    let text = dump(&lines);
    println!("{text}");
    let first = lines
        .iter()
        .position(|l| l.contains(":53121 http_request → send_http"))
        .expect("a traffic row");
    assert!(
        lines[first].contains("17:13:23"),
        "newest first: {}",
        lines[first]
    );
}

#[test]
fn a_vanished_selection_moves_to_its_neighbour_not_to_nothing() {
    let mut app = app();
    app.absorb_snapshot(populated());
    app.focus = Focus::Instances;
    app.select(UiKey::Server(ServerId::new(1)));
    let mut without_first = populated();
    without_first.servers.remove(0);
    app.absorb_snapshot(without_first);
    assert_eq!(app.selected(), Some(UiKey::Server(ServerId::new(2))));

    // Everything gone: the cursor lands on + new server, and the inspector
    // cannot keep focus on nothing.
    app.focus = Focus::Inspector;
    app.absorb_snapshot(RailSnapshot::default());
    assert_eq!(app.selected(), None);
    assert_eq!(app.focus, Focus::Instances);
    let text = dump(&frame(&mut app, 80, 24));
    assert!(text.contains("Nothing selected yet"));
}

#[test]
fn an_instance_that_just_appeared_becomes_the_selection() {
    let mut app = app();
    app.focus = Focus::Instances;
    app.absorb_snapshot(RailSnapshot::default());
    // The cursor sits on `+ new server`; the server it starts appears.
    assert_eq!(app.selected(), None);
    app.absorb_snapshot(populated());
    assert_eq!(
        app.selected(),
        Some(UiKey::Client(ClientId::new(3))),
        "the newest arrival"
    );

    // While something else is being inspected, an arrival does not steal it.
    app.select(UiKey::Server(ServerId::new(1)));
    let mut more = populated();
    more.clients.push(ClientRow {
        id: ClientId::new(9),
        ..more.clients[0].clone()
    });
    app.absorb_snapshot(more);
    assert_eq!(app.selected(), Some(UiKey::Server(ServerId::new(1))));
}

#[test]
fn the_chat_keeps_its_newest_line_visible_when_older_ones_wrap() {
    let mut app = app();
    app.absorb_snapshot(RailSnapshot::default());
    for i in 0..12 {
        app.push_system(format!(
            "line {i}: a sentence long enough to wrap twice inside a forty column chat pane, easily"
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
fn f2_gives_one_right_pane_the_whole_column() {
    use netget::tui::app::RightLayout;
    let mut app = app();
    app.absorb_snapshot(RailSnapshot::default());
    app.push_system("hello");

    app.right_layout = RightLayout::ChatMax;
    let text = dump(&frame(&mut app, 80, 24));
    assert!(text.contains("CHAT"));
    assert!(
        !text.contains("ACTIVITY"),
        "the feed yields the column:\n{text}"
    );

    app.right_layout = RightLayout::FeedMax;
    let text = dump(&frame(&mut app, 80, 24));
    assert!(text.contains("ACTIVITY"));
    assert!(
        !text.contains("CHAT"),
        "the chat history yields the column:\n{text}"
    );
    assert!(text.contains("> "), "the input box always stays:\n{text}");

    assert_eq!(
        RightLayout::Balanced.next().next().next(),
        RightLayout::Balanced
    );
}

#[test]
fn the_action_menu_lists_verbs_vertically_with_their_letters() {
    use netget::tui::modal::Modal;
    let mut app = app();
    app.absorb_snapshot(populated());
    app.focus = Focus::Instances;
    app.select(UiKey::Server(ServerId::new(1)));
    netget::tui::actions::open_action_menu(&mut app);
    assert!(matches!(app.modals.last(), Some(Modal::ActionMenu(_))));
    let lines = frame(&mut app, 80, 24);
    let text = dump(&lines);
    println!("{text}");
    assert!(
        text.contains("#1 http 127.0.0.1:8080"),
        "titled by the instance"
    );
    let stop = lines
        .iter()
        .find(|l| l.contains("stop server"))
        .expect("stop entry");
    assert!(
        stop.contains(" x "),
        "the letter sits beside the entry: {stop}"
    );
    let edit = lines
        .iter()
        .position(|l| l.contains("edit config"))
        .unwrap();
    let stop_at = lines
        .iter()
        .position(|l| l.contains("stop server"))
        .unwrap();
    assert_eq!(edit, stop_at + 1, "one entry per line, nothing wraps");
    assert!(text.contains("driver: MANUAL → LLM"));

    // A disabled entry is listed with its reason rather than hidden.
    app.modals.clear();
    app.select(UiKey::Client(ClientId::new(3)));
    app.snapshot.clients[0].send_state = netget::tui::projection::SendState::NotConnected;
    netget::tui::actions::open_action_menu(&mut app);
    let text = dump(&frame(&mut app, 100, 30));
    assert!(text.contains("send…  — not connected"), "{text}");
}
