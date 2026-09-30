# hyper 1.7.0, patched for wasm32-unknown-unknown

This is hyper **1.7.0** exactly as published on crates.io (the `.crate` whose sha256,
`eb3aa54a13a0dfe7fbe3a59e0c76093041720fdc77b110cc0fc260fafb4dc51e`, is the checksum
Cargo.lock recorded for it; upstream commit `400bdfda`), with one change. The root
`Cargo.toml` points `[patch.crates-io] hyper` here. Only `Cargo.toml`, `LICENSE` and `src/`
are kept — the published manifest declares no tests, benches or examples. This README is the
only added file.

## Why

hyper's HTTP/1 server dispatcher calls `T::update_date()` at the top of every poll
(`src/proto/h1/dispatch.rs` → `src/proto/h1/role.rs` → `src/common/date.rs`), whatever
`auto_date_header` says, and the HTTP/2 server does the same through
`date::update_and_header_value()` when its `date_header` is on. Both reach
`std::time::SystemTime::now()`, which **panics on wasm32-unknown-unknown** — std has no clock
there. NetGet's browser build (`crates/netget-web`, netget.net's demo) compiles ~20 hyper
servers (`http`, `openapi`, `jsonrpc`, `rss`, `oauth2`, `ollama`, `npm`, …), and the first
request to any of them aborted the whole page with `RuntimeError: unreachable`.

## The change

On `cfg(all(target_arch = "wasm32", target_os = "unknown"))` only, the date cache reads the
page's wall clock from JavaScript's `Date.now()` through `js-sys`, so the `Date` header stays
correct. On every other target the compiled code is upstream's, token for token: each added
line is either behind that cfg or is the `not(...)` twin of an upstream line it leaves in
place. wasm32-wasi and emscripten have a working `SystemTime::now()` and keep it.

```diff
--- a/src/common/date.rs
+++ b/src/common/date.rs
@@ -43,6 +43,14 @@ struct CachedDate {
 
 thread_local!(static CACHED: RefCell<CachedDate> = RefCell::new(CachedDate::new()));
 
+// NetGet patch (vendor/hyper/README.md): `SystemTime::now()` panics on
+// wasm32-unknown-unknown, which has no clock in std. The page's wall clock is
+// JavaScript's `Date.now()`, milliseconds since the Unix epoch.
+#[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
+fn wasm_now() -> SystemTime {
+    UNIX_EPOCH + Duration::from_millis(js_sys::Date::now() as u64)
+}
+
 impl CachedDate {
     fn new() -> Self {
         let mut cache = CachedDate {
@@ -50,7 +58,10 @@ impl CachedDate {
             pos: 0,
             #[cfg(feature = "http2")]
             header_value: HeaderValue::from_static(""),
+            #[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
             next_update: SystemTime::now(),
+            #[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
+            next_update: wasm_now(),
         };
         cache.update(cache.next_update);
         cache
@@ -61,7 +72,10 @@ impl CachedDate {
     }
 
     fn check(&mut self) {
+        #[cfg(not(all(target_arch = "wasm32", target_os = "unknown")))]
         let now = SystemTime::now();
+        #[cfg(all(target_arch = "wasm32", target_os = "unknown"))]
+        let now = wasm_now();
         if now > self.next_update {
             self.update(now);
         }
--- a/Cargo.toml
+++ b/Cargo.toml
@@ -186,6 +186,11 @@ default-features = false
 version = "0.3"
 optional = true
 
+# NetGet patch (vendor/hyper/README.md): the wall clock for src/common/date.rs
+# on wasm32-unknown-unknown, where `SystemTime::now()` panics.
+[target.'cfg(all(target_arch = "wasm32", target_os = "unknown"))'.dependencies.js-sys]
+version = "0.3"
+
 [dev-dependencies.form_urlencoded]
 version = "1"
```

`UNIX_EPOCH + Duration` and `SystemTime` comparison are plain arithmetic and work on wasm32;
only `now()` panics. `js-sys` is the version the browser build already links.

Nothing else in hyper reads the clock on a path NetGet uses: `Instant::now()` in
`proto/h1/conn.rs` runs only with a header-read timeout *and* a `Timer`, and in
`proto/h2/ping.rs` only with keep-alive or an adaptive window — NetGet's hyper servers set
none of them.

A path patch needs no `.cargo-checksum.json`; that is for `cargo vendor`'s directory sources.

## Re-applying it on a hyper upgrade

`tests/vendored_hyper_patch_test.rs` fails when Cargo.lock's hyper 1.x is not this copy at this
version (including when Cargo lists the patch under `[[patch.unused]]` because something
needs a newer hyper), and when `date.rs` calls `SystemTime::now()` without the wasm32 twin.
To move to hyper X.Y.Z:

1. `cargo update -p hyper --precise X.Y.Z` will not work while the patch pins the old copy.
   Fetch the new source instead — e.g. temporarily remove the `[patch.crates-io]` entry,
   `cargo update -p hyper --precise X.Y.Z`, `cargo fetch` — and copy
   `~/.cargo/registry/src/*/hyper-X.Y.Z/{Cargo.toml,LICENSE,src}` over this directory
   (delete `src/` first so removed files go). Check the `.crate`'s sha256 against the checksum
   Cargo.lock recorded for it before the patch goes back.
2. Commit that tree unmodified, on its own, so the patch is a readable diff against it.
3. Re-apply the diff above (read `src/common/date.rs` first: if upstream stopped calling
   `SystemTime::now()`, or added another call, adapt it — the test checks every call), put
   this README's version and checksum right, restore `[patch.crates-io]`, and let Cargo
   rewrite Cargo.lock.
4. `grep -rn 'SystemTime::now\|Instant::now' src` for any new clock read on a server path.
5. `cargo test --test vendored_hyper_patch_test`, then `./web/build.sh && node web/test/smoke.mjs`
   — the smoke test sends real requests to the hyper servers in the bundle and checks their
   `Date` headers.

If hyper upstream ever reads the clock through something wasm-safe, delete this directory and
the `[patch.crates-io]` entry instead.
