//! NetGet's Stratum V1 pool against **cpuminer 2.5.1** (pooler's `minerd`, sha256d), which
//! subscribes, authorizes, takes NetGet's jobs and difficulty, hashes them with its own SHA-256
//! code and submits shares; it prints "yay!!!" only for a share the pool accepted, and NetGet
//! accepts only a share whose header it rebuilt and found to meet the difficulty — so every
//! accepted share is the two implementations agreeing on the coinbase, merkle root, header and
//! hash. Then raw JSON-RPC for every refusal Rust decides, bounds, and a failed handler.
//! `install_peers.py` prints `NETGET_CPUMINER`; the test fails rather than skips without it.
//! No LLM calls: a python policy is the model.
use netget::cli::management::ServerForm;
use netget::server::stratum::wire;
use netget::state::app_state::AppState;
use netget::state::{AccessLogOwner, ServerId};
use serde_json::{json, Value};
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;
use tokio::sync::mpsc;

const POLICY: &str = r#"import json,sys
i=json.load(sys.stdin); t=i['event_type_id']; e=i['event']
if t=='stratum_authorize':
  a=[{'type':'stratum_accept'}] if e['worker'].startswith('netget') else [{'type':'stratum_reject','message':'unknown worker'}]
else:
  a=[{'type':'stratum_accept'},{'type':'stratum_show_message','message':'credited '+e['hash'][:16]}]
print(json.dumps({'actions':a}))"#;

fn cpuminer() -> String {
    let v = std::env::var("NETGET_CPUMINER").unwrap_or_default();
    assert!(
        !v.is_empty(),
        "NETGET_CPUMINER is required: python3 tests/server/stratum/install_peers.py <dir> and export what it prints"
    );
    v
}

async fn start(handlers: Option<Vec<Value>>, difficulty: f64) -> (AppState, ServerId, u16) {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    state
        .set_llm_client(netget::llm::OllamaClient::new("http://127.0.0.1:1"))
        .await;
    let (tx, _) = mpsc::unbounded_channel();
    let id = ServerForm {
        protocol: "stratum".into(),
        port: Some(0),
        host: Some("127.0.0.1".into()),
        instruction: Some("Run a small mining pool".into()),
        startup_params: Some(json!({"difficulty": difficulty})),
        event_handlers: handlers,
        ..Default::default()
    }
    .create(&state, tx)
    .await
    .unwrap();
    let port = tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            if let Some(a) = state.get_server(id).await.and_then(|s| s.local_addr) {
                break a.port();
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap();
    (state, id, port)
}

fn policy() -> Option<Vec<Value>> {
    Some(vec![
        json!({"event_pattern":"*","handler":{"type":"script","language":"python","code":POLICY}}),
    ])
}

async fn events(state: &AppState, id: ServerId, kind: &str) -> Vec<Value> {
    state
        .list_access_logs_for(Some(AccessLogOwner::Server(id.as_u32())), None)
        .await
        .iter()
        .map(|e| serde_json::to_value(e).unwrap())
        .filter(|e| e["event_type"] == kind)
        .map(|e| e["request"].clone())
        .collect()
}

#[tokio::test]
async fn cpuminer_mines_against_netget() {
    let difficulty = 0.0005;
    let (state, id, port) = start(policy(), difficulty).await;
    let mut miner = tokio::process::Command::new(cpuminer())
        .args(["-a", "sha256d", "-t", "1", "-o"])
        .arg(format!("stratum+tcp://127.0.0.1:{port}"))
        .args(["-u", "netget.rig1", "-p", "x", "-D"])
        .stderr(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .kill_on_drop(true)
        .spawn()
        .expect("minerd");
    let mut lines = BufReader::new(miner.stderr.take().unwrap()).lines();
    let mut seen = Vec::new();
    let accepted = tokio::time::timeout(Duration::from_secs(120), async {
        let mut accepted = 0;
        while let Ok(Some(l)) = lines.next_line().await {
            seen.push(l.clone());
            assert!(
                !l.contains("booooo"),
                "NetGet refused one of cpuminer's shares: {seen:#?}"
            );
            if l.contains("yay!!!") {
                accepted += 1;
                if accepted == 2 {
                    break;
                }
            }
        }
        accepted
    })
    .await
    .unwrap_or_else(|_| panic!("cpuminer had no two shares accepted:\n{}", seen.join("\n")));
    assert_eq!(accepted, 2, "{}", seen.join("\n"));
    let log = seen.join("\n");
    assert!(log.contains("Stratum difficulty set to 0.0005"), "{log}");
    drop(miner);

    let auth = events(&state, id, "stratum_authorize").await;
    assert_eq!(auth.len(), 1, "{auth:?}");
    assert_eq!(auth[0]["worker"], "netget.rig1", "{auth:?}");
    assert_eq!(auth[0]["password_given"], false, "{auth:?}");
    assert!(
        auth[0]["user_agent"]
            .as_str()
            .unwrap_or_default()
            .contains("cpuminer"),
        "{auth:?}"
    );
    let shares = events(&state, id, "stratum_share").await;
    assert!(shares.len() >= 2, "{shares:?}");
    for s in &shares {
        assert!(s["share_difficulty"].as_f64().unwrap() >= difficulty, "{s}");
        assert_eq!(s["difficulty"].as_f64().unwrap(), difficulty, "{s}");
        assert_eq!(s["worker"], "netget.rig1");
        assert_eq!(s["hash"].as_str().unwrap().len(), 64, "{s}");
    }
}

/// A raw Stratum connection: requests answered in order, notifications kept aside.
struct Raw {
    reader: tokio::io::Lines<BufReader<tokio::io::ReadHalf<TcpStream>>>,
    writer: tokio::io::WriteHalf<TcpStream>,
    next: u64,
    notes: Vec<Value>,
}

impl Raw {
    async fn connect(port: u16) -> Raw {
        let (r, w) = tokio::io::split(TcpStream::connect(("127.0.0.1", port)).await.unwrap());
        Raw {
            reader: BufReader::new(r).lines(),
            writer: w,
            next: 0,
            notes: Vec::new(),
        }
    }

    async fn call(&mut self, method: &str, params: Value) -> Value {
        self.next += 1;
        let line = format!(
            "{}\n",
            json!({"id": self.next, "method": method, "params": params})
        );
        self.writer.write_all(line.as_bytes()).await.unwrap();
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                let l = self.reader.next_line().await.unwrap().expect("pool closed");
                let v: Value = serde_json::from_str(&l).unwrap();
                if v["id"] == self.next {
                    return v;
                }
                self.notes.push(v);
            }
        })
        .await
        .expect("no answer")
    }

    /// Read until a notification of `method` has arrived (answers come before their work).
    async fn note(&mut self, method: &str) -> Value {
        tokio::time::timeout(Duration::from_secs(30), async {
            loop {
                if let Some(n) = self.notes.iter().find(|n| n["method"] == method) {
                    return n.clone();
                }
                let l = self.reader.next_line().await.unwrap().expect("pool closed");
                self.notes.push(serde_json::from_str(&l).unwrap());
            }
        })
        .await
        .unwrap_or_else(|_| panic!("no {method}: {:?}", self.notes))
    }

    fn last_job(&self) -> wire::Job {
        let n = self
            .notes
            .iter()
            .rev()
            .find(|n| n["method"] == "mining.notify")
            .expect("no job");
        wire::Job::from_notify(&n["params"]).unwrap()
    }
}

fn code(v: &Value) -> u64 {
    v["error"][0]
        .as_u64()
        .unwrap_or_else(|| panic!("no error code: {v}"))
}

#[tokio::test]
async fn rust_refuses_bad_shares_and_the_model_credits_good_ones() {
    let difficulty = 0.000001;
    let (state, id, port) = start(policy(), difficulty).await;
    let mut c = Raw::connect(port).await;

    // Nothing before mining.subscribe.
    assert_eq!(
        code(&c.call("mining.authorize", json!(["netget.a", "x"])).await),
        25
    );
    let sub = c.call("mining.subscribe", json!(["raw/1"])).await;
    let en1 = hex::decode(sub["result"][1].as_str().unwrap()).unwrap();
    assert_eq!(en1.len(), 4, "{sub}");
    assert_eq!(sub["result"][2], 4, "{sub}");

    // The model refuses a worker it does not know, and nothing may be submitted for it.
    let no = c
        .call("mining.authorize", json!(["intruder", "secret"]))
        .await;
    assert_eq!(no["result"], false, "{no}");
    assert_eq!(no["error"], json!([24, "unknown worker", null]));
    let ok = c.call("mining.authorize", json!(["netget.a", "x"])).await;
    assert_eq!(ok["result"], true, "{ok}");
    assert_eq!(
        c.note("mining.set_difficulty").await["params"],
        json!([difficulty])
    );
    c.note("mining.notify").await;
    let job = c.last_job();
    let ntime = wire::u32_hex(job.ntime);

    let submit = |worker: &str, job_id: &str, en2: &str, ntime: &str, nonce: u32| {
        json!([worker, job_id, en2, ntime, wire::u32_hex(nonce)])
    };
    assert_eq!(
        code(
            &c.call(
                "mining.submit",
                submit("intruder", &job.job_id, "00000001", &ntime, 0)
            )
            .await
        ),
        24
    );
    assert_eq!(
        code(
            &c.call(
                "mining.submit",
                submit("netget.a", "nope", "00000001", &ntime, 0)
            )
            .await
        ),
        21
    );
    assert_eq!(
        code(
            &c.call(
                "mining.submit",
                submit("netget.a", &job.job_id, "0001", &ntime, 0)
            )
            .await
        ),
        20
    );
    let late = wire::u32_hex(job.ntime + wire::NTIME_ROLL + 1);
    assert_eq!(
        code(
            &c.call(
                "mining.submit",
                submit("netget.a", &job.job_id, "00000001", &late, 0)
            )
            .await
        ),
        20
    );

    // A share below the difficulty, found with NetGet's own hashing code, is refused by Rust
    // with the hash in the message; one that meets it goes to the model, which credits it.
    let en2 = [0u8, 0, 0, 1];
    let meets =
        |nonce: u32| wire::difficulty(&job.share_hash(&en1, &en2, job.ntime, nonce)) >= difficulty;
    let low = (0..).find(|n| !meets(*n)).unwrap();
    let good = (0..u32::MAX).find(|n| meets(*n)).unwrap();
    let r = c
        .call(
            "mining.submit",
            submit("netget.a", &job.job_id, "00000001", &ntime, low),
        )
        .await;
    assert_eq!(code(&r), 23, "{r}");
    let low_hash = wire::display_hex(&job.share_hash(&en1, &en2, job.ntime, low));
    assert!(r["error"][1].as_str().unwrap().contains(&low_hash), "{r}");
    let r = c
        .call(
            "mining.submit",
            submit("netget.a", &job.job_id, "00000001", &ntime, good),
        )
        .await;
    assert_eq!(r["result"], true, "{r}");
    let shown = c.note("client.show_message").await;
    assert!(
        shown["params"][0]
            .as_str()
            .unwrap()
            .starts_with("credited "),
        "{shown}"
    );
    let again = c
        .call(
            "mining.submit",
            submit("netget.a", &job.job_id, "00000001", &ntime, good),
        )
        .await;
    assert_eq!(code(&again), 22, "{again}");
    let shares = events(&state, id, "stratum_share").await;
    assert_eq!(
        shares.len(),
        1,
        "only the good share reaches the model: {shares:?}"
    );
    assert_eq!(
        shares[0]["hash"],
        wire::display_hex(&job.share_hash(&en1, &en2, job.ntime, good))
    );
    assert_eq!(code(&c.call("mining.nonsense", json!([])).await), 20);
    let auth = events(&state, id, "stratum_authorize").await;
    assert_eq!(auth.len(), 2, "{auth:?}");
    let intruder = auth.iter().find(|a| a["worker"] == "intruder").unwrap();
    assert_eq!(intruder["password_given"], true, "{auth:?}");
    assert!(!serde_json::to_string(&auth).unwrap().contains("secret"));

    // A line past the bound ends the connection.
    let mut big = Raw::connect(port).await;
    let line = format!(
        "{{\"id\":1,\"method\":\"mining.subscribe\",\"params\":[\"{}\"]}}\n",
        "a".repeat(wire::MAX_LINE)
    );
    let _ = big.writer.write_all(line.as_bytes()).await;
    let end = tokio::time::timeout(Duration::from_secs(10), big.reader.next_line())
        .await
        .unwrap();
    assert!(matches!(end, Ok(None) | Err(_)), "{end:?}");
}

#[tokio::test]
async fn a_failed_handler_authorizes_nobody() {
    let (_state, _id, port) = start(None, 1.0).await;
    let mut c = Raw::connect(port).await;
    c.call("mining.subscribe", json!([])).await;
    let r = c.call("mining.authorize", json!(["netget.a", "x"])).await;
    assert_eq!(r["result"], false, "{r}");
    assert_eq!(code(&r), 20, "{r}");
    let text = r["error"][1].as_str().unwrap();
    assert!(
        !text.contains("127.0.0.1") && !text.contains("LLM"),
        "the error leaks: {text}"
    );
    assert!(
        c.notes.is_empty(),
        "no work for an unauthorized miner: {:?}",
        c.notes
    );
    let job = c
        .call(
            "mining.submit",
            json!(["netget.a", "1", "00000000", "00000000", "00000000"]),
        )
        .await;
    assert_eq!(code(&job), 24, "{job}");
}
