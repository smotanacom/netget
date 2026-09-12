//! The dashboard's management surface, model-side: the instance list, the
//! driver badge and its rebuild, the inspector's tabs, items and action bar,
//! and the throughput arithmetic behind the sparklines.

#![cfg(feature = "tcp")]

use netget::scripting::event_handler::EventPattern;
use netget::scripting::{EventHandler, EventHandlerConfig, EventHandlerType};
use netget::state::app_state::AccessLogEntry;
use netget::state::client::ClientStatus;
use netget::state::intercepts::{InterceptOwner, InterceptView};
use netget::state::server::ServerStatus;
use netget::state::{ClientId, ServerId};
use netget::tui::app::{InspectorUi, InstanceRef, Section, TrafficFilter, UiKey};
use netget::tui::driver::{driver_of, handlers_with_driver, specific_rule_count, Driver};
use netget::tui::inspector::{self, InspectorTab, InstanceAction, Item};
use netget::tui::metrics::{human_bytes, human_duration, Throughput};
use netget::tui::projection::{ClientRow, ConnRow, RailSnapshot, SendState, SendVerb, ServerRow};
use netget::tui::rail::{self, fit, is_selectable, list_rows, ListRow};

fn entry(id: u64, conn: Option<u32>) -> AccessLogEntry {
    AccessLogEntry {
        id,
        unix_ms: 1_700_000_000_000 + id,
        server_id: Some(1),
        client_id: None,
        protocol: "TCP".into(),
        connection_id: conn,
        event_type: "tcp_data_received".into(),
        request: serde_json::json!({"data": format!("payload-{id}")}),
        response: vec![serde_json::json!({"type": "send_tcp_data", "data": "pong"})],
    }
}

fn conn(id: u32, active: bool, can_message: bool) -> ConnRow {
    ConnRow {
        id,
        remote_addr: format!("127.0.0.1:{}", 40000 + id),
        bytes_received: 10,
        bytes_sent: 20,
        active,
        can_message,
    }
}

fn wildcard(handler: EventHandlerType) -> EventHandlerConfig {
    EventHandlerConfig {
        handlers: vec![EventHandler {
            event_pattern: EventPattern::Wildcard,
            handler,
        }],
    }
}

fn specific(id: &str) -> EventHandler {
    EventHandler {
        event_pattern: EventPattern::Specific(id.to_string()),
        handler: EventHandlerType::Static {
            actions: vec![serde_json::json!({"type": "send_tcp_data", "data": "hi"})],
        },
    }
}

fn server(conns: Vec<ConnRow>, requests: Vec<AccessLogEntry>) -> ServerRow {
    ServerRow {
        id: ServerId::new(1),
        protocol: "TCP".into(),
        port: 8080,
        local_addr: Some("127.0.0.1:8080".into()),
        status: ServerStatus::Running,
        instruction: "be a server".into(),
        memory_len: 0,
        startup_params: Some(serde_json::json!({"send_first": false})),
        routing: Some(wildcard(EventHandlerType::Manual { timeout_secs: 300 })),
        conns,
        recent: Vec::new(),
        requests,
        task_count: 0,
        uptime_secs: 133,
        client_counterpart: Some("TCP".into()),
        intercepts: Vec::new(),
    }
}

fn client(status: ClientStatus, send_state: SendState) -> ClientRow {
    ClientRow {
        id: ClientId::new(4),
        protocol: "telnet".into(),
        remote_addr: "127.0.0.1:2323".into(),
        status,
        instruction: String::new(),
        memory_len: 0,
        startup_params: None,
        routing: None,
        connection: None,
        history: Vec::new(),
        requests: Vec::new(),
        task_count: 0,
        uptime_secs: 5,
        send_state,
        send_actions: vec![
            SendVerb {
                name: "send_command".into(),
                description: "Send a command line".into(),
            },
            SendVerb {
                name: "send_text".into(),
                description: "Send raw text".into(),
            },
        ],
        intercepts: Vec::new(),
    }
}

fn snapshot(servers: Vec<ServerRow>, clients: Vec<ClientRow>) -> RailSnapshot {
    RailSnapshot {
        servers,
        clients,
        ..Default::default()
    }
}

// ---------------------------------------------------------------- the list

#[test]
fn the_list_is_sections_instances_and_one_new_row() {
    let snap = snapshot(
        vec![server(Vec::new(), Vec::new())],
        vec![client(ClientStatus::Connected, SendState::Ready)],
    );
    let rows = list_rows(&snap);
    assert_eq!(
        rows,
        vec![
            ListRow::Header(Section::Servers, 1),
            ListRow::Instance(UiKey::Server(ServerId::new(1))),
            ListRow::Header(Section::Clients, 1),
            ListRow::Instance(UiKey::Client(ClientId::new(4))),
            ListRow::New,
        ]
    );
    assert!(!is_selectable(&rows[0]));
    assert!(is_selectable(&rows[1]));
    assert!(is_selectable(&rows[4]), "the + new row is a cursor stop");
}

#[test]
fn an_empty_list_still_has_the_new_row() {
    let rows = list_rows(&RailSnapshot::default());
    assert_eq!(rows.iter().filter(|r| is_selectable(r)).count(), 1);
}

#[test]
fn a_server_line_carries_status_port_peers_and_driver() {
    let line = rail::server_line(&server(
        vec![conn(1, true, false), conn(2, false, false)],
        Vec::new(),
    ));
    assert_eq!(line.glyph, "●");
    assert_eq!(line.protocol, "tcp");
    assert_eq!(line.target, ":8080");
    assert_eq!(line.peers, "1⇄", "only live peers count");
    assert_eq!(line.driver, Driver::Manual);
    assert_eq!(line.error, None);

    let mut broken = server(Vec::new(), Vec::new());
    broken.status = ServerStatus::Error("bind failed".into());
    let line = rail::server_line(&broken);
    assert_eq!(line.glyph, "✗");
    assert_eq!(line.error.as_deref(), Some("bind failed"));
}

#[test]
fn a_client_line_points_at_its_remote_and_defaults_to_the_model() {
    let line = rail::client_line(&client(ClientStatus::Disconnected, SendState::NotConnected));
    assert_eq!(line.glyph, "○");
    assert_eq!(line.target, "→127.0.0.1:2323");
    assert!(line.peers.is_empty());
    assert_eq!(line.driver, Driver::Llm, "no rules at all means the model");
}

#[test]
fn fit_truncates_with_an_ellipsis_by_chars() {
    assert_eq!(fit("hello", 10), "hello");
    assert_eq!(fit("hello world", 5), "hell…");
    assert_eq!(fit("héllo wörld", 6), "héllo…");
    assert_eq!(fit("x", 0), "");
}

// -------------------------------------------------------------- the driver

#[test]
fn the_driver_is_read_off_the_wildcard_rule() {
    assert_eq!(driver_of(None), Driver::Llm);
    assert_eq!(
        driver_of(Some(&wildcard(EventHandlerType::Manual {
            timeout_secs: 1
        }))),
        Driver::Manual
    );
    assert_eq!(
        driver_of(Some(&wildcard(EventHandlerType::Static {
            actions: Vec::new()
        }))),
        Driver::Silent
    );
    assert_eq!(
        driver_of(Some(&wildcard(EventHandlerType::Static {
            actions: vec![serde_json::json!({"type": "x"})]
        }))),
        Driver::Rules
    );
    assert_eq!(
        driver_of(Some(&wildcard(EventHandlerType::Llm {
            instruction: "answer".into()
        }))),
        Driver::Llm
    );
    // A specific rule alone is not a driver: the fallback is still the model.
    let only_specific = EventHandlerConfig {
        handlers: vec![specific("tcp_connection_opened")],
    };
    assert_eq!(driver_of(Some(&only_specific)), Driver::Llm);
    assert_eq!(specific_rule_count(Some(&only_specific)), 1);
}

#[test]
fn cycling_the_driver_rewrites_only_the_wildcard_and_keeps_it_last() {
    let mut config = wildcard(EventHandlerType::Manual { timeout_secs: 300 });
    config.handlers.insert(0, specific("tcp_connection_opened"));

    // MANUAL → LLM: the wildcard goes away, the specific rule stays.
    let llm = handlers_with_driver(Some(&config), Driver::Llm);
    assert_eq!(llm.len(), 1);
    assert_eq!(llm[0]["event_pattern"], "tcp_connection_opened");

    // → SILENT: a static rule with no actions, after the specific one.
    let silent = handlers_with_driver(Some(&config), Driver::Silent);
    assert_eq!(silent.len(), 2);
    assert_eq!(silent[1]["event_pattern"], "*");
    assert_eq!(silent[1]["handler"]["type"], "static");
    assert_eq!(silent[1]["handler"]["actions"], serde_json::json!([]));

    // → MANUAL again.
    let manual = handlers_with_driver(Some(&config), Driver::Manual);
    assert_eq!(manual[1]["handler"]["type"], "manual");

    // Every rebuilt table parses back as handlers the dispatcher accepts.
    for table in [&llm, &silent, &manual] {
        for rule in table {
            let parsed: EventHandler =
                serde_json::from_value(rule.clone()).expect("rebuilt rule must be a valid handler");
            let _ = parsed;
        }
    }

    // The cycle order itself.
    assert_eq!(Driver::Manual.next(), Driver::Llm);
    assert_eq!(Driver::Llm.next(), Driver::Silent);
    assert_eq!(Driver::Silent.next(), Driver::Manual);
    assert_eq!(Driver::Rules.next(), Driver::Manual);
}

// ----------------------------------------------------------- the inspector

#[test]
fn a_server_offers_five_tabs_and_a_client_six() {
    assert_eq!(
        InspectorTab::for_key(UiKey::Server(ServerId::new(1))),
        vec![
            InspectorTab::Overview,
            InspectorTab::Peers,
            InspectorTab::Traffic,
            InspectorTab::Rules,
            InspectorTab::Config
        ]
    );
    assert!(InspectorTab::for_key(UiKey::Client(ClientId::new(1))).contains(&InspectorTab::Send));
    // A tab the instance lacks resolves to the overview rather than an
    // empty pane.
    assert_eq!(
        inspector::effective_tab(UiKey::Server(ServerId::new(1)), InspectorTab::Send),
        InspectorTab::Overview
    );
}

#[test]
fn the_overview_leads_with_what_is_waiting_for_you() {
    let mut row = server(vec![conn(1, true, true)], Vec::new());
    row.intercepts.push(InterceptView {
        id: 9,
        owner: InterceptOwner::Server(row.id),
        connection_id: Some(1),
        event_type: "tcp_data_received".into(),
        description: "data".into(),
        event_data: None,
        created_unix_ms: 0,
    });
    let ui = InspectorUi::default();
    let view = inspector::build(InstanceRef::Server(&row), &ui, None, 60);
    assert_eq!(view.item_at(0), Some(&Item::Intercept(9)));
    assert_eq!(view.default_action(0), Some(InstanceAction::Answer(9)));
    let text: Vec<String> = view.lines.iter().map(|l| l.text()).collect();
    assert!(text[0].contains("YOUR answer needed"));
    assert!(
        text[0].contains("from :40001"),
        "names the peer by port: {}",
        text[0]
    );
    assert!(text
        .iter()
        .any(|t| t.starts_with("status") && t.contains("Running")));
    assert!(text
        .iter()
        .any(|t| t.starts_with("driver") && t.contains("MANUAL")));
    assert!(
        text.iter().any(|t| t.contains("2m13s")),
        "uptime is shown: {text:?}"
    );

    // The menu: lifecycle, edit, rules, the driver, the counterpart client.
    let actions: Vec<InstanceAction> = view.actions.iter().map(|b| b.action).collect();
    assert_eq!(
        actions[0],
        InstanceAction::Answer(9),
        "what acts on the selected item comes first"
    );
    assert_eq!(actions[1], InstanceAction::Stop, "then the instance's own");
    assert!(actions.contains(&InstanceAction::ConnectClient));
    assert!(actions.contains(&InstanceAction::CycleDriver));
    assert!(view
        .actions
        .iter()
        .any(|b| b.action == InstanceAction::CycleDriver && b.label == "driver: MANUAL → LLM"));
}

#[test]
fn the_peers_tab_lists_live_then_closed_and_arms_the_bar_for_a_live_peer() {
    let row = server(
        vec![
            conn(1, true, true),
            conn(2, true, false),
            conn(3, false, false),
        ],
        vec![entry(1, Some(1)), entry(2, Some(1)), entry(3, None)],
    );
    let ui = InspectorUi {
        tab: InspectorTab::Peers,
        ..Default::default()
    };
    let view = inspector::build(InstanceRef::Server(&row), &ui, None, 60);
    let items: Vec<&Item> = (0..view.item_count())
        .filter_map(|n| view.item_at(n))
        .collect();
    assert_eq!(
        items,
        vec![
            &Item::Peer(Some(1)),
            &Item::Peer(Some(2)),
            &Item::Peer(Some(3)),
            &Item::Peer(None),
        ],
        "live peers, closed peers, then the connectionless bucket"
    );
    assert!(
        view.lines[0].text().contains("2 req"),
        "{}",
        view.lines[0].text()
    );

    // Selected: a live peer with a handle → message and disconnect enabled.
    let message = view
        .actions
        .iter()
        .find(|b| b.action == InstanceAction::MessagePeer(1))
        .expect("message button");
    assert!(message.enabled);
    assert_eq!(
        view.default_action(0),
        Some(InstanceAction::FilterTraffic(Some(1)))
    );

    // A live peer without a handle keeps the buttons, disabled, with the reason.
    let ui = InspectorUi {
        tab: InspectorTab::Peers,
        item: 1,
        ..Default::default()
    };
    let view = inspector::build(InstanceRef::Server(&row), &ui, None, 60);
    let message = view
        .actions
        .iter()
        .find(|b| b.action == InstanceAction::MessagePeer(2))
        .expect("message entry");
    assert!(!message.enabled);
    assert!(message
        .why_disabled
        .as_deref()
        .unwrap_or("")
        .contains("cannot message"));
}

#[test]
fn the_traffic_tab_is_newest_first_and_narrows_to_a_peer() {
    let row = server(
        vec![conn(1, true, false), conn(2, true, false)],
        vec![entry(1, Some(1)), entry(2, Some(2)), entry(3, Some(1))],
    );
    let ui = InspectorUi {
        tab: InspectorTab::Traffic,
        ..Default::default()
    };
    let view = inspector::build(InstanceRef::Server(&row), &ui, None, 80);
    let items: Vec<&Item> = (0..view.item_count())
        .filter_map(|n| view.item_at(n))
        .collect();
    assert_eq!(
        items,
        vec![&Item::Request(3), &Item::Request(2), &Item::Request(1)]
    );
    assert_eq!(view.default_action(0), Some(InstanceAction::OpenRequest(3)));
    assert!(
        view.lines[0].text().contains(":40001"),
        "{}",
        view.lines[0].text()
    );
    assert!(view.lines[0]
        .text()
        .contains("tcp_data_received → send_tcp_data"));
    assert!(!view
        .actions
        .iter()
        .any(|b| b.action == InstanceAction::ClearTrafficFilter));

    let ui = InspectorUi {
        tab: InspectorTab::Traffic,
        filter: TrafficFilter::Peer(Some(2)),
        ..Default::default()
    };
    let view = inspector::build(InstanceRef::Server(&row), &ui, None, 80);
    assert_eq!(view.item_count(), 1);
    assert_eq!(view.item_at(0), Some(&Item::Request(2)));
    assert!(view.lines[0].text().contains("only peer 127.0.0.1:40002"));
    assert!(view
        .actions
        .iter()
        .any(|b| b.action == InstanceAction::ClearTrafficFilter && b.enabled));
}

#[test]
fn the_rules_tab_lists_rules_in_match_order_with_their_kind() {
    let mut row = server(Vec::new(), Vec::new());
    let mut config = wildcard(EventHandlerType::Manual { timeout_secs: 300 });
    config.handlers.insert(0, specific("tcp_connection_opened"));
    row.routing = Some(config);
    let ui = InspectorUi {
        tab: InspectorTab::Rules,
        item: 1,
        ..Default::default()
    };
    let view = inspector::build(InstanceRef::Server(&row), &ui, None, 80);
    assert_eq!(view.item_count(), 2);
    assert_eq!(view.item_at(0), Some(&Item::Rule(0)));
    let texts: Vec<String> = view
        .lines
        .iter()
        .filter(|l| l.item.is_some())
        .map(|l| l.text())
        .collect();
    assert!(
        texts[0].contains("tcp_connection_opened → STATIC"),
        "{}",
        texts[0]
    );
    assert!(texts[1].contains("* → MANUAL"), "{}", texts[1]);
    assert_eq!(view.default_action(1), Some(InstanceAction::EditRule(1)));
    let actions: Vec<InstanceAction> = view
        .actions
        .iter()
        .filter(|b| b.enabled)
        .map(|b| b.action)
        .collect();
    assert!(actions.contains(&InstanceAction::AddRule));
    assert!(actions.contains(&InstanceAction::DeleteRule(1)));
    assert!(actions.contains(&InstanceAction::MoveRule(1, -1)));
    // With a wildcard there is no "otherwise → LLM" note.
    assert!(!view.lines.iter().any(|l| l.text().contains("otherwise")));

    row.routing = None;
    let view = inspector::build(InstanceRef::Server(&row), &ui, None, 80);
    assert_eq!(view.item_count(), 0);
    assert!(view
        .lines
        .iter()
        .any(|l| l.text().contains("otherwise → LLM")));
}

#[test]
fn the_config_tab_shows_every_setting_and_enter_opens_the_form() {
    let row = server(Vec::new(), Vec::new());
    let ui = InspectorUi {
        tab: InspectorTab::Config,
        ..Default::default()
    };
    let view = inspector::build(InstanceRef::Server(&row), &ui, None, 80);
    let names: Vec<String> = (0..view.item_count())
        .filter_map(|n| match view.item_at(n) {
            Some(Item::Config(name)) => Some(name.clone()),
            _ => None,
        })
        .collect();
    assert_eq!(
        names,
        vec![
            "protocol",
            "port",
            "host",
            "send_first",
            "instruction",
            "memory"
        ]
    );
    assert_eq!(view.default_action(1), Some(InstanceAction::Edit));
    let port_line = view
        .lines
        .iter()
        .find(|l| l.item == Some(Item::Config("port".into())))
        .unwrap();
    assert!(port_line.text().contains("8080"));
}

#[test]
fn a_client_offers_its_verbs_on_the_send_tab_and_connect_when_down() {
    let ready = client(ClientStatus::Connected, SendState::Ready);
    let ui = InspectorUi {
        tab: InspectorTab::Send,
        item: 1,
        ..Default::default()
    };
    let view = inspector::build(InstanceRef::Client(&ready), &ui, None, 80);
    assert_eq!(view.item_count(), 2);
    assert_eq!(view.item_at(1), Some(&Item::SendAction(1)));
    assert_eq!(view.default_action(1), Some(InstanceAction::SendAction(1)));
    assert!(view.lines[1].text().contains("send_text"));
    assert!(view.lines[1].text().contains("Send raw text"));
    assert!(view
        .actions
        .iter()
        .any(|b| b.action == InstanceAction::SendAction(1) && b.enabled));

    // Disconnected: the overview leads with connect, and send is disabled
    // with a reason rather than missing.
    let down = client(ClientStatus::Disconnected, SendState::NotConnected);
    let ui = InspectorUi::default();
    let view = inspector::build(InstanceRef::Client(&down), &ui, None, 80);
    assert_eq!(view.actions[0].action, InstanceAction::Connect);
    let send = view
        .actions
        .iter()
        .find(|b| b.action == InstanceAction::Send)
        .unwrap();
    assert!(!send.enabled);
    assert_eq!(send.why_disabled.as_deref(), Some("not connected"));

    // A protocol whose loop has no command channel says so.
    let stuck = client(ClientStatus::Connected, SendState::ProtocolUnsupported);
    let view = inspector::build(InstanceRef::Client(&stuck), &ui, None, 80);
    assert_eq!(view.actions[0].action, InstanceAction::Disconnect);
    let send = view
        .actions
        .iter()
        .find(|b| b.action == InstanceAction::Send)
        .unwrap();
    assert!(send
        .why_disabled
        .as_deref()
        .unwrap_or("")
        .contains("command channel"));
}

// -------------------------------------------------------------- the metrics

#[test]
fn throughput_samples_are_deltas_and_the_sparkline_scales_to_the_peak() {
    let mut t = Throughput::default();
    assert!(t.is_idle());
    t.sample(0, 0);
    t.sample(100, 50); // +100 / +50
    t.sample(100, 50); // idle second
    t.sample(400, 50); // +300
    assert_eq!(t.rate(), (300, 0));
    assert!(!t.is_idle());
    let spark = t.sparkline(4);
    assert_eq!(spark.chars().count(), 4);
    let glyphs: Vec<char> = spark.chars().collect();
    assert_eq!(glyphs[0], ' ', "a missing sample is blank, not zero");
    assert_eq!(glyphs[2], '▁', "an idle second is the floor");
    assert_eq!(glyphs[3], '█', "the peak is the top");
    assert!(
        glyphs[1] > '▁' && glyphs[1] < '█',
        "a smaller sample sits between"
    );

    // Totals that go backwards (a connection reaped) never underflow.
    t.sample(10, 10);
    assert_eq!(t.rate(), (0, 0));
}

#[test]
fn byte_and_duration_formatting_is_compact() {
    assert_eq!(human_bytes(0), "0");
    assert_eq!(human_bytes(999), "999");
    assert_eq!(human_bytes(1_200), "1.2K");
    assert_eq!(human_bytes(123_456), "123K");
    assert_eq!(human_bytes(4_500_000), "4.5M");
    assert_eq!(human_duration(12), "12s");
    assert_eq!(human_duration(133), "2m13s");
    assert_eq!(human_duration(3_840), "1h04m");
}
