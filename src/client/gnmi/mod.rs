//! One responsive owner contains calls, subscriptions, event handlers and controls.
pub mod actions;
pub mod request;
mod runtime;
pub use runtime::connect;
pub const DEFAULT_TLS: bool = false;
pub const CONNECT_TIMEOUT_SECS: u64 = 10;
pub const DEFAULT_IDLE_TIMEOUT_SECS: u64 = 120;
pub fn check_action(action: &serde_json::Value) -> Result<(), tonic::Status> {
    crate::server::gnmi::semantic::check_model(action)?;
    let kind = action["type"].as_str().unwrap_or("");
    let allowed: &[&str] = match kind {
        "disconnect" | "wait_for_more" => &["type"],
        "gnmi_poll" | "gnmi_cancel" => &["type", "call_id"],
        "gnmi_capabilities" => &["type", "call_id", "gzip"],
        "gnmi_get" | "gnmi_set" | "gnmi_subscribe" => &["type", "call_id", "gzip", "request"],
        _ => {
            return Err(tonic::Status::invalid_argument(
                "unknown gNMI client action",
            ))
        }
    };
    let fields = action
        .as_object()
        .ok_or_else(|| tonic::Status::invalid_argument("gNMI action must be an object"))?;
    if fields.keys().any(|key| !allowed.contains(&key.as_str())) {
        return Err(tonic::Status::invalid_argument(
            "unsupported gNMI client action field",
        ));
    }
    if !matches!(kind, "disconnect" | "wait_for_more")
        && action["call_id"]
            .as_u64()
            .is_none_or(|id| id == 0 || id > u64::from(u32::MAX))
    {
        return Err(tonic::Status::invalid_argument(
            "call_id must be a positive u32",
        ));
    }
    if action.get("gzip").is_some_and(|v| !v.is_boolean()) {
        return Err(tonic::Status::invalid_argument("gzip must be boolean"));
    }
    Ok(())
}
