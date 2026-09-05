//! `--ollama-lock` does nothing, and the help text has to keep saying so.
//!
//! The flag is parsed into `Args::ollama_lock` and **nothing reads it**. That is now literally
//! true rather than effectively true: the plumbing it used to feed — an `ollama_lock_enabled`
//! field on `AppStateInner`, `AppState::get_ollama_lock_enabled()`, a parameter on
//! `AppState::new_with_options` and on `create_llm_client`, and `OllamaClient::new_with_options`
//! whose body was `Self::new(url)` — has been deleted. Each hop carried a boolean that could not
//! change any behaviour while making the flag look implemented to anyone who followed it.
//!
//! The flag stays accepted so `--ollama-lock` in an existing script is not a hard clap error.
//!
//! Its old help text promised the opposite, specifically enough to be checkable: "prevents
//! concurrent requests from overloading the LLM, allowing multiple NetGet instances to run
//! safely in parallel. The lock file is created at ./ollama.lock in the current directory."
//! No such file is ever created. And because `tests/helpers/netget.rs` passes the flag to
//! every spawned binary, the entire e2e suite *looked* as though it serialised LLM access
//! across processes.
//!
//! This test exists so the claim cannot come back without the mechanism coming back with it.
//! If someone implements real cross-process locking, this file should fail — and the right
//! response is to delete it and pin the new behaviour instead.
//!
//! Run with:
//!   ./cargo-isolated.sh test --no-default-features --features tcp --test ollama_lock_is_a_noop_test

use clap::CommandFactory;
use netget::cli::Args;

/// The flag's help text must not promise serialization it does not perform.
#[test]
fn the_help_text_does_not_promise_locking_it_does_not_do() {
    let cmd = Args::command();
    let arg = cmd
        .get_arguments()
        .find(|a| a.get_long() == Some("ollama-lock"))
        .expect("--ollama-lock is still a declared flag");

    let help = arg
        .get_help()
        .map(|h| h.to_string())
        .unwrap_or_default()
        .to_lowercase();

    assert!(
        help.contains("ignored") || help.contains("deprecated"),
        "--ollama-lock does nothing, so its help must say so; got: {help}"
    );

    // The specific false claims the old text made. Each one is checkable and each one is
    // untrue, which is why they are named individually rather than as a vague "no promises".
    // The specific claims the old text made, each checkable and each untrue. Phrased as
    // whole promises rather than fragments: an honest denial ("no lock file is created")
    // contains the fragment, so a fragment match would flag the correction itself.
    for lie in [
        "the lock file is created",
        "ollama.lock",
        "run safely in parallel",
        "prevents concurrent requests",
    ] {
        assert!(
            !help.contains(lie),
            "--ollama-lock's help still claims {lie:?}, which no code does. Either implement \
             cross-process locking or keep the text honest; do not describe a mechanism that \
             is not there."
        );
    }
}

/// Passing the flag must not create a lock file, because nothing takes a lock.
///
/// Asserted against a scratch directory rather than the repo, so a stale `ollama.lock` left
/// behind by something else cannot make this pass or fail for the wrong reason.
#[test]
fn constructing_a_client_with_locking_enabled_creates_no_lock_file() {
    let dir = std::env::temp_dir().join(format!("netget-ollama-lock-probe-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("scratch dir");

    let previous = std::env::current_dir().expect("cwd");
    std::env::set_current_dir(&dir).expect("chdir");

    // Parse the flag exactly as the binary does, then build a client the way the CLI now does.
    // There is no longer an API that takes a lock argument at all, which is the point.
    let args = <netget::cli::Args as clap::Parser>::parse_from(["netget", "--ollama-lock"]);
    assert!(args.ollama_lock, "--ollama-lock must still be accepted");
    let _client = netget::llm::OllamaClient::new("http://127.0.0.1:1".to_string());

    let lock_exists = std::path::Path::new("ollama.lock").exists();

    std::env::set_current_dir(previous).expect("restore cwd");
    let _ = std::fs::remove_dir_all(&dir);

    assert!(
        !lock_exists,
        "an ollama.lock appeared, so locking may now be real — good, but this test and the \
         flag's help text both describe it as a no-op and must be updated together"
    );
}

/// No code may read the flag again without re-introducing a mechanism.
///
/// The plumbing was deleted, so the honest guard is that `ollama_lock` is *written* by clap and
/// read nowhere. A source scan is the only way to say that: a runtime check cannot distinguish
/// "read and ignored" from "not read".
#[test]
fn nothing_in_src_reads_the_flag() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut offenders = Vec::new();

    fn walk(dir: &std::path::Path, out: &mut Vec<String>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_dir() {
                walk(&p, out);
            } else if p.extension().is_some_and(|e| e == "rs") {
                let src = std::fs::read_to_string(&p).unwrap_or_default();
                for (i, line) in src.lines().enumerate() {
                    let code = line.split("//").next().unwrap_or("");
                    if code.contains("ollama_lock") && !code.contains("pub ollama_lock: bool") {
                        out.push(format!("{}:{}: {}", p.display(), i + 1, line.trim()));
                    }
                }
            }
        }
    }
    walk(&root, &mut offenders);

    assert!(
        offenders.is_empty(),
        "`ollama_lock` is read somewhere in src/ again:\n{}\n\nThe flag is documented as \
         deprecated and ignored, and its whole plumbing was removed. If locking is being \
         implemented for real, update the help text and this file together — do not thread a \
         boolean back through that changes nothing.",
        offenders.join("\n")
    );
}
