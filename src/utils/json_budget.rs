//! Iterative budgets for JSON constructed in memory (serde's parse depth limit
//! does not apply to values built programmatically).
use serde_json::Value;

/// Conservative retained-content estimate: strings/keys plus per-node overhead.
/// The traversal itself is bounded by the node budget before queuing children.
pub fn within_budget(value: &Value, max_bytes: usize, max_nodes: usize, max_depth: usize) -> bool {
    within_values_budget(std::iter::once(value), max_bytes, max_nodes, max_depth)
}

/// Apply one aggregate budget to a borrowed batch before any values are cloned.
/// Stop collecting roots at the node budget, then walk children iteratively.
pub fn within_values_budget<'a>(
    values: impl IntoIterator<Item = &'a Value>,
    max_bytes: usize,
    max_nodes: usize,
    max_depth: usize,
) -> bool {
    let mut pending = Vec::new();
    for value in values {
        if pending.len() >= max_nodes {
            return false;
        }
        pending.push((value, 0usize));
    }
    let mut nodes = 0usize;
    let mut bytes = 0usize;
    while let Some((value, depth)) = pending.pop() {
        nodes = nodes.saturating_add(1);
        bytes = bytes.saturating_add(std::mem::size_of::<Value>());
        if nodes > max_nodes || depth > max_depth || bytes > max_bytes {
            return false;
        }
        match value {
            Value::String(s) => bytes = bytes.saturating_add(s.len()),
            Value::Array(values) => {
                if values.len() > max_nodes.saturating_sub(nodes + pending.len()) {
                    return false;
                }
                pending.extend(values.iter().map(|v| (v, depth + 1)));
            }
            Value::Object(values) => {
                if values.len() > max_nodes.saturating_sub(nodes + pending.len()) {
                    return false;
                }
                for (key, value) in values {
                    bytes = bytes.saturating_add(key.len()).saturating_add(64);
                    if bytes > max_bytes {
                        return false;
                    }
                    pending.push((value, depth + 1));
                }
            }
            _ => {}
        }
        if bytes > max_bytes {
            return false;
        }
    }
    true
}

/// Avoid recursive Value destruction when rejecting an excessive nesting depth.
pub fn drop_iteratively(value: Value) {
    let mut pending = vec![value];
    while let Some(value) = pending.pop() {
        match value {
            Value::Array(values) => pending.extend(values),
            Value::Object(values) => pending.extend(values.into_values()),
            _ => {}
        }
    }
}
