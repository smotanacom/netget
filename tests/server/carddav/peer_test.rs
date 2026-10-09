//! vdirsyncer 0.21.0, unchanged, against NetGet's CardDAV server (backed by the script
//! store): discovery, a download, a local new card and a local edit uploaded with
//! preconditions, a local delete propagated. Fails, never skips.
use crate::helpers::dav::*;
use serde_json::{json, Value};

const ADA: &str = "BEGIN:VCARD\r\nVERSION:3.0\r\nUID:ada\r\nFN:Ada Lovelace\r\nEMAIL:ada@example.com\r\nEND:VCARD\r\n";
const BOB: &str = "BEGIN:VCARD\r\nVERSION:3.0\r\nUID:bob\r\nFN:Bob\r\nEND:VCARD\r\n";

#[tokio::test(flavor = "multi_thread")]
async fn vdirsyncer_syncs_address_books_both_ways() {
    let state = state();
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("db.json");
    std::fs::write(&db, json!({"collections": {"contacts": {"displayname": "Contacts", "objects": {"ada.vcf": ADA, "bob.vcf": BOB}}}}).to_string()).unwrap();
    let (sid, addr) = server_in(
        &state,
        "carddav",
        store_policy("carddav", &db, "contacts"),
        json!({}),
    )
    .await;
    let sync = tempfile::tempdir().unwrap();
    let conf = vdirsyncer_config(sync.path(), "carddav", &format!("http://{addr}/"), "vcf");
    let (ok, log) = vdirsyncer(&conf, &["discover"]).await;
    assert!(ok, "discover failed:\n{log}");
    let (ok, log) = vdirsyncer(&conf, &["sync"]).await;
    assert!(ok, "sync failed:\n{log}");
    let local = sync.path().join("local/contacts");
    let mut files: Vec<String> = std::fs::read_dir(&local)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    files.sort();
    assert_eq!(files.len(), 2, "{files:?}");
    let ada = files
        .iter()
        .find(|f| {
            std::fs::read_to_string(local.join(f))
                .unwrap()
                .contains("UID:ada")
        })
        .unwrap()
        .clone();
    let bob = files.iter().find(|f| *f != &ada).unwrap().clone();
    std::fs::write(
        local.join(&ada),
        ADA.replace("FN:Ada Lovelace", "FN:Augusta Ada King"),
    )
    .unwrap();
    std::fs::remove_file(local.join(&bob)).unwrap();
    std::fs::write(
        local.join("cy.vcf"),
        "BEGIN:VCARD\r\nVERSION:4.0\r\nUID:cy\r\nFN:Cy\r\nEMAIL:cy@example.org\r\nEND:VCARD\r\n",
    )
    .unwrap();
    let (ok, log) = vdirsyncer(&conf, &["sync"]).await;
    assert!(ok, "second sync failed:\n{log}");
    let stored: Value = serde_json::from_str(&std::fs::read_to_string(&db).unwrap()).unwrap();
    let cards: Vec<String> = stored["collections"]["contacts"]["objects"]
        .as_object()
        .unwrap()
        .values()
        .map(|v| v.as_str().unwrap().to_owned())
        .collect();
    assert_eq!(cards.len(), 2, "{cards:?}");
    assert!(
        cards.iter().any(|c| c.contains("FN:Augusta Ada King")),
        "the edit was uploaded"
    );
    assert!(
        cards.iter().any(|c| c.contains("UID:cy")),
        "the new card was uploaded"
    );
    assert!(
        !cards.iter().any(|c| c.contains("UID:bob")),
        "the local delete reached the server"
    );
    state.remove_server(sid).await;
}
