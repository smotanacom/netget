//! SRT stream IDs: the access-control syntax (`#!::r=live/cam,m=publish,u=alice`, SRT
//! AccessControl.md) and the `publish:path` / `read:path` form MediaMTX uses.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Target {
    /// The resource, e.g. live/cam.
    pub resource: String,
    /// "publish" or "request".
    pub mode: String,
    pub user: Option<String>,
}

pub fn parse(stream_id: &str) -> Result<Target, String> {
    if stream_id.len() > 512 || crate::utils::sanitize::has_controls(&stream_id) {
        return Err("stream ID is over 512 characters or holds a control character".into());
    }
    if let Some(list) = stream_id.strip_prefix("#!::") {
        let mut t = Target {
            resource: String::new(),
            mode: "request".into(),
            user: None,
        };
        for item in list.split(',').filter(|i| !i.is_empty()) {
            let (k, v) = item
                .split_once('=')
                .ok_or_else(|| format!("{item:?} is not key=value"))?;
            match k {
                "r" => t.resource = v.to_owned(),
                "m" => {
                    t.mode = match v {
                        "publish" => "publish",
                        "request" => "request",
                        "bidirectional" => return Err("bidirectional mode is not supported".into()),
                        other => return Err(format!("unknown mode {other:?}")),
                    }
                    .into()
                }
                "u" => t.user = Some(v.to_owned()),
                _ => {}
            }
        }
        return Ok(t);
    }
    for (prefix, mode) in [("publish:", "publish"), ("read:", "request")] {
        if let Some(rest) = stream_id.strip_prefix(prefix) {
            // publish:path[:user:pass][?query]
            let mut parts = rest.split('?').next().unwrap_or_default().split(':');
            let path = parts.next().unwrap_or_default();
            let user = parts.next().filter(|u| !u.is_empty()).map(str::to_owned);
            return Ok(Target {
                resource: path.to_owned(),
                mode: mode.into(),
                user,
            });
        }
    }
    Ok(Target {
        resource: stream_id.to_owned(),
        mode: "request".into(),
        user: None,
    })
}
