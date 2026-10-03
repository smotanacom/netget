//! Independent upsd plus dummy-ups driver, entirely inside an owned temporary directory.
//! Set NUT_UPSD_BIN and NUT_DUMMY_BIN for an uninstalled source build. No skip gates.
use super::session_test::{start, wait_log};
use serde_json::json;
use std::{path::PathBuf, time::Duration};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
fn binary(env: &str, name: &str) -> PathBuf {
    std::env::var_os(env)
        .map(PathBuf::from)
        .or_else(|| crate::helpers::real_server::find_binary(name))
        .unwrap_or_else(|| {
            panic!("Independent NUT {name} required: install nut/nut-server or set {env}")
        })
}
#[tokio::test]
async fn netget_reads_independent_upsd_and_dummy_driver() {
    let upsd = binary("NUT_UPSD_BIN", "upsd");
    let driver = binary("NUT_DUMMY_BIN", "dummy-ups");
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path();
    let probe = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = probe.local_addr().unwrap();
    drop(probe);
    std::fs::write(
        path.join("upsd.conf"),
        format!(
            "LISTEN 127.0.0.1 {}\nSTATEPATH {}\n",
            addr.port(),
            path.display()
        ),
    )
    .unwrap();
    std::fs::write(path.join("upsd.users"), "").unwrap();
    std::fs::write(path.join("ups.conf"),format!("[rack1]\n driver = dummy-ups\n port = {}\n mode = dummy-once\n desc = \"Independent rack UPS\"\n",path.join("sample.dev").display())).unwrap();
    std::fs::write(
        path.join("sample.dev"),
        "ups.status: OL\nbattery.charge: 97\nups.model: IndependentDummy\n",
    )
    .unwrap();
    let spawn = |binary: &PathBuf, args: &[&str], log: &str| {
        let file = std::fs::File::create(path.join(log)).unwrap();
        let mut command = tokio::process::Command::new(binary);
        command
            .args(args)
            .env("NUT_CONFPATH", path)
            .env("NUT_STATEPATH", path)
            .env("NUT_ALTPIDPATH", path)
            .stdout(file.try_clone().unwrap())
            .stderr(file)
            .kill_on_drop(true);
        command.spawn().expect("launch independent NUT process")
    };
    let mut driver = spawn(&driver, &["-a", "rack1", "-F"], "driver.log");
    let mut server = spawn(&upsd, &["-F"], "upsd.log");
    let ready = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            assert!(
                driver.try_wait().unwrap().is_none(),
                "dummy-ups exited: {}",
                std::fs::read_to_string(path.join("driver.log")).unwrap()
            );
            assert!(
                server.try_wait().unwrap().is_none(),
                "upsd exited: {}",
                std::fs::read_to_string(path.join("upsd.log")).unwrap()
            );
            if let Ok(socket) = tokio::net::TcpStream::connect(addr).await {
                let mut reader = BufReader::new(socket);
                reader
                    .get_mut()
                    .write_all(b"GET VAR rack1 battery.charge\n")
                    .await
                    .unwrap();
                let mut line = String::new();
                if tokio::time::timeout(Duration::from_secs(1), reader.read_line(&mut line))
                    .await
                    .is_ok()
                    && line.contains("\"97\"")
                {
                    break;
                }
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await;
    assert!(
        ready.is_ok(),
        "Independent server not ready: {} / {}",
        std::fs::read_to_string(path.join("upsd.log")).unwrap(),
        std::fs::read_to_string(path.join("driver.log")).unwrap()
    );
    let (state, id) = start(
        addr.to_string(),
        json!([{"type":"nut_request","operation":"get_var","ups":"rack1","name":"battery.charge"}]),
    )
    .await;
    wait_log(&state, id, "97").await;
    state
        .send_to_client(
            id,
            json!({"type":"nut_request","operation":"list_var","ups":"rack1"}),
            Duration::from_secs(3),
        )
        .await
        .unwrap();
    wait_log(&state, id, "IndependentDummy").await;
    state.remove_client(id).await;
    server.kill().await.unwrap();
    driver.kill().await.unwrap();
    server.wait().await.unwrap();
    driver.wait().await.unwrap();
}
