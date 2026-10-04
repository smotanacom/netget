//! GraphQL fixtures: a bookstore schema and handler policy, server and client through the shared
//! forms, the pinned gql/strawberry peers and access-log waits.
// Shared by the server and client test binaries; each uses a different subset.
#![allow(dead_code)]
use netget::{
    cli::management::{ClientForm, ServerForm},
    state::{app_state::AppState, AccessLogOwner, ClientId, ClientStatus, ServerId},
};
use serde_json::{json, Value};
use std::{net::SocketAddr, path::PathBuf, time::Duration};
use tokio::sync::mpsc;

pub(crate) fn state() -> AppState {
    AppState::new_with_options(false, "http://127.0.0.1:1".into())
}

pub(crate) const BOOK_SCHEMA: &str = "type Query { hello(name: String): String! book(id: ID!): Book search(term: String!): [SearchResult!]! secret: String } type Mutation { addBook(title: String!, year: Int): Book! } type Book { id: ID! title: String! year: Int author: Author! } type Author { name: String! } union SearchResult = Book | Author type Subscription { countdown(from: Int!): Int! bookAdded: Book! forbidden: String }";

/// Answers every root field from a small catalogue: book "404" is a field error, `secret`
/// refuses the whole operation, `addBook` echoes its arguments as book 3. Subscriptions:
/// `countdown(from)` answers every event at once and completes, `bookAdded` waits for events
/// pushed with `send_to_peer`, `forbidden` is refused.
pub(crate) fn book_policy() -> Vec<Value> {
    vec![
        json!({"event_pattern":"graphql_operation","handler":{"type":"script","language":"python","code":concat!(
            "import json,sys\n",
            "e=json.load(sys.stdin)['event']\n",
            "books={'1':{'id':'1','title':'Dune','year':1965,'author':{'name':'Frank Herbert'}},'2':{'id':'2','title':'Emma','year':1815,'author':{'name':'Jane Austen'}}}\n",
            "data={}; errors=[]; refuse=False\n",
            "for f in e['root_fields']:\n",
            "    k=f['response_key']; a=f['arguments']; n=f['field']\n",
            "    if n=='hello': data[k]='Hello, '+(a.get('name') or 'world')\n",
            "    elif n=='book':\n",
            "        if a['id']=='404': data[k]=None; errors.append({'message':'book 404 is gone','path':[k]})\n",
            "        else: data[k]=books.get(a['id'])\n",
            "    elif n=='search': data[k]=[dict(books['1'],__typename='Book'),{'__typename':'Author','name':'Frank Herbert'}]\n",
            "    elif n=='addBook': data[k]={'id':'3','title':a['title'],'year':a.get('year'),'author':{'name':'Anonymous'}}\n",
            "    elif n=='secret': refuse=True\n",
            "if refuse: a={'type':'graphql_error','message':'not authorized','extensions':{'code':'FORBIDDEN'}}\n",
            "else: a={'type':'graphql_result','data':data,'errors':errors}\n",
            "print(json.dumps({'actions':[a]}))\n"
        )}}),
        json!({"event_pattern":"graphql_subscription_start","handler":{"type":"script","language":"python","code":concat!(
            "import json,sys\n",
            "e=json.load(sys.stdin)['event']\n",
            "f=e['root_fields'][0]; k=f['response_key']\n",
            "if f['field']=='countdown': acts=[{'type':'graphql_event','data':{k:i}} for i in range(f['arguments']['from'],0,-1)]+[{'type':'graphql_complete'}]\n",
            "elif f['field']=='forbidden': acts=[{'type':'graphql_error','message':'not authorized'}]\n",
            "else: acts=[]\n",
            "print(json.dumps({'actions':acts}))\n"
        )}}),
    ]
}

pub(crate) async fn server_in(
    state: &AppState,
    handlers: Vec<Value>,
    params: Value,
) -> (ServerId, SocketAddr) {
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move {
        while let Some(m) = rx.recv().await {
            if std::env::var("GQL_DEBUG").is_ok() {
                eprintln!("STATUS {m}");
            }
        }
    });
    let id = ServerForm {
        protocol: "graphql".into(),
        host: Some("127.0.0.1".into()),
        port: Some(0),
        instruction: Some(String::new()),
        event_handlers: Some(handlers),
        startup_params: Some(params),
        ..Default::default()
    }
    .create(state, tx)
    .await
    .unwrap();
    let addr = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(addr) = state.get_server(id).await.and_then(|s| s.local_addr) {
                break addr;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .unwrap();
    (id, addr)
}

pub(crate) async fn client_in(
    state: &AppState,
    remote: String,
    params: Value,
) -> anyhow::Result<ClientId> {
    let (tx, mut rx) = mpsc::unbounded_channel::<String>();
    tokio::spawn(async move { while rx.recv().await.is_some() {} });
    let handlers = vec![
        json!({"event_pattern":"graphql_connected","handler":{"type":"static","actions":[]}}),
        json!({"event_pattern":"graphql_response","handler":{"type":"static","actions":[]}}),
        json!({"event_pattern":"graphql_subscription_event","handler":{"type":"static","actions":[]}}),
        json!({"event_pattern":"graphql_subscription_error","handler":{"type":"static","actions":[]}}),
        json!({"event_pattern":"graphql_subscription_complete","handler":{"type":"static","actions":[]}}),
    ];
    let id = ClientForm {
        protocol: "graphql".into(),
        remote_addr: Some(remote),
        instruction: Some(String::new()),
        event_handlers: Some(handlers),
        startup_params: Some(params),
        ..Default::default()
    }
    .create(
        state,
        netget::llm::OllamaClient::new("http://127.0.0.1:1"),
        tx,
    )
    .await?;
    tokio::time::timeout(Duration::from_secs(15), async {
        while !state.has_client_handle(id).await {
            if let Some(ClientStatus::Error(e)) = state.get_client(id).await.map(|c| c.status) {
                anyhow::bail!(e);
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        Ok(())
    })
    .await
    .map_err(|_| anyhow::anyhow!("GraphQL client did not connect"))??;
    Ok(id)
}

pub(crate) async fn logs(
    state: &AppState,
    owner: AccessLogOwner,
    kind: &str,
    count: usize,
) -> Vec<netget::state::app_state::AccessLogEntry> {
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            let mut rows = state
                .list_access_logs_for(Some(owner), None)
                .await
                .into_iter()
                .filter(|e| e.event_type == kind)
                .collect::<Vec<_>>();
            if rows.len() >= count {
                rows.sort_by_key(|e| e.id);
                break rows;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("timed out waiting for {count} {kind} access-log entries"))
}

pub(crate) fn python() -> String {
    std::env::var("NETGET_GRAPHQL_PYTHON").expect("NETGET_GRAPHQL_PYTHON must name the Python from tests/server/graphql/install_peers.py (gql 4.4.0, strawberry-graphql 0.330.2); this evidence never skips")
}
pub(crate) fn peer_script() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/server/graphql/peer.py")
}
