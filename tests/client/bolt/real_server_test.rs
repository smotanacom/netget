use super::common::*;
use crate::helpers::real_server::{find_binary, InstallHint, RealServer};
use serde_json::{json, Value};
use std::{path::PathBuf, time::Duration};
const PASSWORD: &str = "netget-owned-bolt-password-61";
const WRAPPER: &str = r#"import os,pathlib,shutil,subprocess,sys
peer,directory,port,java=sys.argv[1:]
root=pathlib.Path(directory)
for d in ['home','conf','data','logs','run','plugins','import','transactions']:(root/d).mkdir()
env={'HOME':str(root/'home'),'PATH':os.environ.get('PATH',''),'JAVA_HOME':java,'NEO4J_HOME':peer,'NEO4J_CONF':str(root/'conf')}
settings={'server.default_listen_address':'127.0.0.1','server.bolt.enabled':'true','server.bolt.listen_address':'127.0.0.1:'+port,'server.http.enabled':'false','server.https.enabled':'false','dbms.usage_report.enabled':'false','server.memory.heap.initial_size':'128m','server.memory.heap.max_size':'256m','server.memory.pagecache.size':'64m','server.bolt.thread_pool_min_size':'1','server.bolt.thread_pool_max_size':'4','db.tx_log.preallocate':'false','db.tx_log.rotation.size':'16M','db.tx_log.rotation.retention_policy':'32M size'}
for d in ['data','logs','run','plugins','import']:settings['server.directories.'+d]=str(root/d)
settings['server.directories.transaction.logs.root']=str(root/'transactions')
for name in ['user-logs.xml','server-logs.xml']:
 shutil.copyfile(pathlib.Path(peer)/'conf'/name,root/'conf'/name)
(root/'conf'/'neo4j.conf').write_text(''.join(k+'='+v+'\n' for k,v in settings.items()))
credential=root/'password.args';credential.write_text('netget-owned-bolt-password-61\n');credential.chmod(0o600)
# Neo4j's supported @argument-file expansion avoids a credential in CLI arguments.
p=subprocess.run([peer+'/bin/neo4j-admin','dbms','set-initial-password','@'+str(credential)],env=env,stdout=subprocess.PIPE,stderr=subprocess.STDOUT,text=True,timeout=30)
credential.unlink()
if p.returncode:raise RuntimeError('isolated initial-password command failed: '+p.stdout.replace('netget-owned-bolt-password-61','<redacted>'))
p=subprocess.Popen([peer+'/bin/neo4j','console'],env=env,stdout=subprocess.PIPE,stderr=subprocess.STDOUT,text=True)
for line in p.stdout:
 print(line.replace('netget-owned-bolt-password-61','<redacted>'),end='',flush=True)
 if 'Started.' in line:print('BOLT_PEER_READY',flush=True)
p.wait();sys.exit(p.returncode)
"#;
fn peer_home() -> PathBuf {
    let root = std::env::var_os("NETGET_NEO4J_HOME").expect("NETGET_NEO4J_HOME must point to the verified official Neo4j Community5.26.31 peer; run scripts/test-peers/install-neo4j.sh in an owned absolute directory");
    let root = PathBuf::from(root);
    assert!(root.is_absolute() && root.join("bin/neo4j").is_file());
    root
}
fn java_home() -> String {
    if let Ok(java) = std::env::var("NETGET_BOLT_JAVA_HOME") {
        return java;
    }
    std::env::var("JAVA_HOME")
        .expect("JAVA_HOME or NETGET_BOLT_JAVA_HOME must name an isolated Java21 installation")
}
async fn daemon() -> crate::helpers::E2EResult<RealServer> {
    RealServer::builder(
        "python3",
        InstallHint {
            brew: "python3 and official Neo4j Community5.26.31 with Java21",
            apt: "python3 openjdk-21-jre-headless and scripts/test-peers/install-neo4j.sh",
        },
    )
    .config_file("bolt_peer.py", WRAPPER)
    .args([
        "-u",
        "{dir}/bolt_peer.py",
        peer_home().to_str().unwrap(),
        "{dir}",
        "{port}",
        &java_home(),
    ])
    .startup_timeout(Duration::from_secs(60))
    .ready_when_log_matches("BOLT_PEER_READY")
    .start()
    .await
}
async fn request(
    state: &netget::state::AppState,
    id: netget::state::ClientId,
    action: Value,
    name: &str,
) -> Value {
    let after = latest(state, id).await;
    send(state, id, action).await;
    event(state, id, name, after).await.1
}
async fn run(
    state: &netget::state::AppState,
    id: netget::state::ClientId,
    q: &str,
    parameters: Value,
) -> Value {
    request(
        state,
        id,
        json!({"type":"bolt_run","query":q,"parameters":parameters}),
        "bolt_query_started",
    )
    .await
}
async fn pull(state: &netget::state::AppState, id: netget::state::ClientId, n: usize) -> Value {
    request(
        state,
        id,
        json!({"type":"bolt_pull","n":n}),
        "bolt_result_page",
    )
    .await
}
#[tokio::test]
async fn official_neo4j_auth_queries_pages_native_values_transactions_errors_and_cli_readback(
) -> crate::helpers::E2EResult<()> {
    let peer = daemon().await?;
    let state = state();
    let addr = format!("bolt://{}", peer.addr());
    let id = client(
        &state,
        addr.clone(),
        json!({}),
        vec![static_handler("*", json!([]))],
    )
    .await;
    let connected = event(&state, id, "bolt_connected", 0).await.1;
    assert_eq!(connected["version"], "5.8");
    assert_eq!(connected["authentication_verified"], false);
    assert_eq!(connected["server"]["server"], "Neo4j/5.26.31");
    rejected(
        &state,
        id,
        json!({"type":"bolt_run","query":"RETURN 1"}),
        "ready phase",
    )
    .await;
    let auth = request(
        &state,
        id,
        json!({"type":"bolt_login","username":"neo4j","password":PASSWORD}),
        "bolt_authentication",
    )
    .await;
    assert_eq!(auth["authentication_verified"], true);
    let started = run(
        &state,
        id,
        "UNWIND range(1,5) AS n RETURN n AS number, $text AS text",
        json!({"text":"film ☃"}),
    )
    .await;
    assert_eq!(started["fields"], json!(["number", "text"]));
    let first = pull(&state, id, 2).await;
    assert_eq!(first["records"], json!([[1, "film ☃"], [2, "film ☃"]]));
    assert_eq!(first["has_more"], true);
    rejected(
        &state,
        id,
        json!({"type":"bolt_run","query":"RETURN 2"}),
        "no open result",
    )
    .await;
    let next = pull(&state, id, 2).await;
    assert_eq!(next["records"], json!([[3, "film ☃"], [4, "film ☃"]]));
    assert_eq!(next["has_more"], true);
    let last = pull(&state, id, 2).await;
    assert_eq!(last["records"], json!([[5, "film ☃"]]));
    assert_eq!(last["has_more"], false);
    assert_eq!(last["summary"]["type"], "r");
    assert!(last["summary"]["bookmark"].is_string());
    run(&state,id,"RETURN date('2026-10-03') AS d, localtime('12:34:56.123') AS t, datetime('2026-10-03T12:34:56.123Z') AS dt, duration('P1M2DT3.4S') AS dur, point({x:1.0,y:2.0}) AS p",json!({})).await;
    let page = pull(&state, id, 1).await;
    let row = page["records"][0].as_array().unwrap();
    assert!(row[0]["$date"]["days_since_epoch"].is_i64());
    assert_eq!(
        row[1]["$local_time"]["nanoseconds_since_midnight"],
        45_296_123_000_000i64
    );
    assert_eq!(row[2]["$datetime"]["nanoseconds"], 123_000_000);
    assert_eq!(row[3]["$duration"]["months"], 1);
    assert_eq!(row[4]["$point"]["coordinates"], json!([1.0, 2.0]));
    run(&state,id,"RETURN time('12:34:56.123-04:00') AS t, localdatetime('2026-10-03T12:34:56.123') AS ldt, datetime('2026-10-03T12:34:56.123-04:00[America/Toronto]') AS zdt, duration('-PT0.4S') AS negative",json!({})).await;
    let page = pull(&state, id, 1).await;
    assert_eq!(page["records"][0][0]["$time"]["offset_seconds"], -14400);
    assert_eq!(
        page["records"][0][1]["$local_datetime"]["nanoseconds"],
        123_000_000
    );
    assert_eq!(
        page["records"][0][2]["$datetime_zone"]["zone_id"],
        "America/Toronto"
    );
    assert_eq!(page["records"][0][3]["$duration"]["seconds"], -1);
    assert_eq!(
        page["records"][0][3]["$duration"]["nanoseconds"],
        600_000_000
    );
    request(
        &state,
        id,
        json!({"type":"bolt_begin"}),
        "bolt_session_result",
    )
    .await;
    run(
        &state,
        id,
        "CREATE (n:NetgetOwned61 {name:$name}) RETURN n",
        json!({"name":"owned"}),
    )
    .await;
    let node = pull(&state, id, 1).await;
    assert_eq!(
        node["records"][0][0]["$node"]["properties"]["name"],
        "owned"
    );
    assert!(node["records"][0][0]["$node"]["element_id"].is_string());
    let committed = request(
        &state,
        id,
        json!({"type":"bolt_commit"}),
        "bolt_session_result",
    )
    .await;
    assert!(committed["metadata"]["bookmark"].is_string());
    run(&state,id,"MATCH (a:NetgetOwned61) CREATE p=(a)-[r:NETGET_KNOWS61 {weight:3}]->(b:NetgetOwnedNeighbor61 {name:'neighbor'}) RETURN p,r",json!({})).await;
    let graph = pull(&state, id, 1).await;
    assert_eq!(graph["records"][0][0]["$path"]["indices"], json!([1, 1]));
    assert_eq!(
        graph["records"][0][0]["$path"]["relationships"][0]["$unbound_relationship"]
            ["relationship_type"],
        "NETGET_KNOWS61"
    );
    assert_eq!(
        graph["records"][0][1]["$relationship"]["properties"]["weight"],
        3
    );
    assert_eq!(graph["summary"]["stats"]["relationships-created"], 1);
    let cli = find_binary("cypher-shell").expect("required independent Neo4j cypher-shell");
    let output = tokio::time::timeout(
        Duration::from_secs(30),
        tokio::process::Command::new(cli)
            .env("JAVA_HOME", java_home())
            .env("HOME", peer.dir().join("home"))
            .env("NEO4J_USERNAME", "neo4j")
            .env("NEO4J_PASSWORD", PASSWORD)
            .args([
                "-a",
                &addr,
                "--format",
                "plain",
                "MATCH (n:NetgetOwned61) RETURN n.name;",
            ])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .unwrap()
    .unwrap();
    assert!(
        output.status.success(),
        "cypher-shell failed: {}",
        String::from_utf8_lossy(&output.stderr).replace(PASSWORD, "<redacted>")
    );
    assert!(String::from_utf8_lossy(&output.stdout).contains("owned"));
    request(
        &state,
        id,
        json!({"type":"bolt_begin"}),
        "bolt_session_result",
    )
    .await;
    run(&state, id, "CREATE (:NetgetRolledBack61)", json!({})).await;
    pull(&state, id, 1).await;
    request(
        &state,
        id,
        json!({"type":"bolt_rollback"}),
        "bolt_session_result",
    )
    .await;
    run(
        &state,
        id,
        "MATCH (n:NetgetRolledBack61) RETURN count(n)",
        json!({}),
    )
    .await;
    assert_eq!(pull(&state, id, 1).await["records"], json!([[0]]));
    let failure = request(
        &state,
        id,
        json!({"type":"bolt_run","query":"THIS IS NOT CYPHER"}),
        "bolt_failure",
    )
    .await;
    assert_eq!(
        failure["failure"]["neo4j_code"],
        "Neo.ClientError.Statement.SyntaxError"
    );
    assert!(failure["failure"]["gql_status"].is_string());
    assert_eq!(failure["records_discarded"], 0);
    rejected(
        &state,
        id,
        json!({"type":"bolt_run","query":"RETURN 1"}),
        "ready phase",
    )
    .await;
    request(
        &state,
        id,
        json!({"type":"bolt_reset"}),
        "bolt_session_result",
    )
    .await;
    run(&state, id, "UNWIND range(1,1000) AS n RETURN n", json!({})).await;
    assert_eq!(
        request(
            &state,
            id,
            json!({"type":"bolt_discard"}),
            "bolt_result_page"
        )
        .await["records"],
        json!([])
    );
    request(
        &state,
        id,
        json!({"type":"bolt_logoff"}),
        "bolt_authentication",
    )
    .await;
    request(
        &state,
        id,
        json!({"type":"bolt_login","username":"neo4j","password":PASSWORD}),
        "bolt_authentication",
    )
    .await;
    run(
        &state,
        id,
        "RETURN $private AS reflected",
        json!({"private":PASSWORD}),
    )
    .await;
    assert_eq!(
        pull(&state, id, 1).await["records"],
        json!([["<redacted>"]])
    );
    let logs = state
        .list_access_logs_for(
            Some(netget::state::AccessLogOwner::Client(id.as_u32())),
            None,
        )
        .await;
    assert!(!serde_json::to_string(&logs).unwrap().contains(PASSWORD));
    state.remove_client(id).await;
    let authenticated = client(
        &state,
        addr.clone(),
        json!({"username":"neo4j","password":PASSWORD}),
        vec![static_handler("*", json!([]))],
    )
    .await;
    assert_eq!(
        event(&state, authenticated, "bolt_connected", 0).await.1["authentication_verified"],
        true
    );
    state.remove_client(authenticated).await;
    let error = refused_start(
        &state,
        addr,
        json!({"username":"neo4j","password":"wrong-password"}),
    )
    .await;
    assert!(error.contains("startup authentication refused"));
    assert!(!error.contains("wrong-password"));
    drop(peer);
    Ok(())
}

#[tokio::test]
async fn native_bolt_tls_refuses_an_independent_untrusted_certificate(
) -> crate::helpers::E2EResult<()> {
    let hint = InstallHint {
        brew: "openssl@3",
        apt: "openssl",
    };
    let peer = RealServer::builder("openssl", hint)
        .setup_command(
            "openssl",
            hint,
            [
                "req",
                "-x509",
                "-newkey",
                "rsa:2048",
                "-nodes",
                "-keyout",
                "{dir}/key.pem",
                "-out",
                "{dir}/cert.pem",
                "-subj",
                "/CN=localhost",
                "-days",
                "1",
            ],
        )
        .args([
            "s_server",
            "-accept",
            "127.0.0.1:{port}",
            "-key",
            "{dir}/key.pem",
            "-cert",
            "{dir}/cert.pem",
            "-www",
        ])
        .ready_when_log_matches("ACCEPT")
        .start()
        .await?;
    let state = state();
    let error = refused_start(
        &state,
        format!("bolt+s://{}", peer.addr()),
        json!({"request_timeout_secs":2,"username":"neo4j","password":PASSWORD}),
    )
    .await;
    assert!(error.contains("TLS verification failed"), "{error}");
    assert!(!error.contains(PASSWORD));
    Ok(())
}
