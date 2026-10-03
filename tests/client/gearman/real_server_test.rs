use super::common::*;
use crate::helpers::real_server::{find_binary, InstallHint, RealServer};
use serde_json::json;
use std::time::Duration;
use tokio::process::Command;
async fn daemon() -> crate::helpers::E2EResult<RealServer> {
    RealServer::builder(
        "gearmand",
        InstallHint {
            brew: "gearman",
            apt: "gearman-job-server",
        },
    )
    .args([
        "-L",
        "127.0.0.1",
        "-p",
        "{port}",
        "--log-file=stderr",
        "--pid-file={dir}/gearmand.pid",
        "--threads=1",
        "--job-handle-prefix=H:test",
        "--verbose=DEBUG",
    ])
    .start()
    .await
}
fn cli(server: &RealServer) -> Command {
    let mut cmd = Command::new(
        find_binary("gearman")
            .expect("gearman CLI required: brew install gearman / apt install gearman-tools"),
    );
    cmd.args([
        "-h",
        "127.0.0.1",
        "-p",
        server.addr().rsplit(':').next().unwrap(),
    ])
    .kill_on_drop(true);
    cmd
}
#[tokio::test]
async fn independent_gearmand_and_cli_worker_submitter_priorities_background_status_and_echo(
) -> crate::helpers::E2EResult<()> {
    let server = daemon().await?;
    let mut worker = cli(&server);
    worker.args([
        "-w",
        "-f",
        "reverse",
        "-c",
        "4",
        "-n",
        // GNU getopt otherwise consumes Python's -c as Gearman's job count.
        "--",
        "python3",
        "-c",
        "import sys; sys.stdout.write('piece:\\n'+sys.stdin.read()[::-1])",
    ]);
    let worker_task = tokio::spawn(async move {
        tokio::time::timeout(Duration::from_secs(30), worker.output())
            .await
            .unwrap()
            .unwrap()
    });
    let state = state();
    let id = connected_client(&state, server.addr(), "submitter").await;
    assert_eq!(
        request(
            &state,
            id,
            json!({"operation":"echo","data":"echo ✓\u{0000}tail"})
        )
        .await["payload"]["text"],
        "echo ✓\u{0000}tail"
    );
    assert_eq!(
        request(&state, id, json!({"operation":"enable_exceptions"})).await["option"],
        "exceptions"
    );
    for (n, priority) in ["normal", "high", "low"].into_iter().enumerate() {
        let after = latest(&state, id).await;
        let job=request(&state,id,json!({"operation":"submit","function_name":"reverse","unique_id":format!("case{n}"),"workload":"hello ✓","priority":priority})).await;
        assert!(job["job_handle"].as_str().unwrap().starts_with("H:test:"));
        let mut cursor = after;
        let mut output = String::new();
        loop {
            let (entry, update) = event(&state, id, "gearman_job_update", cursor).await;
            cursor = entry;
            assert_eq!(update["job_handle"], job["job_handle"]);
            assert_eq!(update["request"]["priority"], priority);
            if let Some(text) = update["payload"]["text"].as_str() {
                output.push_str(text);
            }
            if update["terminal"] == true {
                assert_eq!(update["kind"], "complete");
                break;
            }
        }
        assert!(
            output.contains("piece:") && output.contains("✓ olleh"),
            "{output:?}"
        );
        let status = request(
            &state,
            id,
            json!({"operation":"status","job_handle":job["job_handle"]}),
        )
        .await;
        assert_eq!(status["known"], false);
        assert_eq!(status["running"], false);
    }
    assert_eq!(request(&state,id,json!({"operation":"submit","function_name":"reverse","workload":"background","priority":"low","background":true})).await["background"],true);
    let output = worker_task.await?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    state.remove_client(id).await;
    Ok(())
}
#[tokio::test]
async fn independent_cli_producer_observes_selected_worker_progress_data_complete_fail_and_exception(
) -> crate::helpers::E2EResult<()> {
    let server = daemon().await?;
    let state = state();
    let id = connected_client(&state, server.addr(), "worker").await;
    send(
        &state,
        id,
        json!({"type":"gearman_worker","operation":"set_id","client_id":"netget-worker"}),
    )
    .await;
    send(
        &state,
        id,
        json!({"type":"gearman_worker","operation":"register","function_name":"selected"}),
    )
    .await;
    for (n, outcome) in ["complete", "fail", "exception"].into_iter().enumerate() {
        assert_eq!(
            request(
                &state,
                id,
                json!({"type":"gearman_worker","operation":"grab_unique"})
            )
            .await["kind"],
            "no_job"
        );
        let after = latest(&state, id).await;
        send(
            &state,
            id,
            json!({"type":"gearman_worker","operation":"sleep"}),
        )
        .await;
        let mut producer = cli(&server);
        producer.args(["-f", "selected", "-u", &format!("job{n}"), "work ✓"]);
        let producer_task = tokio::spawn(async move {
            tokio::time::timeout(Duration::from_secs(10), producer.output()).await
        });
        event(&state, id, "gearman_worker_wakeup", after).await;
        let job = request(
            &state,
            id,
            json!({"type":"gearman_worker","operation":if n==0{"grab"}else{"grab_unique"}}),
        )
        .await;
        assert_eq!(job["kind"], "job_assigned");
        assert_eq!(job["function"], "selected");
        assert_eq!(job["workload"]["text"], "work ✓");
        if n > 0 {
            assert_eq!(job["unique_id"], format!("job{n}"));
        }
        let handle = job["job_handle"].clone();
        if outcome == "complete" {
            send(&state,id,json!({"type":"gearman_worker","operation":"progress","job_handle":handle,"numerator":1,"denominator":2})).await;
            send(&state,id,json!({"type":"gearman_worker","operation":"data","job_handle":handle,"data":"partial:"})).await;
            send(&state,id,json!({"type":"gearman_worker","operation":"warning","job_handle":handle,"data":"caution"})).await;
        }
        send(&state,id,json!({"type":"gearman_worker","operation":outcome,"job_handle":handle,"result":"result ✓","text":"failed deliberately"})).await;
        let output = producer_task.await?.unwrap_or_else(|_| {
            panic!(
                "CLI producer timed out for {outcome}; daemon log: {}",
                server.log()
            )
        })?;
        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        if outcome == "complete" {
            assert!(output.status.success(), "{stdout}{stderr}");
            assert!(
                stdout.contains("partial:")
                    && stdout.contains("result ✓")
                    && stdout.contains("50% Complete"),
                "{stdout}{stderr}"
            );
        } else {
            assert!(!output.status.success(), "{outcome} {stdout}{stderr}");
            assert!(
                format!("{stdout}{stderr}").contains("Job failed"),
                "{stdout}{stderr}"
            );
        }
        rejected(
            &state,
            id,
            json!({"type":"gearman_worker","operation":"fail","job_handle":handle}),
            "assigned",
        )
        .await;
    }
    send(
        &state,
        id,
        json!({"type":"gearman_worker","operation":"unregister","function_name":"selected"}),
    )
    .await;
    rejected(
        &state,
        id,
        json!({"type":"gearman_worker","operation":"grab"}),
        "Register an ability",
    )
    .await;
    send(
        &state,
        id,
        json!({"type":"gearman_worker","operation":"reset"}),
    )
    .await;
    state.remove_client(id).await;
    Ok(())
}
#[tokio::test]
async fn independent_daemon_exception_option_preserves_typed_terminal_exception(
) -> crate::helpers::E2EResult<()> {
    let server = daemon().await?;
    let state = state();
    let submitter = connected_client(&state, server.addr(), "submitter").await;
    let worker = connected_client(&state, server.addr(), "worker").await;
    request(&state, submitter, json!({"operation":"enable_exceptions"})).await;
    send(
        &state,
        worker,
        json!({"type":"gearman_worker","operation":"register","function_name":"explode"}),
    )
    .await;
    let job = request(
        &state,
        submitter,
        json!({"operation":"submit","function_name":"explode","workload":"oops"}),
    )
    .await;
    let assigned = request(
        &state,
        worker,
        json!({"type":"gearman_worker","operation":"grab_unique"}),
    )
    .await;
    assert_eq!(assigned["job_handle"], job["job_handle"]);
    send(&state,worker,json!({"type":"gearman_worker","operation":"progress","job_handle":assigned["job_handle"],"numerator":1,"denominator":2})).await;
    let (progress_id, progress) = event(&state, submitter, "gearman_job_update", 0).await;
    assert_eq!(progress["kind"], "progress");
    assert_eq!(progress["numerator"], 1);
    assert_eq!(progress["denominator"], 2);
    let status = request(
        &state,
        submitter,
        json!({"operation":"status","job_handle":assigned["job_handle"]}),
    )
    .await;
    assert_eq!(status["known"], true);
    assert_eq!(status["running"], true);
    assert_eq!(status["numerator"], 1);
    assert_eq!(status["denominator"], 2);
    send(&state,worker,json!({"type":"gearman_worker","operation":"data","job_handle":assigned["job_handle"],"data":"typed data ✓"})).await;
    let (data_id, data) = event(&state, submitter, "gearman_job_update", progress_id).await;
    assert_eq!(data["kind"], "data");
    assert_eq!(data["payload"]["text"], "typed data ✓");
    send(&state,worker,json!({"type":"gearman_worker","operation":"warning","job_handle":assigned["job_handle"],"data":"typed warning ✓"})).await;
    let (warning_id, warning) = event(&state, submitter, "gearman_job_update", data_id).await;
    assert_eq!(warning["kind"], "warning");
    assert_eq!(warning["payload"]["text"], "typed warning ✓");
    send(&state,worker,json!({"type":"gearman_worker","operation":"exception","job_handle":assigned["job_handle"],"text":"typed exception ✓"})).await;
    let (_, update) = event(&state, submitter, "gearman_job_update", warning_id).await;
    assert_eq!(update["kind"], "exception");
    assert_eq!(update["terminal"], true);
    assert_eq!(update["payload"]["text"], "typed exception ✓");
    state.remove_client(submitter).await;
    state.remove_client(worker).await;
    Ok(())
}

#[tokio::test]
async fn mocked_model_submits_to_independent_daemon_and_shared_memory_reaches_followup(
) -> crate::helpers::E2EResult<()> {
    use crate::helpers::{mock_builder::MockLlmBuilder, mock_ollama::MockOllamaServer};
    let server = daemon().await?;
    let mut worker = cli(&server);
    worker.args([
        "-w",
        "-f",
        "model-job",
        "-c",
        "1",
        "--",
        "python3",
        "-c",
        "import sys; sys.stdout.write(sys.stdin.read().upper())",
    ]);
    let worker_task = tokio::spawn(async move {
        tokio::time::timeout(Duration::from_secs(15), worker.output())
            .await
            .unwrap()
            .unwrap()
    });
    let config=MockLlmBuilder::new().on_event("gearman_connected").respond_with_actions(json!([
        {"type":"set_memory","value":"submitted Gearman model job"},
        {"type":"gearman_request","operation":"submit","function_name":"model-job","workload":"chosen by model"}
    ])).expect_calls(1).and().on_event("gearman_response").and_prompt_containing("submitted Gearman model job").respond_with_actions(json!([])).expect_calls(1).and()
    .on_event("gearman_job_update").and_prompt_containing("submitted Gearman model job").respond_with_actions(json!([])).expect_calls(1).and().build();
    let mock = MockOllamaServer::start(config).await?;
    let state = netget::state::AppState::new_with_options(false, mock.base_url());
    state.set_ollama_model(Some("mock".into())).await;
    let (tx, _) = tokio::sync::mpsc::unbounded_channel();
    let id = netget::cli::management::ClientForm {
        protocol: "gearman".into(),
        remote_addr: Some(server.addr()),
        instruction: Some("Submit selected job and keep shared memory".into()),
        ..Default::default()
    }
    .create(&state, netget::llm::OllamaClient::new(mock.base_url()), tx)
    .await?;
    let (_, update) = event(&state, id, "gearman_job_update", 0).await;
    assert_eq!(update["kind"], "complete");
    assert_eq!(update["payload"]["text"], "CHOSEN BY MODEL");
    assert_eq!(
        state.get_memory_for_client(id).await.as_deref(),
        Some("submitted Gearman model job")
    );
    assert_eq!(mock.call_count().await, 3);
    mock.verify_calls().await?;
    let output = worker_task.await?;
    assert!(output.status.success());
    state.remove_client(id).await;
    Ok(())
}

#[tokio::test]
async fn advertised_static_and_script_examples_keep_function_name_against_cli_worker(
) -> crate::helpers::E2EResult<()> {
    use netget::llm::actions::protocol_trait::Protocol;
    let examples = netget::client::gearman::GearmanClientProtocol::new().get_startup_examples();
    examples.validate("Gearman").unwrap();
    let server = daemon().await?;
    let mut worker = cli(&server);
    worker.args([
        "-w",
        "-f",
        "reverse",
        "-c",
        "2",
        "--",
        "python3",
        "-c",
        "import sys; sys.stdout.write(sys.stdin.read()[::-1])",
    ]);
    let worker_task = tokio::spawn(async move {
        tokio::time::timeout(Duration::from_secs(15), worker.output())
            .await
            .unwrap()
            .unwrap()
    });
    let state = state();
    for example in [examples.static_mode, examples.script_mode] {
        let handlers = serde_json::from_value(example["event_handlers"].clone()).unwrap();
        let id = client(&state, server.addr(), "submitter", handlers).await;
        let (_, update) = event(&state, id, "gearman_job_update", 0).await;
        assert_eq!(update["request"]["function_name"], "reverse");
        assert_eq!(update["payload"]["text"], "olleh");
        state.remove_client(id).await;
    }
    let output = worker_task.await?;
    assert!(output.status.success());
    Ok(())
}
#[tokio::test]
async fn script_worker_registers_function_name_and_answers_cli_jobs_from_assigned_handle(
) -> crate::helpers::E2EResult<()> {
    let server = daemon().await?;
    let state = state();
    let id=client(&state,server.addr(),"worker",vec![
        json!({"event_pattern":"gearman_connected","handler":{"type":"script","language":"python","code":"import json,sys\njson.dump({'actions':[{'type':'gearman_worker','operation':'register','function_name':'scripted'},{'type':'gearman_worker','operation':'grab_unique'}]},sys.stdout)"}}),
        json!({"event_pattern":"gearman_response","handler":{"type":"script","language":"python","code":"import json,sys\nr=json.load(sys.stdin)['event']['response']\na=[]\nif r['kind']=='no_job': a=[{'type':'gearman_worker','operation':'sleep'}]\nif r['kind']=='job_assigned': a=[{'type':'gearman_worker','operation':'complete','job_handle':r['job_handle'],'result':r['workload']['text'][::-1]}]\njson.dump({'actions':a},sys.stdout)"}}),
        static_handler("gearman_worker_wakeup",json!([{"type":"gearman_worker","operation":"grab_unique"}])),
        static_handler("*",json!([])),
    ]).await;
    let mut producer = cli(&server);
    producer.args(["-f", "scripted", "-u", "script-unique", "script ✓"]);
    let output = tokio::time::timeout(Duration::from_secs(10), producer.output()).await??;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("✓ tpircs"));
    state.remove_client(id).await;
    Ok(())
}
