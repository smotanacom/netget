# The schema the Cap'n Proto RPC tests share: NetGet's server and client, pycapnp, and the
# Go peer (tests/client/capnp_rpc/peer, generated from a copy carrying Go annotations).
@0xd8f4c0e7a1b2c3d4;

enum Kind {
  file @0;
  directory @1;
  link @2;
}

struct Entry {
  name @0 :Text;
  size @1 :UInt64;
  kind @2 :Kind;
  tags @3 :List(Text);
  priority @4 :Int16 = 5;
  owner :group {
    uid @5 :UInt32;
    gid @6 :UInt32;
  }
  union {
    none @7 :Void;
    target @8 :Text;
    blob @9 :Data;
  }
  children @10 :List(Entry);
  scores @11 :List(Float64);
  hidden @12 :Bool = true;
}

interface Base {
  ping @0 () -> (pong :Text);
}

interface Directory extends(Base) {
  lookup @0 (name :Text) -> (entry :Entry);
  add @1 (a :Int32, b :Int32) -> (sum :Int64);
  store @2 (entry :Entry) -> (stored :Entry, count :UInt32);
  fail @3 (why :Text) -> ();
}
