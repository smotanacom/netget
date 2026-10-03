use super::common::*;
use crate::helpers::real_server::{InstallHint, RealServer};
use netget::cli::management::ClientForm;
use serde_json::json;
#[tokio::test]
async fn agate_text_input_redirect_errors_and_certificate_validation(
) -> crate::helpers::E2EResult<()> {
    let certs = tempfile::tempdir()?;
    let (pem, cert, key, _) = certificate();
    std::fs::write(certs.path().join("cert.der"), cert)?;
    std::fs::write(certs.path().join("key.der"), key)?;
    let binary = std::env::var("NETGET_TEST_AGATE").unwrap_or_else(|_| "agate".into());
    let server=RealServer::builder(&binary,InstallHint{brew:"Agate (use scripts/test-peers/fetch-agate.sh)",apt:"Agate (use scripts/test-peers/fetch-agate.sh)"})
        .config_file("content/index.gmi","# Independent capsule\n=> /about.gmi About\n* Item\n> Quote\n```art\n /\\_/\\\n```\n")
        .config_file("content/about.gmi","About the independent server.\n")
        .config_file("content/.meta","input: 10 Search\nsecret: 11 Password\nredirect: 31 /about.gmi\nslow: 44 7\nidentity: 60 Client certificate required\n")
        .args(vec!["--addr".into(),"127.0.0.1:{port}".into(),"--content".into(),"{dir}/content".into(),"--certs".into(),certs.path().display().to_string(),"--hostname".into(),"localhost".into()]).start().await?;
    let state = state();
    let id = client(&state, server.addr(), &pem, json!([])).await;
    let port = server.addr().rsplit(':').next().unwrap().to_string();
    let url = |path: &str| format!("gemini://localhost:{port}/{path}");
    let home = request(&state, id, json!({"url":url("")})).await;
    assert_eq!(home["status"], 20);
    assert_eq!(home["lines"][0]["text"], "Independent capsule");
    assert_eq!(home["lines"][1]["url"], url("about.gmi"));
    assert_eq!(
        request(&state, id, json!({"url":url("input")})).await["kind"],
        "input"
    );
    assert_eq!(
        request(&state, id, json!({"url":url("secret")})).await["sensitive"],
        true
    );
    let redirect = request(&state, id, json!({"url":url("redirect")})).await;
    assert_eq!(redirect["url"], url("about.gmi"));
    assert!(
        request(&state, id, json!({"url":redirect["url"]})).await["text"]
            .as_str()
            .unwrap()
            .contains("independent")
    );
    assert_eq!(
        request(&state, id, json!({"url":url("slow")})).await["retry_after_secs"],
        7
    );
    assert_eq!(
        request(&state, id, json!({"url":url("identity")})).await["kind"],
        "certificate_required"
    );
    assert_eq!(
        request(&state, id, json!({"url":url("missing")})).await["kind"],
        "permanent_failure"
    );
    state.remove_client(id).await;
    for params in [
        json!({"server_name":"localhost"}),
        json!({"server_name":"wrong.example","custom_ca_cert_pem":pem}),
    ] {
        let (tx, _) = tokio::sync::mpsc::unbounded_channel();
        let id = ClientForm {
            protocol: "gemini".into(),
            remote_addr: Some(server.addr()),
            startup_params: Some(params),
            ..Default::default()
        }
        .create(
            &state,
            netget::llm::OllamaClient::new("http://127.0.0.1:1"),
            tx,
        )
        .await?;
        let failure = error(&state, id).await;
        assert!(failure.contains("certificate"), "{failure}");
        state.remove_client(id).await;
    }
    Ok(())
}
