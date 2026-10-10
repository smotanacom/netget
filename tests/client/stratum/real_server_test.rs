//! NetGet's Stratum V1 miner against **ckpool** (solo mode) building its work from **Bitcoin
//! Core 28.1** in regtest. NetGet subscribes and authorizes with a regtest address (ckpool
//! checks it with the node), the handler mines each job, and NetGet submits. ckpool's minimum
//! difficulty is 1 — about 4 billion hashes a share — so the share is submitted with
//! `submit_best` and refused "Above target"; what the test asserts is that ckpool's log names
//! the very hash NetGet computed for it, which only happens when the two agree byte for byte
//! on the coinbase, merkle root and header. `tests/server/stratum/install_peers.py` prints
//! `NETGET_CKPOOL`, `NETGET_BITCOIND` and `NETGET_BITCOIN_CLI`; the test fails rather than
//! skips without them. No LLM calls: a python chain is the model.
use netget::cli::management::ClientForm;
use netget::state::app_state::AppState;
use netget::state::client_handles::ClientSendOutcome;
use netget::state::{AccessLogOwner, ClientId};
use serde_json::{json, Value};
use std::path::Path;
use std::time::Duration;
use tokio::sync::mpsc;

/// Every job is mined briefly and the best share submitted whatever it meets.
const CHAIN: &str = r#"import json,sys
i=json.load(sys.stdin); t=i['event_type_id']
a=[{'type':'stratum_mine','max_hashes':20000,'submit_best':True}] if t=='stratum_job' else []
print(json.dumps({'actions':a}))"#;

fn env(var: &str) -> String {
    let v = std::env::var(var).unwrap_or_default();
    assert!(
        !v.is_empty(),
        "{var} is required: python3 tests/server/stratum/install_peers.py <dir> and export what it prints"
    );
    v
}

fn free_port() -> u16 {
    std::net::TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

struct Node {
    _bitcoind: tokio::process::Child,
    _ckpool: tokio::process::Child,
    address: String,
    pool_port: u16,
    /// ckpool's own log file, which carries the INFO lines the console omits.
    log_file: std::path::PathBuf,
    _dirs: (tempfile::TempDir, tempfile::TempDir),
}

async fn cli(dir: &Path, rpc: u16, args: &[&str]) -> Result<String, String> {
    let out = tokio::process::Command::new(env("NETGET_BITCOIN_CLI"))
        .arg("-regtest")
        .arg(format!("-datadir={}", dir.display()))
        .args(["-rpcuser=netget", "-rpcpassword=netget"])
        .arg(format!("-rpcport={rpc}"))
        .args(args)
        .output()
        .await
        .map_err(|e| e.to_string())?;
    if out.status.success() {
        Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
    } else {
        Err(String::from_utf8_lossy(&out.stderr).to_string())
    }
}

/// bitcoind in regtest with one block, and ckpool mining solo on it.
async fn node() -> Node {
    let data = tempfile::tempdir().unwrap();
    // ckpool's unix sockets need a short path.
    let sock = tempfile::Builder::new()
        .prefix("ck")
        .tempdir_in("/tmp")
        .unwrap();
    let (rpc, p2p, pool_port) = (free_port(), free_port(), free_port());
    let btc = data.path().join("btc");
    std::fs::create_dir_all(&btc).unwrap();
    let bitcoind = tokio::process::Command::new(env("NETGET_BITCOIND"))
        .arg("-regtest")
        .arg(format!("-datadir={}", btc.display()))
        .args([
            "-rpcuser=netget",
            "-rpcpassword=netget",
            "-listen=0",
            "-txindex=0",
        ])
        .arg(format!("-rpcport={rpc}"))
        .arg(format!("-port={p2p}"))
        .stdout(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("bitcoind");
    tokio::time::timeout(Duration::from_secs(60), async {
        while cli(&btc, rpc, &["getblockchaininfo"]).await.is_err() {
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .expect("bitcoind never answered RPC");
    cli(&btc, rpc, &["createwallet", "w"]).await.unwrap();
    let address = cli(&btc, rpc, &["getnewaddress"]).await.unwrap();
    cli(&btc, rpc, &["generatetoaddress", "1", &address])
        .await
        .unwrap();

    let conf = data.path().join("ckpool.conf");
    std::fs::write(
        &conf,
        json!({"btcd": [{"url": format!("127.0.0.1:{rpc}"), "auth": "netget", "pass": "netget", "notify": false}],
               "btcaddress": address, "serverurl": [format!("127.0.0.1:{pool_port}")],
               "mindiff": 1, "startdiff": 1, "logdir": data.path().join("logs")})
        .to_string(),
    )
    .unwrap();
    let ck_log = data.path().join("ckpool.out");
    let out = std::fs::File::create(&ck_log).unwrap();
    let ckpool = tokio::process::Command::new(env("NETGET_CKPOOL"))
        .arg("-c")
        .arg(&conf)
        .arg("-s")
        .arg(sock.path())
        .args(["-n", "ckt", "-l", "6", "-B"])
        .stdout(out.try_clone().unwrap())
        .stderr(out)
        .kill_on_drop(true)
        .spawn()
        .expect("ckpool");
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let log = std::fs::read_to_string(&ck_log).unwrap_or_default();
            if log.contains("stratifier ready")
                && tokio::net::TcpStream::connect(("127.0.0.1", pool_port))
                    .await
                    .is_ok()
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "ckpool never came up:\n{}",
            std::fs::read_to_string(&ck_log).unwrap_or_default()
        )
    });
    Node {
        _bitcoind: bitcoind,
        _ckpool: ckpool,
        address,
        pool_port,
        log_file: data.path().join("logs").join("ckt.log"),
        _dirs: (data, sock),
    }
}

async fn wait_event(state: &AppState, id: ClientId, pred: impl Fn(&Value) -> bool) -> Value {
    tokio::time::timeout(Duration::from_secs(60), async {
        loop {
            let hit = state
                .list_access_logs_for(Some(AccessLogOwner::Client(id.as_u32())), None)
                .await
                .iter()
                .map(|e| serde_json::to_value(e).unwrap())
                .find(|e| pred(e))
                .map(|e| e["request"].clone());
            if let Some(e) = hit {
                return e;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("no matching event")
}

async fn ckpool_logged(node: &Node, needle: &str) -> String {
    tokio::time::timeout(Duration::from_secs(20), async {
        loop {
            let log = std::fs::read_to_string(&node.log_file).unwrap_or_default();
            if let Some(line) = log.split(['\n', '\r']).find(|l| l.contains(needle)) {
                return line.to_string();
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    })
    .await
    .unwrap_or_else(|_| {
        panic!(
            "ckpool never logged {needle}:\n{}",
            std::fs::read_to_string(&node.log_file).unwrap_or_default()
        )
    })
}

#[tokio::test]
async fn netget_mines_ckpool_work_and_agrees_on_every_hash() {
    let node = node().await;
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    let (tx, _) = mpsc::unbounded_channel();
    let id = ClientForm {
        protocol: "stratum".into(),
        remote_addr: Some(format!("stratum+tcp://127.0.0.1:{}", node.pool_port)),
        instruction: Some("Mine every job".into()),
        startup_params: Some(json!({"user": node.address})),
        event_handlers: Some(vec![
            json!({"event_pattern":"*","handler":{"type":"script","language":"python","code":CHAIN}}),
        ]),
        ..Default::default()
    }
    .create(&state, netget::llm::OllamaClient::new("http://127.0.0.1:1"), tx)
    .await
    .expect("connect");

    let auth = wait_event(&state, id, |e| e["event_type"] == "stratum_authorized").await;
    assert_eq!(
        auth["ok"], true,
        "ckpool refused the regtest address: {auth}"
    );
    assert_eq!(auth["worker"], node.address.as_str());
    let job = wait_event(&state, id, |e| e["event_type"] == "stratum_job").await;
    assert_eq!(job["nbits"], "207fffff", "regtest work: {job}");
    assert!(
        job["difficulty"].as_f64().unwrap() >= 1.0,
        "ckpool's minimum is 1: {job}"
    );

    // The handler mined and submitted; ckpool refused it, and logged the hash it computed.
    let share = wait_event(&state, id, |e| e["event_type"] == "stratum_share_result").await;
    assert_eq!(share["accepted"], false, "{share}");
    assert_eq!(share["error"], json!([23, "Above target", null]), "{share}");
    let hash = share["hash"].as_str().unwrap().to_string();
    let line = ckpool_logged(&node, &hash).await;
    assert!(line.contains("high diff"), "{line}");

    // By hand: a share with nonce 0 on the latest job, the same agreement.
    let outcome = state
        .send_to_client(
            id,
            json!({"type":"stratum_submit","nonce":0,"extranonce2":4242}),
            Duration::from_secs(30),
        )
        .await
        .unwrap();
    let ClientSendOutcome::Executed { detail } = outcome else {
        panic!("{outcome:?}");
    };
    let detail: Value = serde_json::from_str(&detail).unwrap();
    assert_eq!(detail["error"][0], 23, "{detail}");
    ckpool_logged(&node, detail["hash"].as_str().unwrap()).await;

    // Refused before the wire: no such job.
    let outcome = state
        .send_to_client(
            id,
            json!({"type":"stratum_submit","job_id":"nope","nonce":1,"extranonce2":1}),
            Duration::from_secs(10),
        )
        .await
        .unwrap();
    assert!(
        matches!(&outcome, ClientSendOutcome::Rejected { error } if error.contains("no such job")),
        "{outcome:?}"
    );

    // ckpool checks a solo worker is a payable address; NetGet reports its refusal.
    let (tx, _) = mpsc::unbounded_channel();
    let refused = ClientForm {
        protocol: "stratum".into(),
        remote_addr: Some(format!("127.0.0.1:{}", node.pool_port)),
        instruction: Some("Mine".into()),
        startup_params: Some(json!({"user": "not-an-address"})),
        event_handlers: Some(vec![
            json!({"event_pattern":"*","handler":{"type":"static","actions":[]}}),
        ]),
        ..Default::default()
    }
    .create(
        &state,
        netget::llm::OllamaClient::new("http://127.0.0.1:1"),
        tx,
    )
    .await
    .expect("connect");
    let auth = wait_event(&state, refused, |e| e["event_type"] == "stratum_authorized").await;
    assert_eq!(auth["ok"], false, "{auth}");
}
