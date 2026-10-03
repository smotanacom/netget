use netget::client::prometheus::{
    exposition::{self, Format},
    request,
};
use serde_json::json;
fn parsed(body: &str, format: Format) -> serde_json::Value {
    exposition::parse(body.as_bytes(), format).unwrap()
}
#[test]
fn classic_types_labels_special_numbers_and_integer_millisecond_timestamps() {
    let value=parsed("# HELP requests_total Lines\\nwith \\\\ slash\n# TYPE requests_total counter\nrequests_total{path=\"C:\\\\logs \\\"primary\\\"\\nnext\",code=\"200\",} 3 1395066363000\n# TYPE temperature gauge\ntemperature -Inf -3982045\nplain NaN\n",Format::Text);
    assert_eq!(value["sample_count"], 3);
    let family = value["metrics"]
        .as_array()
        .unwrap()
        .iter()
        .find(|f| f["name"] == "requests_total")
        .unwrap();
    assert_eq!(family["type"], "counter");
    assert_eq!(family["help"], "Lines\nwith \\ slash");
    assert_eq!(
        family["samples"][0]["labels"]["path"],
        "C:\\logs \"primary\"\nnext"
    );
    assert_eq!(family["samples"][0]["timestamp_ms"], 1395066363000_i64);
    assert_eq!(family["samples"][0]["suffix"], "");
    assert!(value["metrics"].to_string().contains("-Inf"));
    assert!(value["metrics"].to_string().contains("NaN"));
}
#[test]
fn openmetrics_counter_family_units_created_exemplars_and_timestamp_seconds() {
    let v=parsed("# TYPE requests_seconds counter\n# UNIT requests_seconds seconds\n# HELP requests_seconds Escaped \\\"help\\\"\nrequests_seconds_total{method=\"GET\"} 3 1605281325.125 # {trace_id=\"abc\"} 2.5 1605281325.5\nrequests_seconds_created{method=\"GET\"} 1600000000\n# TYPE version info\nversion_info{version=\"1.2\"} 1\n# TYPE state stateset\nstate{state=\"ready\"} 1\n# EOF\n",Format::OpenMetrics);
    assert_eq!(v["sample_count"], 4);
    let f = &v["metrics"][0];
    assert_eq!(f["name"], "requests_seconds");
    assert_eq!(f["unit"], "seconds");
    assert_eq!(f["samples"][0]["name"], "requests_seconds_total");
    assert_eq!(f["samples"][0]["suffix"], "_total");
    assert_eq!(f["samples"][0]["timestamp_seconds"], 1605281325.125);
    assert_eq!(f["samples"][0]["exemplar"]["labels"]["trace_id"], "abc");
    assert_eq!(
        f["samples"][0]["exemplar"]["timestamp_seconds"],
        1605281325.5
    );
    assert!(f["samples"][0].get("timestamp_ms").is_none());
}
#[test]
fn histogram_summary_and_gauge_histogram_keep_sample_components() {
    let v=parsed("# TYPE h histogram\nh_bucket{le=\"1.0\"} 2\nh_bucket{le=\"+Inf\"} 3\nh_sum 4\nh_count 3\nh_created 100\n# TYPE s summary\ns{quantile=\"0.99\"} 0.2\ns_sum 3\ns_count 5\n# TYPE g gaugehistogram\ng_bucket{le=\"+Inf\"} 2\ng_gsum 4\ng_gcount 2\n# EOF\n",Format::OpenMetrics);
    assert_eq!(v["sample_count"], 11);
    assert_eq!(v["metrics"][1]["samples"].as_array().unwrap().len(), 5);
    assert_eq!(v["metrics"][0]["samples"][1]["suffix"], "_gsum");
}
#[test]
fn negotiation_request_and_content_type_validation() {
    assert_eq!(
        exposition::content_type("text/plain; version=0.0.4; charset=utf-8").unwrap(),
        Format::Text
    );
    assert_eq!(
        exposition::content_type("application/openmetrics-text;version=1.0.0;charset=\"UTF-8\"")
            .unwrap(),
        Format::OpenMetrics
    );
    for value in [
        "application/json",
        "text/plain;version=1.0.0",
        "text/plain;charset=ascii",
        "application/openmetrics-text;version=2.0.0",
        "text/plain;version=0.0.4;version=0.0.4",
    ] {
        assert!(exposition::content_type(value).is_err(), "{value}");
    }
    assert_eq!(
        request(&json!({"type":"scrape_metrics"}), "/custom?target=local")
            .unwrap()
            .path,
        "/custom?target=local"
    );
    for path in [
        "http://remote/metrics",
        "//remote/metrics",
        "/metrics#fragment",
        "x",
        "/a\r\nX: y",
    ] {
        assert!(
            request(&json!({"type":"scrape_metrics","path":path}), "/metrics").is_err(),
            "{path}"
        );
    }
    assert!(request(
        &json!({"type":"scrape_metrics","format":"protobuf"}),
        "/metrics"
    )
    .is_err());
}
#[test]
fn malformed_or_partial_expositions_are_refused_as_a_whole() {
    let cases = [
        ("x 1", Format::Text),
        ("x 1\n", Format::OpenMetrics),
        ("# EOF\nx 1\n", Format::OpenMetrics),
        ("# TYPE x gauge\n# TYPE x gauge\nx 1\n", Format::Text),
        ("x 1\n# TYPE x gauge\n", Format::Text),
        ("x{a=\"one\",a=\"two\"} 1\n", Format::Text),
        ("x{a=\"bad\\q\"} 1\n", Format::Text),
        ("x{a=\"one\"} 1\nx{a=\"one\"} 2\n", Format::Text),
        ("x 1.0 1.25\n", Format::Text),
        ("x 1e999\n", Format::Text),
        ("# TYPE h histogram\nh_bucket 1\n", Format::Text),
        ("# TYPE s summary\ns{quantile=\"1.1\"} 1\n", Format::Text),
        ("# TYPE x counter\nx_total -1\n# EOF\n", Format::OpenMetrics),
        ("# TYPE x info\nx_info 2\n# EOF\n", Format::OpenMetrics),
        (
            "# TYPE x gauge\nx 1 # {id=\"x\"} 1\n# EOF\n",
            Format::OpenMetrics,
        ),
        ("x 1\ny 2\nx{a=\"b\"} 3\n", Format::Text),
        (
            "# TYPE h histogram\n# TYPE h_count gauge\nh_count 1\n",
            Format::Text,
        ),
        (
            "# TYPE x gauge\n# UNIT x seconds\nx 1\n# EOF\n",
            Format::OpenMetrics,
        ),
        ("x{a=\"x\",} 1\n# EOF\n", Format::OpenMetrics),
    ];
    for (body, format) in cases {
        assert!(
            exposition::parse(body.as_bytes(), format).is_err(),
            "unexpected accept: {body:?}"
        );
    }
    assert!(exposition::parse(&[0xff, b'\n'], Format::Text).is_err());
}
#[test]
fn exact_sample_name_label_and_body_bounds() {
    let valid = (0..exposition::MAX_SAMPLES)
        .map(|n| format!("x{{id=\"{n}\"}} 1\n"))
        .collect::<String>();
    assert_eq!(
        parsed(&valid, Format::Text)["sample_count"],
        exposition::MAX_SAMPLES
    );
    assert!(exposition::parse(
        format!("{valid}x{{id=\"extra\"}} 1\n").as_bytes(),
        Format::Text
    )
    .is_err());
    let labels = (0..exposition::MAX_LABELS)
        .map(|n| format!("l{n}=\"v\""))
        .collect::<Vec<_>>()
        .join(",");
    parsed(&format!("x{{{labels}}} 1\n"), Format::Text);
    assert!(exposition::parse(
        format!("x{{{labels},extra=\"v\"}} 1\n").as_bytes(),
        Format::Text
    )
    .is_err());
    parsed(
        &format!("{} 1\n", "x".repeat(exposition::MAX_NAME)),
        Format::Text,
    );
    assert!(exposition::parse(
        format!("{} 1\n", "x".repeat(exposition::MAX_NAME + 1)).as_bytes(),
        Format::Text
    )
    .is_err());
    let mut body = vec![b' '; exposition::MAX_BODY];
    body[0] = b'#';
    *body.last_mut().unwrap() = b'\n';
    parsed(std::str::from_utf8(&body).unwrap(), Format::Text);
    body.push(b'\n');
    assert!(exposition::parse(&body, Format::Text).is_err());
}
#[test]
fn histogram_structure_and_complete_family_and_text_bounds() {
    for body in [
        "# TYPE h histogram\nh_bucket{le=\"1\"} 2\nh_count 2\n",
        "# TYPE h histogram\nh_bucket{le=\"1\"} 2\nh_bucket{le=\"+Inf\"} 1\n",
        "# TYPE h histogram\nh_bucket{le=\"1\"} 2\nh_bucket{le=\"+Inf\"} 3\nh_count 2\n",
        "# TYPE h histogram\nh_bucket{le=\"+Inf\"} 0.5\n",
        "# TYPE h histogram\nh_count 2\n",
    ] {
        assert!(
            exposition::parse(body.as_bytes(), Format::Text).is_err(),
            "{body}"
        );
    }
    let families = (0..exposition::MAX_FAMILIES)
        .map(|n| format!("# TYPE f{n} gauge\nf{n} 1\n"))
        .collect::<String>();
    assert_eq!(
        parsed(&families, Format::Text)["metrics"]
            .as_array()
            .unwrap()
            .len(),
        exposition::MAX_FAMILIES
    );
    assert!(exposition::parse(format!("{families}last 1\n").as_bytes(), Format::Text).is_err());
    let help = "x".repeat(exposition::MAX_TEXT);
    parsed(&format!("# HELP x {help}\nx 1\n"), Format::Text);
    assert!(
        exposition::parse(format!("# HELP x {help}x\nx 1\n").as_bytes(), Format::Text).is_err()
    );
}
