use super::common::*;
use crate::helpers::real_server::{find_binary, InstallHint, RealServer};
use serde_json::json;
use std::{process::Stdio, time::Duration};
#[tokio::test]
async fn real_dictd_definitions_search_discovery_and_errors() -> crate::helpers::E2EResult<()> {
    let data = tempfile::tempdir()?;
    let source = data.path().join("source.txt");
    std::fs::write(
        &source,
        ":hello: A greeting from the independent server.\n.dot starts here\n:help: Assistance.\n",
    )?;
    let dictfmt = find_binary("dictfmt")
        .expect("dictfmt required: brew install dict / apt-get install dictfmt dictd");
    let output = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::process::Command::new(dictfmt)
            .args([
                "-j",
                "--utf8",
                "--quiet",
                "--without-headword",
                "--without-time",
                "-s",
                "Fixture dictionary",
            ])
            .arg(data.path().join("fixture"))
            .stdin(Stdio::from(std::fs::File::open(source)?))
            .kill_on_drop(true)
            .output(),
    )
    .await??;
    assert!(
        output.status.success(),
        "dictfmt: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let conf=format!("access {{ allow * }}\ndatabase fixture {{ data \"{}/fixture.dict\" index \"{}/fixture.index\" }}\n",data.path().display(),data.path().display());
    let server = RealServer::builder(
        "dictd",
        InstallHint {
            brew: "dict",
            apt: "dictd dictfmt",
        },
    )
    .config_file("dictd.conf", &conf)
    .args([
        "--debug",
        "nodetach",
        "--listen-to",
        "127.0.0.1",
        "--port",
        "{port}",
        "--config",
        "{dir}/dictd.conf",
        "--pid-file",
        "{dir}/dictd.pid",
    ])
    .start()
    .await?;
    let state = state();
    let id = client(&state, server.addr(), json!([])).await;
    assert_eq!(
        request(
            &state,
            id,
            json!({"operation":"client","name":"NetGet independent test"})
        )
        .await["code"],
        250
    );
    let definitions = request(
        &state,
        id,
        json!({"operation":"define","database":"fixture","word":"hello"}),
    )
    .await;
    assert!(
        definitions["definitions"][0]["text"]
            .as_str()
            .unwrap()
            .contains("independent server"),
        "{definitions}"
    );
    assert!(definitions["definitions"][0]["text"]
        .as_str()
        .unwrap()
        .contains(".dot"));
    let matches = request(
        &state,
        id,
        json!({"operation":"match","database":"fixture","strategy":"prefix","word":"hel"}),
    )
    .await;
    assert!(matches["entries"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["word"] == "hello"));
    let databases = request(&state, id, json!({"operation":"databases"})).await;
    assert_eq!(databases["entries"][0]["name"], "fixture");
    let strategies = request(&state, id, json!({"operation":"strategies"})).await;
    assert!(strategies["entries"]
        .as_array()
        .unwrap()
        .iter()
        .any(|e| e["name"] == "prefix"));
    for operation in ["info", "server", "help"] {
        let r = request(
            &state,
            id,
            json!({"operation":operation,"database":"fixture"}),
        )
        .await;
        assert!(!r["text"].as_str().unwrap().is_empty(), "{r}");
    }
    assert_eq!(
        request(&state, id, json!({"operation":"status"})).await["code"],
        210
    );
    assert_eq!(
        request(
            &state,
            id,
            json!({"operation":"define","word":"missing-test-word"})
        )
        .await["code"],
        552
    );
    assert_eq!(
        request(
            &state,
            id,
            json!({"operation":"define","database":"absent","word":"hello"})
        )
        .await["code"],
        550
    );
    assert_eq!(
        request(&state, id, json!({"operation":"quit"})).await["code"],
        221
    );
    state.remove_client(id).await;
    Ok(())
}
