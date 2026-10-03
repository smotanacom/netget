use super::e2e_test::{logs, start};
use serde_json::json;
use std::time::Duration;
#[tokio::test]
async fn pinned_pygelf_emitters_deliver_udp_gzip_chunks_and_tcp_nul_messages() {
    for transport in ["udp", "tcp"] {
        let (state, id, addr) = start(None, Some(json!({"transport":transport}))).await;
        let script = r#"
import logging,sys
from pygelf import GelfTcpHandler,GelfUdpHandler
transport=sys.argv[1]
handler=(GelfUdpHandler(host='127.0.0.1',port=int(sys.argv[2]),chunk_size=80) if transport=='udp' else GelfTcpHandler(host='127.0.0.1',port=int(sys.argv[2])))
handler.additional_fields={'_service':'pygelf','_count':42}
logger=logging.getLogger('netget-peer');logger.addHandler(handler);logger.setLevel(logging.INFO)
logger.info('independent 温度 '+'0123456789abcdefghijklmnopqrstuvwxyz '*100)
handler.close()
"#;
        let mut command = tokio::process::Command::new(
            std::env::var("NETGET_GELF_PYTHON").unwrap_or_else(|_| "python3".into()),
        );
        command
            .args(["-c", script, transport, &addr.port().to_string()])
            .kill_on_drop(true);
        let result = tokio::time::timeout(Duration::from_secs(15), command.output())
            .await
            .unwrap()
            .unwrap();
        assert!(
            result.status.success(),
            "pygelf peer absent/failed; install pinned peers: {}",
            String::from_utf8_lossy(&result.stderr)
        );
        let entries = logs(&state, id, 1).await;
        let m = &entries[0].request["message"];
        assert_eq!(entries[0].event_type, "gelf_message");
        assert!(m["short_message"]
            .as_str()
            .unwrap()
            .starts_with("independent 温度"));
        assert_eq!(m["level"], 6);
        assert_eq!(m["additional_fields"]["service"], "pygelf");
        assert_eq!(m["additional_fields"]["count"], 42);
        state.remove_server(id).await;
    }
}
