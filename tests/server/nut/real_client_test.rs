//! Mandatory independent-peer test. Install NUT or set NUT_UPSC_BIN; absence fails.
use super::e2e_test::start;
use std::{path::PathBuf, time::Duration};
fn binary() -> PathBuf {
    std::env::var_os("NUT_UPSC_BIN").map(PathBuf::from).or_else(||crate::helpers::real_server::find_binary("upsc")).expect("NUT upsc is required: install nut (Homebrew) or nut-client (Debian), or set NUT_UPSC_BIN")
}
#[tokio::test]
async fn official_upsc_discovers_and_reads_programmable_ups() {
    let binary = binary();
    let (state, id, addr) = start(false, 300).await;
    for (args, expected) in [
        (vec!["-L".into(), addr.to_string()], "ups: Rack UPS"),
        (vec![format!("ups@{addr}"), "ups.status".into()], "OL"),
        (vec![format!("ups@{addr}")], "ups: OL"),
    ] {
        let mut command = tokio::process::Command::new(&binary);
        command.args(args).kill_on_drop(true);
        let output = tokio::time::timeout(Duration::from_secs(10), command.output())
            .await
            .expect("upsc deadline")
            .expect("start upsc");
        assert!(
            output.status.success(),
            "upsc failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(
            String::from_utf8_lossy(&output.stdout).contains(expected),
            "upsc parsed unexpected output: {}",
            String::from_utf8_lossy(&output.stdout)
        );
    }
    state.remove_server(id).await;
}

#[tokio::test]
async fn official_upscmd_authenticates_and_invokes_an_instant_command() {
    let binary = std::env::var_os("NUT_UPSCMD_BIN")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os("NUT_UPSC_BIN")
                .and_then(|p| PathBuf::from(p).parent().map(|dir| dir.join("upscmd")))
                .filter(|p| p.exists())
        })
        .or_else(|| crate::helpers::real_server::find_binary("upscmd"))
        .expect("NUT upscmd required: install nut/nut-client or set NUT_UPSCMD_BIN");
    let (state, id, addr) = start(true, 300).await;
    let output = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::process::Command::new(binary)
            .args([
                "-u",
                "operator",
                "-p",
                "synthetic-test-password",
                &format!("ups@{addr}"),
                "test.panel.start",
            ])
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("upscmd deadline")
    .expect("start upscmd");
    assert!(
        output.status.success(),
        "upscmd failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    state.remove_server(id).await;
}
