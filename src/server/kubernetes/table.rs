//! Server-side `Table` rendering (`application/json;as=Table;v=1;g=meta.k8s.io`).
//!
//! This is what `kubectl get` actually asks for: it sends `Accept: application/json;as=Table;…`
//! and prints the returned cells verbatim. A real apiserver produces these with a per-kind
//! `TableConvertor`; this module is the same idea, applied to whatever objects the model
//! invented. It is presentation, not content — every value in every cell comes out of the
//! model's object.
//!
//! A model that wants full control skips this entirely and answers with `k8s_table_response`,
//! which carries explicit `columns` and `rows`.

use serde_json::{json, Value};

/// Build a `Table` from a list of objects the model supplied.
pub fn table_from_items(kind: &str, items: &[Value]) -> Value {
    let columns = columns_for_kind(kind);
    let rows: Vec<Value> = items
        .iter()
        .map(|item| {
            let cells: Vec<Value> = columns
                .iter()
                .map(|col| Value::String(cell_value(kind, col, item)))
                .collect();
            json!({
                "cells": cells,
                "object": partial_object_metadata(item),
            })
        })
        .collect();

    json!({
        "kind": "Table",
        "apiVersion": "meta.k8s.io/v1",
        "metadata": {"resourceVersion": "1"},
        "columnDefinitions": columns
            .iter()
            .enumerate()
            .map(|(i, name)| json!({
                "name": name,
                "type": "string",
                "format": if i == 0 { "name" } else { "" },
                "description": format!("{name} column"),
                "priority": 0,
            }))
            .collect::<Vec<_>>(),
        "rows": rows,
    })
}

/// Build a `Table` from explicit `columns` / `rows` supplied by the model.
///
/// `columns` is an array of strings (or objects with a `name`); `rows` is an array of objects
/// with a `cells` array, optionally `name` and `namespace` used to synthesise the row's
/// `PartialObjectMetadata`. Everything is plain structured JSON — nothing is encoded, so
/// nothing needs decoding.
pub fn table_from_columns_and_rows(columns: &[Value], rows: &[Value]) -> Value {
    let column_names: Vec<String> = columns
        .iter()
        .map(|c| match c {
            Value::String(s) => s.clone(),
            other => other
                .get("name")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string(),
        })
        .collect();

    let column_defs: Vec<Value> = columns
        .iter()
        .enumerate()
        .map(|(i, c)| {
            let name = column_names.get(i).cloned().unwrap_or_default();
            let type_hint = c
                .get("type")
                .and_then(Value::as_str)
                .unwrap_or("string")
                .to_string();
            json!({
                "name": name,
                "type": type_hint,
                "format": if i == 0 { "name" } else { "" },
                "description": format!("{name} column"),
                "priority": 0,
            })
        })
        .collect();

    let row_values: Vec<Value> = rows
        .iter()
        .map(|row| {
            let cells: Vec<Value> = row
                .get("cells")
                .and_then(Value::as_array)
                .map(|cells| {
                    cells
                        .iter()
                        .map(|c| match c {
                            Value::String(s) => Value::String(s.clone()),
                            Value::Null => Value::String("<none>".to_string()),
                            other => Value::String(other.to_string()),
                        })
                        .collect()
                })
                .unwrap_or_default();

            let mut metadata = json!({});
            if let Some(name) = row.get("name").and_then(Value::as_str) {
                metadata["name"] = json!(name);
            } else if let Some(Value::String(first)) = cells.first() {
                metadata["name"] = json!(first);
            }
            if let Some(ns) = row.get("namespace").and_then(Value::as_str) {
                metadata["namespace"] = json!(ns);
            }

            json!({
                "cells": cells,
                "object": {
                    "kind": "PartialObjectMetadata",
                    "apiVersion": "meta.k8s.io/v1",
                    "metadata": metadata,
                },
            })
        })
        .collect();

    json!({
        "kind": "Table",
        "apiVersion": "meta.k8s.io/v1",
        "metadata": {"resourceVersion": "1"},
        "columnDefinitions": column_defs,
        "rows": row_values,
    })
}

fn partial_object_metadata(item: &Value) -> Value {
    json!({
        "kind": "PartialObjectMetadata",
        "apiVersion": "meta.k8s.io/v1",
        "metadata": item.get("metadata").cloned().unwrap_or_else(|| json!({})),
    })
}

/// Column headers per kind, matching what a real apiserver prints.
///
/// `kind` is the *item* kind (`Pod`), not the list kind (`PodList`); callers strip the suffix.
fn columns_for_kind(kind: &str) -> Vec<String> {
    let cols: &[&str] = match kind {
        "Pod" => &["NAME", "READY", "STATUS", "RESTARTS", "AGE"],
        "Node" => &["NAME", "STATUS", "ROLES", "AGE", "VERSION"],
        "Namespace" => &["NAME", "STATUS", "AGE"],
        "Service" => &[
            "NAME",
            "TYPE",
            "CLUSTER-IP",
            "EXTERNAL-IP",
            "PORT(S)",
            "AGE",
        ],
        "Deployment" | "ReplicaSet" | "StatefulSet" => {
            &["NAME", "READY", "UP-TO-DATE", "AVAILABLE", "AGE"]
        }
        _ => &["NAME", "AGE"],
    };
    cols.iter().map(|s| s.to_string()).collect()
}

fn cell_value(kind: &str, column: &str, item: &Value) -> String {
    match column {
        "NAME" => string_at(item, &["metadata", "name"]).unwrap_or_else(|| "<unknown>".into()),
        "AGE" => age_cell(item),
        "STATUS" => status_cell(kind, item),
        "READY" => ready_cell(kind, item),
        "RESTARTS" => restarts_cell(item),
        "ROLES" => roles_cell(item),
        "VERSION" => string_at(item, &["status", "nodeInfo", "kubeletVersion"])
            .unwrap_or_else(|| "<unknown>".into()),
        "TYPE" => string_at(item, &["spec", "type"]).unwrap_or_else(|| "ClusterIP".into()),
        "CLUSTER-IP" => string_at(item, &["spec", "clusterIP"]).unwrap_or_else(|| "<none>".into()),
        "EXTERNAL-IP" => external_ip_cell(item),
        "PORT(S)" => ports_cell(item),
        "UP-TO-DATE" => number_at(item, &["status", "updatedReplicas"]),
        "AVAILABLE" => number_at(item, &["status", "availableReplicas"]),
        _ => "<none>".into(),
    }
}

fn string_at(item: &Value, path: &[&str]) -> Option<String> {
    let mut cursor = item;
    for key in path {
        cursor = cursor.get(key)?;
    }
    cursor.as_str().map(|s| s.to_string())
}

fn number_at(item: &Value, path: &[&str]) -> String {
    let mut cursor = item;
    for key in path {
        match cursor.get(key) {
            Some(next) => cursor = next,
            None => return "0".to_string(),
        }
    }
    cursor
        .as_i64()
        .map(|n| n.to_string())
        .unwrap_or_else(|| "0".to_string())
}

fn status_cell(kind: &str, item: &Value) -> String {
    if item
        .get("metadata")
        .and_then(|m| m.get("deletionTimestamp"))
        .is_some()
    {
        return "Terminating".to_string();
    }
    if kind == "Node" {
        if let Some(conditions) = item
            .get("status")
            .and_then(|s| s.get("conditions"))
            .and_then(Value::as_array)
        {
            for condition in conditions {
                if condition.get("type").and_then(Value::as_str) == Some("Ready") {
                    return match condition.get("status").and_then(Value::as_str) {
                        Some("True") => "Ready".to_string(),
                        _ => "NotReady".to_string(),
                    };
                }
            }
        }
        return "Unknown".to_string();
    }
    string_at(item, &["status", "phase"])
        .or_else(|| string_at(item, &["status", "reason"]))
        .unwrap_or_else(|| "Unknown".into())
}

fn ready_cell(kind: &str, item: &Value) -> String {
    if kind == "Pod" {
        let statuses = item
            .get("status")
            .and_then(|s| s.get("containerStatuses"))
            .and_then(Value::as_array);
        let total_spec = item
            .get("spec")
            .and_then(|s| s.get("containers"))
            .and_then(Value::as_array)
            .map(|c| c.len());
        if let Some(statuses) = statuses {
            let ready = statuses
                .iter()
                .filter(|s| s.get("ready").and_then(Value::as_bool).unwrap_or(false))
                .count();
            let total = total_spec.unwrap_or(statuses.len());
            return format!("{ready}/{total}");
        }
        // No containerStatuses: a model that returned only spec+phase still deserves a
        // plausible column rather than "0/1" for a Running pod.
        let total = total_spec.unwrap_or(1);
        let phase = string_at(item, &["status", "phase"]).unwrap_or_default();
        let ready = if phase == "Running" || phase == "Succeeded" {
            total
        } else {
            0
        };
        return format!("{ready}/{total}");
    }
    // Workload kinds: readyReplicas/replicas.
    let ready = number_at(item, &["status", "readyReplicas"]);
    let desired = item
        .get("spec")
        .and_then(|s| s.get("replicas"))
        .and_then(Value::as_i64)
        .or_else(|| {
            item.get("status")
                .and_then(|s| s.get("replicas"))
                .and_then(Value::as_i64)
        })
        .unwrap_or(0);
    format!("{ready}/{desired}")
}

fn restarts_cell(item: &Value) -> String {
    // Saturating, not `sum()`. `restartCount` comes out of the model's object, so two
    // containers claiming `i64::MAX` is reachable — and `Cargo.toml` has no `[profile.dev]`, so
    // `overflow-checks` is on in every debug and test build: a plain `sum()` would panic there
    // and wrap silently in the shipped binary, which is exactly the wrong way round.
    let total: i64 = item
        .get("status")
        .and_then(|s| s.get("containerStatuses"))
        .and_then(Value::as_array)
        .map(|statuses| {
            statuses
                .iter()
                .filter_map(|s| s.get("restartCount").and_then(Value::as_i64))
                .fold(0i64, i64::saturating_add)
        })
        .unwrap_or(0);
    total.to_string()
}

fn roles_cell(item: &Value) -> String {
    let labels = item
        .get("metadata")
        .and_then(|m| m.get("labels"))
        .and_then(Value::as_object);
    let Some(labels) = labels else {
        return "<none>".to_string();
    };
    let mut roles: Vec<String> = labels
        .keys()
        .filter_map(|k| k.strip_prefix("node-role.kubernetes.io/"))
        .filter(|r| !r.is_empty())
        .map(|r| r.to_string())
        .collect();
    roles.sort();
    if roles.is_empty() {
        "<none>".to_string()
    } else {
        roles.join(",")
    }
}

fn external_ip_cell(item: &Value) -> String {
    if let Some(ips) = item
        .get("spec")
        .and_then(|s| s.get("externalIPs"))
        .and_then(Value::as_array)
    {
        let joined: Vec<&str> = ips.iter().filter_map(Value::as_str).collect();
        if !joined.is_empty() {
            return joined.join(",");
        }
    }
    if let Some(ingress) = item
        .get("status")
        .and_then(|s| s.get("loadBalancer"))
        .and_then(|lb| lb.get("ingress"))
        .and_then(Value::as_array)
    {
        let joined: Vec<String> = ingress
            .iter()
            .filter_map(|i| {
                i.get("ip")
                    .and_then(Value::as_str)
                    .or_else(|| i.get("hostname").and_then(Value::as_str))
            })
            .map(|s| s.to_string())
            .collect();
        if !joined.is_empty() {
            return joined.join(",");
        }
    }
    "<none>".to_string()
}

fn ports_cell(item: &Value) -> String {
    let Some(ports) = item
        .get("spec")
        .and_then(|s| s.get("ports"))
        .and_then(Value::as_array)
    else {
        return "<none>".to_string();
    };
    let rendered: Vec<String> = ports
        .iter()
        .map(|p| {
            let port = p.get("port").and_then(Value::as_i64).unwrap_or(0);
            let proto = p
                .get("protocol")
                .and_then(Value::as_str)
                .unwrap_or("TCP")
                .to_string();
            match p.get("nodePort").and_then(Value::as_i64) {
                Some(node_port) => format!("{port}:{node_port}/{proto}"),
                None => format!("{port}/{proto}"),
            }
        })
        .collect();
    if rendered.is_empty() {
        "<none>".to_string()
    } else {
        rendered.join(",")
    }
}

/// `metadata.creationTimestamp` → the compact duration kubectl shows in the AGE column.
fn age_cell(item: &Value) -> String {
    let Some(created) = string_at(item, &["metadata", "creationTimestamp"]) else {
        return "<unknown>".to_string();
    };
    let Ok(created) = chrono::DateTime::parse_from_rfc3339(&created) else {
        return "<unknown>".to_string();
    };
    let seconds = (chrono::Utc::now() - created.with_timezone(&chrono::Utc)).num_seconds();
    human_duration(seconds)
}

/// A close-enough port of Kubernetes' `duration.HumanDuration`.
pub fn human_duration(seconds: i64) -> String {
    if seconds < 0 {
        return "<invalid>".to_string();
    }
    if seconds < 60 {
        return format!("{seconds}s");
    }
    let minutes = seconds / 60;
    if minutes < 60 {
        return format!("{minutes}m");
    }
    let hours = minutes / 60;
    if hours < 48 {
        return format!("{hours}h");
    }
    let days = hours / 24;
    if days < 365 {
        return format!("{days}d");
    }
    let years = days / 365;
    format!("{}y{}d", years, days % 365)
}
