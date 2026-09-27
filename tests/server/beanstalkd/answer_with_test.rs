//! The `answer_with` hints `beanstalkd_reserve` and `beanstalkd_stats` carry.
//!
//! Two copied examples in the real-model eval: a reserve answered with the example body
//! `{"image": 7, "width": 640}` instead of the job's text, and a stats answered with the
//! example's own names, the 2 buried jobs filed under `current-jobs-reserved` because the
//! example had no `current-jobs-buried`. Both five runs in five.

use netget::server::beanstalkd::actions::{
    reserve_answer_with, stats_answer_with, BEANSTALKD_RESERVE_EVENT, BEANSTALKD_STATS_EVENT,
};

#[test]
fn a_reserve_names_the_watched_tubes_and_where_the_body_comes_from() {
    let hint = reserve_answer_with(&["images".to_string(), "default".to_string()]);
    assert!(hint.starts_with("reserve_beanstalkd_job"), "{hint}");
    assert!(hint.contains("(images, default)"), "{hint}");
    assert!(hint.contains("word for word"), "{hint}");
    assert!(hint.contains("wait_for_beanstalkd_job"), "{hint}");
}

#[test]
fn stats_carries_beanstalkds_own_names_for_its_scope() {
    let server = stats_answer_with("server");
    for name in [
        "current-jobs-ready",
        "current-jobs-buried",
        "current-jobs-delayed",
        "version",
    ] {
        assert!(server.contains(name), "missing {name}: {server}");
    }
    assert!(stats_answer_with("tube").contains("current-watching"));
    assert!(stats_answer_with("job").contains("time-left"));
    assert!(stats_answer_with("tubes").starts_with("send_beanstalkd_tubes"));
}

/// The examples a model reads carry no plausible job body or queue figures.
#[test]
fn the_examples_are_placeholders() {
    for event in [&*BEANSTALKD_RESERVE_EVENT, &*BEANSTALKD_STATS_EVENT] {
        let mut texts: Vec<String> = event
            .actions
            .iter()
            .map(|a| a.example.to_string())
            .collect();
        texts.push(event.effective_response_example().to_string());
        for text in texts {
            assert!(
                !text.contains("\"image\\\": 7") && !text.contains("resize image 7"),
                "{text}"
            );
            assert!(
                !text.contains("\"total-jobs\":42") && !text.contains("\"current-jobs-ready\":3"),
                "{text}"
            );
        }
    }
}
