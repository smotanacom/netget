use crate::helpers::diameter::{aa, client, logs, Peer, PeerKind};
use netget::state::{client_handles::ClientSendOutcome, AccessLogOwner};
use serde_json::json;
use std::time::Duration;
#[tokio::test]
async fn native_client_interoperates_with_both_unchanged_receivers_and_readback() {
    for kind in [PeerKind::Python, PeerKind::Go] {
        let peer = Peer::start(kind).await;
        let (state, id) = client(peer.addr.to_string(), None, None).await;
        assert!(state.has_client_handle(id).await);
        let owner = AccessLogOwner::Client(id.as_u32());
        for (index, (password, typ, code)) in [
            ("Correct", 1, 2001),
            ("wrong", 3, 4001),
            ("Correct", 3, 2001),
            ("", 2, 2001),
        ]
        .into_iter()
        .enumerate()
        {
            assert!(matches!(
                state
                    .send_to_client(id, aa(password, typ), Duration::from_secs(10))
                    .await
                    .unwrap(),
                ClientSendOutcome::Executed { .. }
            ));
            let rows = logs(&state, owner, "diameter_aa_result", index + 1).await;
            let row = &rows[index];
            assert_eq!(row.request["reply"]["result_code"], code);
            assert_eq!(row.request["reply"]["accepted"], code == 2001);
            assert_eq!(row.request["reply"]["stateless"], true);
            assert!(row.request["request"].get("password").is_none());
            assert_eq!(row.request["reply"]["service_type"], 1);
        }
        let injected = logs(&state, owner, "injected_action", 4).await;
        assert_eq!(injected[0].request["password"], "<redacted>");
        assert!(!serde_json::to_string(&injected)
            .unwrap()
            .contains("Correct"));
        assert!(matches!(
            state
                .send_to_client(id, json!({"type":"disconnect"}), Duration::from_secs(10))
                .await
                .unwrap(),
            ClientSendOutcome::Disconnected
        ));
        state.remove_client(id).await;
        peer.stop().await;
    }
}
