use crate::helpers::p2p as h;
use serde_json::json;
#[tokio::test]
async fn independent_ncdc_uploader() {
    let (mut peer, addr, meta) = h::peer_server_details("adc_peer").await;
    let s = h::state();
    let params = json!({"cid":meta["cid"],"token":meta["token"]});
    let id = h::client_in(&s, addr.to_string(), "adc_peer", h::quiet(), params).await;
    let id = match id {
        Ok(id) => id,
        Err(e) => {
            drop(peer.stdin.take());
            let _ = peer.wait().await;
            panic!("connect: {e}");
        }
    };
    let result = h::send(
        &s,
        id,
        json!({"type":"adc_peer_get","identifier":"files.xml.bz2","offset":0,"length":-1}),
    )
    .await;
    s.remove_client(id).await;
    drop(peer.stdin.take());
    assert!(peer.wait().await.unwrap().success());
    assert!(
        result["file_list_xml"]
            .as_str()
            .unwrap()
            .contains("hello.txt"),
        "{result}"
    );
}
