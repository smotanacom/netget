//! 9P2000 server over raw messages: version negotiation, attach, full, partial and ".."
//! walks, reads at offsets, directory paging in whole entries, stat, create/write accepted
//! and refused, remove, the no-op wstat, and the bounds and fail-closed paths.
use netget::cli::management::ServerForm;
use netget::server::ninep::wire::{self, Reader, Writer};
use netget::state::{app_state::AppState, AccessLogOwner, ServerId};
use serde_json::{json, Value};
use std::{net::SocketAddr, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    sync::mpsc,
};

/// One script for every event: a small tree, /many with 300 files, /bin.dat as binary, and
/// changes accepted only under /scratch.
pub const TREE_SCRIPT: &str = r#"import json,sys
i=json.load(sys.stdin); t=i['event_type_id']; e=i['event']; p=e['path']
FILES={'/readme.txt':'hello from netget\n','/docs/guide.md':'# Guide\n'}
DIRS={'/':['readme.txt','docs','scratch','many','bin.dat','readonly.txt'],'/docs':['guide.md'],'/scratch':[],'/many':['f%03d'%n for n in range(300)]}
def entry(p):
  if p in DIRS: return {'type':'ninep_entry','kind':'dir','mtime':1700000000}
  if p in FILES: return {'type':'ninep_entry','kind':'file','size':len(FILES[p]),'mtime':1700000000,'owner':'glenda'}
  if p=='/bin.dat': return {'type':'ninep_entry','kind':'file','size':3}
  if p.startswith('/scratch/') or p.startswith('/many/'): return {'type':'ninep_entry','kind':'file','size':0}
  return {'type':'ninep_not_found'}
def child(d,n):
  a=entry(('' if d=='/' else d)+'/'+n); a.pop('type'); a['name']=n
  if a.get('kind') is None: a['kind']='file'
  return a
if t=='ninep_stat': a=entry(p)
elif t=='ninep_list': a={'type':'ninep_listing','entries':[child(p,n) for n in DIRS[p]]}
elif t=='ninep_read': a={'type':'ninep_content','data':'00ff10','encoding':'hex'} if p=='/bin.dat' else {'type':'ninep_content','data':FILES.get(p,'')}
elif p.startswith('/scratch/'): a={'type':'ninep_ok'}
else: a={'type':'ninep_error','message':'permission denied'}
print(json.dumps({'actions':[a]}))"#;

pub fn handlers() -> Vec<Value> {
    vec![
        json!({"event_pattern":"*","handler":{"type":"script","language":"python","code":TREE_SCRIPT}}),
    ]
}

pub async fn start(handlers: Vec<Value>, params: Value) -> (AppState, ServerId, SocketAddr) {
    let state = AppState::new_with_options(false, "http://127.0.0.1:1".into());
    state
        .set_llm_client(netget::llm::OllamaClient::new("http://127.0.0.1:1"))
        .await;
    let (tx, _) = mpsc::unbounded_channel();
    let id = ServerForm {
        protocol: "9p".into(),
        port: Some(0),
        host: Some("127.0.0.1".into()),
        instruction: Some("Serve files".into()),
        startup_params: Some(params),
        event_handlers: Some(handlers),
        ..Default::default()
    }
    .create(&state, tx)
    .await
    .unwrap();
    let addr = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Some(a) = state.get_server(id).await.and_then(|s| s.local_addr) {
                break a;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .unwrap();
    (state, id, SocketAddr::from(([127, 0, 0, 1], addr.port())))
}

pub struct Raw {
    s: TcpStream,
    tag: u16,
}

impl Raw {
    pub async fn connect(addr: SocketAddr, msize: u32) -> Self {
        let mut raw = Raw {
            s: TcpStream::connect(addr).await.unwrap(),
            tag: 0,
        };
        let (k, body) = raw
            .call(wire::TVERSION, Writer::new().u32(msize).string("9P2000"))
            .await;
        assert_eq!(k, wire::RVERSION);
        let mut r = Reader::new(&body);
        assert_eq!(
            (r.u32().unwrap(), r.string().unwrap()),
            (msize, "9P2000".into())
        );
        let (k, _) = raw
            .call(
                wire::TATTACH,
                Writer::new()
                    .u32(1)
                    .u32(wire::NOFID)
                    .string("glenda")
                    .string(""),
            )
            .await;
        assert_eq!(k, wire::RATTACH);
        raw
    }

    pub async fn call(&mut self, kind: u8, body: Writer) -> (u8, Vec<u8>) {
        self.tag += 1;
        let tag = if kind == wire::TVERSION {
            wire::NOTAG
        } else {
            self.tag
        };
        self.s.write_all(&body.finish(kind, tag)).await.unwrap();
        let (k, t, body) =
            wire::read_message(&mut self.s, wire::MAX_MSIZE, Duration::from_secs(20))
                .await
                .unwrap()
                .expect("a reply, not EOF");
        assert_eq!(t, tag);
        (k, body)
    }

    /// The Rerror text, failing on anything else.
    pub async fn error(&mut self, kind: u8, body: Writer) -> String {
        let (k, body) = self.call(kind, body).await;
        assert_eq!(k, wire::RERROR, "expected Rerror for message {kind}");
        Reader::new(&body).string().unwrap()
    }

    pub async fn walk(&mut self, fid: u32, newfid: u32, names: &[&str]) -> (u8, Vec<u8>) {
        let mut w = Writer::new().u32(fid).u32(newfid).u16(names.len() as u16);
        for n in names {
            w = w.string(n);
        }
        self.call(wire::TWALK, w).await
    }

    pub async fn closed(&mut self) {
        let mut buf = [0u8; 64];
        match tokio::time::timeout(Duration::from_secs(5), self.s.read(&mut buf)).await {
            Ok(Ok(0)) | Ok(Err(_)) => {}
            Ok(Ok(n)) => panic!("expected a close, read {n} bytes"),
            Err(_) => panic!("the connection stayed open"),
        }
    }
}

fn qids(body: &[u8]) -> Vec<wire::Qid> {
    let mut r = Reader::new(body);
    (0..r.u16().unwrap()).map(|_| r.qid().unwrap()).collect()
}

fn read_data(body: &[u8]) -> Vec<u8> {
    let mut r = Reader::new(body);
    let n = r.u32().unwrap() as usize;
    r.take(n).unwrap().to_vec()
}

#[tokio::test]
async fn version_attach_walk_read_list_stat_and_changes() {
    let (state, id, addr) = start(handlers(), json!({})).await;
    // Version: msize is capped, a dialect is answered with plain 9P2000, anything else unknown.
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(
        &Writer::new()
            .u32(1 << 20)
            .string("9P2000.L")
            .finish(wire::TVERSION, wire::NOTAG),
    )
    .await
    .unwrap();
    let (_, _, body) = wire::read_message(&mut s, wire::MAX_MSIZE, Duration::from_secs(5))
        .await
        .unwrap()
        .unwrap();
    let mut r = Reader::new(&body);
    assert_eq!(
        (r.u32().unwrap(), r.string().unwrap()),
        (wire::MAX_MSIZE, "9P2000".into())
    );
    s.write_all(
        &Writer::new()
            .u32(8192)
            .string("XYZ")
            .finish(wire::TVERSION, wire::NOTAG),
    )
    .await
    .unwrap();
    let (_, _, body) = wire::read_message(&mut s, wire::MAX_MSIZE, Duration::from_secs(5))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(Reader::new(&body[4..]).string().unwrap(), "unknown");

    let mut c = Raw::connect(addr, 8192).await;
    assert_eq!(
        c.error(
            wire::TAUTH,
            Writer::new().u32(9).string("glenda").string("")
        )
        .await,
        "authentication not required"
    );
    // Full walk, partial walk (newfid not created), failed first element, and "..".
    let (k, body) = c.walk(1, 2, &["docs", "guide.md"]).await;
    assert_eq!(k, wire::RWALK);
    let q = qids(&body);
    assert_eq!(
        (q.len(), q[0].is_dir(), q[1].is_dir(), q[1].version),
        (2, true, false, 1_700_000_000)
    );
    let (k, body) = c.walk(1, 3, &["docs", "nope"]).await;
    assert_eq!((k, qids(&body).len()), (wire::RWALK, 1), "partial walk");
    assert_eq!(
        c.error(wire::TSTAT, Writer::new().u32(3)).await,
        "unknown fid"
    );
    assert_eq!(c.walk(1, 3, &["nope"]).await.0, wire::RERROR);
    let (_, body) = c.walk(1, 4, &["docs", ".."]).await;
    assert_eq!(qids(&body)[1].path, wire::qid_path("/"));
    // Reads at offsets.
    let (k, body) = c
        .call(wire::TOPEN, Writer::new().u32(2).u8(wire::OREAD))
        .await;
    assert_eq!(k, wire::ROPEN);
    let mut r = Reader::new(&body);
    r.qid().unwrap();
    assert_eq!(r.u32().unwrap(), 8192 - wire::IOHDRSZ, "iounit");
    let (_, body) = c
        .call(wire::TREAD, Writer::new().u32(2).u64(0).u32(4))
        .await;
    assert_eq!(read_data(&body), b"# Gu");
    let (_, body) = c
        .call(wire::TREAD, Writer::new().u32(2).u64(4).u32(100))
        .await;
    assert_eq!(read_data(&body), b"ide\n");
    let (_, body) = c
        .call(wire::TREAD, Writer::new().u32(2).u64(8).u32(100))
        .await;
    assert!(read_data(&body).is_empty());
    // Binary content arrives as the bytes the handler gave in hex.
    c.walk(1, 6, &["bin.dat"]).await;
    c.call(wire::TOPEN, Writer::new().u32(6).u8(wire::OREAD))
        .await;
    let (_, body) = c
        .call(wire::TREAD, Writer::new().u32(6).u64(0).u32(100))
        .await;
    assert_eq!(read_data(&body), vec![0x00, 0xff, 0x10]);
    // Directory paging: whole entries only, sequential offsets, 300 in total.
    c.walk(1, 5, &["many"]).await;
    c.call(wire::TOPEN, Writer::new().u32(5).u8(wire::OREAD))
        .await;
    let (mut offset, mut names) = (0u64, Vec::new());
    loop {
        let (k, body) = c
            .call(wire::TREAD, Writer::new().u32(5).u64(offset).u32(300))
            .await;
        assert_eq!(k, wire::RREAD);
        let data = read_data(&body);
        if data.is_empty() {
            break;
        }
        assert!(data.len() <= 300);
        offset += data.len() as u64;
        let mut r = Reader::new(&data);
        while !r.is_empty() {
            names.push(r.stat().unwrap().name);
        }
    }
    assert_eq!(
        (names.len(), names[0].as_str(), names[299].as_str()),
        (300, "f000", "f299")
    );
    assert_eq!(
        c.error(wire::TREAD, Writer::new().u32(5).u64(7).u32(300))
            .await,
        "bad offset in directory read"
    );
    // Stat.
    let (k, body) = c.call(wire::TSTAT, Writer::new().u32(4)).await;
    assert_eq!(k, wire::RSTAT);
    let (_, body) = c.call(wire::TSTAT, Writer::new().u32(2)).await;
    let mut r = Reader::new(&body);
    r.u16().unwrap();
    let st = r.stat().unwrap();
    assert_eq!(
        (
            st.name.as_str(),
            st.length,
            st.uid.as_str(),
            st.mode,
            st.mtime
        ),
        ("guide.md", 8, "glenda", 0o644, 1_700_000_000)
    );
    drop(k);
    // Create and write under /scratch; the write reaches the handler as text.
    c.walk(1, 7, &["scratch"]).await;
    let (k, _) = c
        .call(
            wire::TCREATE,
            Writer::new()
                .u32(7)
                .string("new.txt")
                .u32(0o644)
                .u8(wire::OWRITE),
        )
        .await;
    assert_eq!(k, wire::RCREATE);
    let (k, body) = c
        .call(
            wire::TWRITE,
            Writer::new().u32(7).u64(0).u32(11).bytes(b"hello 9p!\r\n"),
        )
        .await;
    assert_eq!((k, Reader::new(&body).u32().unwrap()), (wire::RWRITE, 11));
    let logged = state
        .list_access_logs_for(Some(AccessLogOwner::Server(id.as_u32())), None)
        .await
        .iter()
        .map(|e| serde_json::to_string(e).unwrap())
        .any(|e| e.contains("/scratch/new.txt") && e.contains(r#"hello 9p!\r\n"#));
    assert!(logged, "the write's path and data reached the handler");
    // Refusals carry the handler's text; a refused remove still clunks the fid.
    c.walk(1, 8, &["readme.txt"]).await;
    c.call(wire::TOPEN, Writer::new().u32(8).u8(wire::OWRITE))
        .await;
    assert_eq!(
        c.error(wire::TWRITE, Writer::new().u32(8).u64(0).u32(1).bytes(b"x"))
            .await,
        "permission denied"
    );
    assert_eq!(
        c.error(
            wire::TCREATE,
            Writer::new().u32(1).string("x").u32(0o644).u8(wire::OWRITE)
        )
        .await,
        "permission denied"
    );
    c.walk(1, 9, &["readme.txt"]).await;
    assert_eq!(
        c.error(wire::TREMOVE, Writer::new().u32(9)).await,
        "permission denied"
    );
    assert_eq!(
        c.error(wire::TCLUNK, Writer::new().u32(9)).await,
        "unknown fid"
    );
    // A wstat that changes nothing is answered without asking (the handler would refuse it).
    c.walk(1, 10, &["readme.txt"]).await;
    let noop = wire::encode_stat(&wire::Stat::dont_touch());
    let (k, _) = c
        .call(
            wire::TWSTAT,
            Writer::new().u32(10).u16(noop.len() as u16).bytes(&noop),
        )
        .await;
    assert_eq!(k, wire::RWSTAT);
    let rename = wire::encode_stat(&wire::Stat {
        name: "x.txt".into(),
        ..wire::Stat::dont_touch()
    });
    assert_eq!(
        c.error(
            wire::TWSTAT,
            Writer::new()
                .u32(10)
                .u16(rename.len() as u16)
                .bytes(&rename)
        )
        .await,
        "permission denied"
    );
    // A directory cannot be opened for writing.
    c.walk(1, 11, &["docs"]).await;
    assert_eq!(
        c.error(wire::TOPEN, Writer::new().u32(11).u8(wire::OWRITE))
            .await,
        "is a directory"
    );
    state.remove_server(id).await;
}

#[tokio::test]
async fn bounds_and_fail_closed() {
    let (state, id, addr) = start(handlers(), json!({"idle_timeout_secs": 1})).await;
    // Before Tversion nothing else is served.
    let mut s = TcpStream::connect(addr).await.unwrap();
    s.write_all(
        &Writer::new()
            .u32(1)
            .u32(wire::NOFID)
            .string("u")
            .string("")
            .finish(wire::TATTACH, 1),
    )
    .await
    .unwrap();
    let (k, _, _) = wire::read_message(&mut s, wire::MAX_MSIZE, Duration::from_secs(5))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(k, wire::RERROR);
    // A message over the negotiated msize closes the connection before it is read.
    let mut c = Raw::connect(addr, 1024).await;
    c.s.write_all(&1025u32.to_le_bytes()).await.unwrap();
    c.closed().await;
    // More than 16 walk elements is malformed.
    let mut c = Raw::connect(addr, 8192).await;
    let names: Vec<&str> = std::iter::repeat_n("docs", 17).collect();
    let mut w = Writer::new().u32(1).u32(2).u16(17);
    for n in &names {
        w = w.string(n);
    }
    c.s.write_all(&w.finish(wire::TWALK, 5)).await.unwrap();
    c.closed().await;
    // 256 fids per connection.
    let mut c = Raw::connect(addr, 8192).await;
    for fid in 2..=wire::MAX_FIDS as u32 {
        let (k, _) = c.walk(1, fid, &[]).await;
        assert_eq!(k, wire::RWALK, "fid {fid}");
    }
    let (k, body) = c.walk(1, 9999, &[]).await;
    assert_eq!(
        (k, Reader::new(&body).string().unwrap()),
        (wire::RERROR, "too many fids".into())
    );
    // A silent connection is closed after idle_timeout_secs.
    let mut c = Raw::connect(addr, 8192).await;
    c.closed().await;
    state.remove_server(id).await;

    // Content over 1 MiB is an invalid answer; no handler at all is a model failure. Each is
    // answered with a generic Rerror, never a file.
    let huge = vec![
        json!({"event_pattern":"ninep_stat","handler":{"type":"static","actions":[{"type":"ninep_entry","kind":"file","size":1}]}}),
        json!({"event_pattern":"ninep_read","handler":{"type":"script","language":"python","code":"import json\nprint(json.dumps({'actions':[{'type':'ninep_content','data':'x'*(1024*1024+1)}]}))"}}),
    ];
    for handlers in [huge, vec![]] {
        let (state, id, addr) = start(handlers.clone(), json!({})).await;
        let mut c = Raw::connect(addr, 8192).await;
        if handlers.is_empty() {
            assert_eq!(
                c.error(wire::TWALK, Writer::new().u32(1).u32(2).u16(1).string("a"))
                    .await,
                "netget: request could not be processed"
            );
        } else {
            c.walk(1, 2, &["a"]).await;
            c.call(wire::TOPEN, Writer::new().u32(2).u8(wire::OREAD))
                .await;
            assert_eq!(
                c.error(wire::TREAD, Writer::new().u32(2).u64(0).u32(10))
                    .await,
                "netget: request could not be processed"
            );
        }
        state.remove_server(id).await;
    }
}
