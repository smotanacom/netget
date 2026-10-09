//! `append_to_log`'s `output_name` is a filename component, so it is an allowlist.
//!
//! The name is model-authored and offered on network events, and it became
//! `netget_<name>_<time>_s<id>.log` unchecked. The prefix and suffix meant `..` could only
//! escape through a directory literally named `netget_<x>` in the working directory — one
//! `mkdir` away from appending model text into any file the process can write. The name is
//! refused rather than rewritten, as a database name is, so the model's own bookkeeping
//! matches what exists on disk.

use netget::state::server::{validate_log_output_name, MAX_LOG_OUTPUT_NAME_LEN};
use netget::state::{ServerId, ServerInstance};

#[test]
fn a_name_that_could_steer_the_path_is_refused() {
    for bad in [
        "",
        "../x",
        "a/../../etc/cron.d/job",
        "a\\b",
        "x\u{0}y",
        "access logs",
        "logs.txt",
        "名前",
        &"a".repeat(MAX_LOG_OUTPUT_NAME_LEN + 1),
    ] {
        let err = validate_log_output_name(bad).expect_err(&format!("{bad:?} must be refused"));
        assert!(
            err.to_string().contains("output_name"),
            "the refusal should name the parameter: {err}"
        );
    }
    for good in [
        "access_logs",
        "audit-trail",
        "A1",
        &"a".repeat(MAX_LOG_OUTPUT_NAME_LEN),
    ] {
        validate_log_output_name(good).unwrap_or_else(|e| panic!("{good:?}: {e}"));
    }
}

#[test]
fn a_server_builds_a_path_for_a_good_name_and_none_for_a_bad_one() {
    let mut server = ServerInstance::new(ServerId::new(7), 0, "tcp".to_string(), String::new());
    let path = server
        .get_or_create_log_path("access_logs")
        .expect("a plain name");
    let shown = path.to_string_lossy();
    assert!(
        shown.starts_with("netget_access_logs_") && shown.ends_with("_s7.log"),
        "{shown}"
    );
    assert_eq!(
        path.components().count(),
        1,
        "one component, no directories: {shown}"
    );

    let err = server
        .get_or_create_log_path("../../tmp/evil")
        .expect_err("a traversal must be refused");
    assert!(err.to_string().contains("output_name"), "{err}");
}
