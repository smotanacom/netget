//! Independent A2S clients against NetGet's server, each failing rather than skipping when
//! absent: python-a2s 1.4.2 and woozymasta/a2s v0.4.0's client (Go, through
//! `tests/client/a2s/peer`). Both handle the challenge and reassemble the split rules answer.
//! `tests/client/a2s/install_peers.py` installs them and prints NETGET_A2S_PYTHON and
//! NETGET_A2S_PEER.
use super::wire_test::{handlers, start};
use serde_json::{json, Value};
use std::{path::PathBuf, time::Duration};

fn env_path(var: &str) -> PathBuf {
    std::env::var_os(var).map(PathBuf::from).unwrap_or_else(|| panic!("{var} is required: run tests/client/a2s/install_peers.py <root> and export what it prints"))
}

async fn run(program: PathBuf, args: &[&str]) -> Value {
    let out = tokio::time::timeout(
        Duration::from_secs(60),
        tokio::process::Command::new(&program)
            .args(args)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("peer deadline")
    .unwrap_or_else(|e| panic!("start {}: {e}", program.display()));
    let text = String::from_utf8_lossy(&out.stdout).to_string();
    serde_json::from_str(text.trim().lines().last().unwrap_or_default())
        .unwrap_or_else(|e| panic!("{e}: {text}{}", String::from_utf8_lossy(&out.stderr)))
}

const PYTHON_CLIENT: &str = "import a2s, json, sys\naddr = ('127.0.0.1', int(sys.argv[1]))\ni = a2s.info(addr, timeout=5)\np = a2s.players(addr, timeout=5)\nr = a2s.rules(addr, timeout=5)\nprint(json.dumps({'name': i.server_name, 'map': i.map_name, 'folder': i.folder, 'game': i.game, 'app_id': i.app_id, 'players': i.player_count, 'max': i.max_players, 'bots': i.bot_count, 'vac': i.vac_enabled, 'version': i.version, 'port': i.port, 'keywords': i.keywords, 'player_names': [x.name for x in p], 'scores': [x.score for x in p], 'rules': len(r), 'last_rule': r.get('rule_149')}))";

#[tokio::test]
async fn python_a2s_reads_info_players_and_split_rules() {
    for info_challenge in [false, true] {
        let (state, id, addr) = start(handlers(), json!({"info_challenge": info_challenge})).await;
        let out = run(
            env_path("NETGET_A2S_PYTHON"),
            &["-I", "-c", PYTHON_CLIENT, &addr.port().to_string()],
        )
        .await;
        assert_eq!(
            out,
            json!({"name":"NetGet Arena","map":"de_dust2","folder":"csgo","game":"Counter-Strike","app_id":730,"players":2,"max":16,"bots":1,"vac":true,"version":"1.38.8.1","port":27015,"keywords":"netget,test","player_names":["alice","bob"],"scores":[12,-1],"rules":150,"last_rule":"value_149"}),
            "info_challenge={info_challenge}"
        );
        state.remove_server(id).await;
    }
}

#[tokio::test]
async fn woozymasta_client_reads_info_players_and_split_rules() {
    let (state, id, addr) = start(handlers(), json!({"info_challenge": true})).await;
    let out = run(
        env_path("NETGET_A2S_PEER"),
        &["client", &addr.to_string(), "info", "players", "rules"],
    )
    .await;
    assert!(
        out.get("info_error").is_none()
            && out.get("players_error").is_none()
            && out.get("rules_error").is_none(),
        "{out}"
    );
    assert_eq!(
        (
            out["info"]["name"].clone(),
            out["info"]["map"].clone(),
            out["info"]["app_id"].clone()
        ),
        (json!("NetGet Arena"), json!("de_dust2"), json!(730))
    );
    assert_eq!(
        (out["info"]["port"].clone(), out["info"]["vac"].clone()),
        (json!(27015), json!(true))
    );
    assert_eq!(
        out["players"][0],
        json!({"name":"alice","score":12,"seconds":330.5})
    );
    assert_eq!(out["rules"].as_object().unwrap().len(), 150);
    assert_eq!(out["rules"]["rule_000"], "value_000");
    state.remove_server(id).await;
}
