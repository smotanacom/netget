//! NetGet's JMAP client against Stalwart 0.16.24 (independent mail server, unchanged), driven
//! by its handler: session discovery through Stalwart's redirect, Mailbox/get, an Email/set
//! create read back by creation id, a query feeding a get by result reference, changes since
//! the state before the create, a keyword update and an unknown method. Stalwart itself is
//! then asked what it holds. A wrong password never connects. Fails, never skips.
use crate::helpers::jmap::*;
use netget::state::AccessLogOwner;
use serde_json::{json, Value};

const DRIVER: &str = r#"import json,sys
i=json.load(sys.stdin); k=i['event_type_id']; e=i['event']
def out(*a): print(json.dumps({'actions':list(a)})); sys.exit()
def req(*calls): out({'type':'jmap_request','calls':list(calls)})
if k=='jmap_connected': req(['Mailbox/get',{},'0'])
R={r[2]:r for r in (e.get('method_responses') or [])}
if '0' in R:
    inbox=[m['id'] for m in R['0'][1]['list'] if m.get('role')=='inbox'][0]
    req(['Email/set',{'create':{'draft':{'mailboxIds':{inbox:True},'subject':'Hello from NetGet','keywords':{'$draft':True},'from':[{'email':'alice@test.local'}],'bodyValues':{'b':{'value':'hi'}},'textBody':[{'partId':'b','type':'text/plain'}]}}},'1'],
        ['Email/get',{'ids':['#draft'],'properties':['subject','keywords','mailboxIds']},'2'])
if '2' in R:
    inbox=list(R['2'][1]['list'][0]['mailboxIds'])[0]
    req(['Email/query',{'filter':{'inMailbox':inbox}},'3'],
        ['Email/get',{'#ids':{'resultOf':'3','name':'Email/query','path':'/ids'},'properties':['subject']},'4'],
        ['Email/changes',{'sinceState':R['1'][1]['oldState']},'5'])
if '4' in R:
    req(['Email/set',{'update':{R['4'][1]['list'][0]['id']:{'keywords/$seen':True}}},'6'],['Foo/bar',{},'7'])
out()
"#;

#[tokio::test(flavor = "multi_thread")]
async fn netget_against_stalwart() {
    let stalwart = Stalwart::start().await;
    let state = state();
    let remote = format!("127.0.0.1:{}", stalwart.port);
    let params = json!({"username": stalwart.user, "password": stalwart.password, "tls": false});
    let id = client_in(&state, remote.clone(), script(DRIVER), params.clone())
        .await
        .unwrap();
    let owner = AccessLogOwner::Client(id.as_u32());
    let responses = |e: &Value| -> Vec<Value> {
        e["method_responses"]
            .as_array()
            .cloned()
            .unwrap_or_default()
    };
    let find = |e: &Value, call: &str| responses(e).into_iter().find(|r| r[2] == call);

    let connected = wait_for(&state, owner, "jmap_connected", |_| true).await;
    assert_eq!(connected["username"], "alice@test.local", "{connected}");
    let created = wait_for(&state, owner, "jmap_response", |e| find(e, "2").is_some()).await;
    let set = find(&created, "1").unwrap();
    let new_id = set[1]["created"]["draft"]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    assert_eq!(
        find(&created, "2").unwrap()[1]["list"][0]["subject"],
        "Hello from NetGet",
        "{created}"
    );
    let queried = wait_for(&state, owner, "jmap_response", |e| find(e, "4").is_some()).await;
    assert_eq!(
        find(&queried, "4").unwrap()[1]["list"][0]["id"],
        new_id.as_str(),
        "{queried}"
    );
    let changes = find(&queried, "5").unwrap();
    assert!(
        changes[1]["created"]
            .as_array()
            .unwrap()
            .iter()
            .any(|c| c == new_id.as_str()),
        "{changes}"
    );
    let last = wait_for(&state, owner, "jmap_response", |e| find(e, "7").is_some()).await;
    assert!(
        find(&last, "6").unwrap()[1]["updated"]
            .get(new_id.as_str())
            .is_some(),
        "{last}"
    );
    assert_eq!(find(&last, "7").unwrap()[0], "error");
    assert_eq!(find(&last, "7").unwrap()[1]["type"], "unknownMethod");

    // Stalwart, asked directly, holds the email with the keyword the client set.
    let session: Value = reqwest::Client::new()
        .get(format!("http://{remote}/jmap/session"))
        .basic_auth(&stalwart.user, Some(&stalwart.password))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let account = session["primaryAccounts"]["urn:ietf:params:jmap:mail"].clone();
    let r: Value = reqwest::Client::new()
        .post(format!("http://{remote}/jmap/"))
        .basic_auth(&stalwart.user, Some(&stalwart.password))
        .json(&json!({"using": ["urn:ietf:params:jmap:core", "urn:ietf:params:jmap:mail"], "methodCalls": [["Email/get", {"accountId": account, "ids": [new_id], "properties": ["subject", "keywords"]}, "0"]]}))
        .send().await.unwrap().json().await.unwrap();
    let email = &r["methodResponses"][0][1]["list"][0];
    assert_eq!(email["subject"], "Hello from NetGet", "{r}");
    assert_eq!(email["keywords"]["$seen"], true, "{r}");
    assert_eq!(email["keywords"]["$draft"], true, "{r}");

    let mut wrong = params.clone();
    wrong["password"] = json!("nope");
    let refused = client_in(&state, remote, script(DRIVER), wrong)
        .await
        .unwrap_err();
    assert!(format!("{refused:#}").contains("401"), "{refused:#}");
}
