//! The read path: what a `bluetooth_read_request` answers when the handler call *succeeds* but
//! does not hand back a usable value.
//!
//! Three outcomes have to stay apart, because ATT carries only "here is a value" or an error
//! code and cannot express the difference itself:
//!
//! * the handler answered without naming a value (`decision=model_silent`) → serve the
//!   characteristic's stored value. That value is this server's own state (`add_service`'s
//!   `initial_value`, a `send_notification`, or the last write), so serving it is ordinary GATT
//!   behaviour, not an invention.
//! * the handler named no value and nothing is stored
//!   (`decision=fail_closed_model_silent_no_value`) → ATT Unlikely Error. An empty `Success`
//!   would assert that the characteristic holds zero bytes, which nothing here knows.
//! * the handler named a value that will not decode (`decision=fail_closed_bad_value`) → ATT
//!   Unlikely Error. The model *tried* to say what the characteristic holds; quietly serving the
//!   stored value instead would hand the central a different value under `Success`.
//!
//! The stored-value case also guards a fixed defect: that fallback used to acquire the
//! `ServerData` lock with `futures::executor::block_on(server_data.lock())` inside a synchronous
//! `unwrap_or_else` closure — a blocking lock on a tokio worker thread, which can panic ("Cannot
//! block the current thread from within a runtime") into the `tokio::spawn` that swallows it, so
//! the read dies while the server still looks healthy. It shows up here as the responder never
//! answering, caught by the 20s timeout.
//!
//! Everything runs against the real `BluetoothBle::event_loop` via `run_event_loop_without_radio`
//! — no Bluetooth hardware, byte-for-byte the request/response paths a real central drives.

#![cfg(all(test, feature = "bluetooth-ble"))]

use super::super::super::helpers::mock_builder::MockLlmBuilder;
use super::super::super::helpers::mock_ollama::MockOllamaServer;

use ble_peripheral_rust::gatt::peripheral_event::{
    PeripheralEvent, PeripheralRequest, RequestResponse,
};
use netget::llm::ollama_client::OllamaClient;
use netget::server::bluetooth_ble::{read_decision, BluetoothBle, ReadDecision};
use netget::state::app_state::AppState;
use netget::state::ServerId;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

type TestResult = Result<(), Box<dyn std::error::Error>>;

/// Full 128-bit forms, because the read request carries `Uuid::to_string()` (the long form) and
/// the stored-value map is keyed by exactly the string `add_service` was given. Using the long
/// form on both sides is what makes the fallback lookup hit.
const SERVICE: &str = "0000180d-0000-1000-8000-00805f9b34fb";
const CHARACTERISTIC: &str = "00002a37-0000-1000-8000-00805f9b34fb";

/// Start the radio-free event loop against a mock LLM and issue one ATT read.
async fn read_with_actions(
    actions: serde_json::Value,
) -> Result<
    ble_peripheral_rust::gatt::peripheral_event::ReadRequestResponse,
    Box<dyn std::error::Error>,
> {
    read_with_actions_seeded(actions, Vec::new()).await
}

/// As [`read_with_actions`], but with the GATT table already populated.
///
/// The stored-value tests need a characteristic that exists and holds bytes *before* the read.
/// They used to arrange that by returning `add_service` from the read response, which
/// `tests/helpers/mock_action_names.rs` correctly rejects: `add_service` is declared on
/// `bluetooth_ble_started` and nowhere else, so `call_llm` would never offer it on a read and
/// no real model could produce that answer. Seeding the table directly tests the fallback
/// without faking a model response that cannot happen.
async fn read_with_actions_seeded(
    actions: serde_json::Value,
    seed: Vec<(String, Vec<u8>)>,
) -> Result<
    ble_peripheral_rust::gatt::peripheral_event::ReadRequestResponse,
    Box<dyn std::error::Error>,
> {
    let mock_config = MockLlmBuilder::new()
        .on_event("bluetooth_read_request")
        .respond_with_actions(actions)
        .expect_at_least(1)
        .and()
        .build();

    let mock = MockOllamaServer::start(mock_config).await?;

    let (event_tx, event_rx) = mpsc::channel::<PeripheralEvent>(8);
    tokio::spawn(BluetoothBle::run_event_loop_without_radio(
        event_rx,
        ServerId::new(1),
        OllamaClient::new(mock.base_url()),
        Arc::new(AppState::new()),
        mpsc::unbounded_channel::<String>().0,
        seed,
    ));

    let (responder, response) = oneshot::channel();
    event_tx
        .send(PeripheralEvent::ReadRequest {
            request: PeripheralRequest {
                client: "test-central".to_string(),
                service: uuid::Uuid::parse_str(SERVICE)?,
                characteristic: uuid::Uuid::parse_str(CHARACTERISTIC)?,
            },
            offset: 0,
            responder,
        })
        .await?;

    let reply = tokio::time::timeout(Duration::from_secs(20), response)
        .await
        .map_err(|_| {
            "No ATT read response within 20s — the read path never answered (a swallowed panic \
             in the former block_on would look exactly like this)"
        })??;

    mock.verify_calls().await?;
    Ok(reply)
}

/// One action list that defines the characteristic (so something *is* stored) and nothing else:
/// the handler declines to name a read value.
/// The GATT table the stored-value tests need: one characteristic holding `initial_value`.
fn seeded_with(initial_value: &[u8]) -> Vec<(String, Vec<u8>)> {
    vec![(CHARACTERISTIC.to_string(), initial_value.to_vec())]
}

#[tokio::test]
async fn read_falls_back_to_stored_value_without_blocking() -> TestResult {
    // The handler defines the characteristic with a stored value but produces no
    // `respond_to_read`, so the read must take the async stored-value fallback and answer
    // Success with exactly those bytes.
    let reply = read_with_actions_seeded(serde_json::json!([]), seeded_with(&[0x00, 0x48])).await?;

    assert_eq!(
        reply.response,
        RequestResponse::Success,
        "the handler succeeded and the characteristic has a stored value, so the read must \
         answer Success via the async fallback"
    );
    assert_eq!(
        reply.value,
        vec![0x00, 0x48],
        "the fallback must serve the characteristic's stored value verbatim"
    );
    Ok(())
}

#[tokio::test]
async fn read_with_no_answer_and_no_stored_value_fails_closed() -> TestResult {
    // Empty action list and an unknown characteristic: nothing was said and nothing is stored.
    // This used to answer Success with an empty value — an assertion that the characteristic
    // holds zero bytes, which no part of the server is in a position to make.
    let reply = read_with_actions(serde_json::json!([])).await?;

    assert_eq!(
        reply.response,
        RequestResponse::UnlikelyError,
        "handler silence with nothing stored must fail closed (0x0E), not synthesise an empty \
         Success"
    );
    assert!(
        reply.value.is_empty(),
        "a failed read carries no value, got {:?}",
        reply.value
    );
    Ok(())
}

#[tokio::test]
async fn read_with_undecodable_value_fails_closed_without_substituting_stored() -> TestResult {
    // The characteristic *does* have a stored value, and the handler *does* answer — with a
    // value that is not hex. Serving the stored bytes here would hand the central a value the
    // handler never chose, under Success.
    let actions = serde_json::json!([{
        "type": "respond_to_read",
        "characteristic_uuid": CHARACTERISTIC,
        "value": "ninety-nine"
    }]);

    let reply = read_with_actions_seeded(actions, seeded_with(&[0x00, 0x48])).await?;

    assert_eq!(
        reply.response,
        RequestResponse::UnlikelyError,
        "an undecodable respond_to_read must fail closed, not degrade into the stored value"
    );
    assert!(
        reply.value.is_empty(),
        "a failed read carries no value, got {:?}",
        reply.value
    );
    Ok(())
}

#[test]
fn read_decision_keeps_the_three_outcomes_apart() {
    assert_eq!(read_decision(&[]), ReadDecision::UseStored);

    assert_eq!(
        read_decision(&[serde_json::json!({"type": "send_notification", "value": "00"})]),
        ReadDecision::UseStored,
        "an unrelated action is not a read answer"
    );

    assert_eq!(
        read_decision(&[serde_json::json!({"type": "respond_to_read", "value": "0x0048"})]),
        ReadDecision::Value(vec![0x00, 0x48]),
        "the 0x prefix is optional, as it is everywhere else in this protocol"
    );

    assert_eq!(
        read_decision(&[serde_json::json!({"type": "send_read_response", "value": "48"})]),
        ReadDecision::Value(vec![0x48]),
        "the send_read_response alias the event loop accepts must decide identically"
    );

    assert!(matches!(
        read_decision(&[serde_json::json!({"type": "respond_to_read", "value": "zz"})]),
        ReadDecision::Unusable(_)
    ));

    assert!(
        matches!(
            read_decision(&[serde_json::json!({"type": "respond_to_read"})]),
            ReadDecision::Unusable(_)
        ),
        "a respond_to_read with no value is an attempted answer that cannot be served, not \
         silence — silence would quietly serve the stored value"
    );
}

#[test]
fn undecodable_initial_value_is_refused_rather_than_stored_as_empty() {
    // `add_service` with a non-hex `initial_value` must fail rather than register the
    // characteristic with zero bytes. `unwrap_or_default()` here used to store nothing, and the
    // read path then served those zero bytes under Success on decision=model_silent — an
    // assertion that the characteristic holds nothing, made on the strength of a value nobody
    // could read.
    //
    // This is a direct call rather than a driven read, and deliberately so: `add_service` is
    // declared only on `bluetooth_ble_started`, which `run_event_loop_without_radio` never
    // raises. The previous version of this test reached it by returning `add_service` from a
    // *read* response, which no real model could do — `call_llm` offers only the firing event's
    // actions — and `tests/helpers/mock_action_names.rs` rightly rejects that mock.
    let err = ::netget::server::bluetooth_ble::parse_initial_value(
        CHARACTERISTIC,
        &serde_json::json!("not-hex"),
    )
    .expect_err("a non-hex initial_value must be refused, not stored as zero bytes");
    let message = format!("{err:#}");
    assert!(
        message.contains(CHARACTERISTIC) && message.contains("not-hex"),
        "the error must name the characteristic and the offending value so the model can fix \
         it, got {message:?}"
    );

    // Absent is not the same as malformed: a characteristic may legitimately start empty.
    assert_eq!(
        ::netget::server::bluetooth_ble::parse_initial_value(
            CHARACTERISTIC,
            &serde_json::Value::Null
        )
        .expect("an absent initial_value is legal"),
        Vec::<u8>::new()
    );

    // And a well-formed value still decodes, with or without the 0x prefix.
    assert_eq!(
        ::netget::server::bluetooth_ble::parse_initial_value(
            CHARACTERISTIC,
            &serde_json::json!("0x0048")
        )
        .expect("0x-prefixed hex is legal"),
        vec![0x00, 0x48]
    );
}
