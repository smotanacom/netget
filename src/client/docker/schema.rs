//! Selected read-only Engine response fields, with their native JSON types.
use anyhow::{ensure, Context, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
pub const MAX_BODY: usize = 4 * 1024 * 1024;
pub const MAX_ITEMS: usize = 4096;
pub const MAX_TEXT: usize = 16 * 1024;
type Strings = BTreeMap<String, String>;
macro_rules! object {
    ($name:ident { $($field:ident : $ty:ty  =>  $wire:literal),* $(,)? })  =>  {
        #[derive(Debug, Deserialize, Serialize)]
        pub struct $name { $(#[serde(rename(deserialize = $wire))] pub $field: $ty),* }
    }
}
object!(Component { name:String => "Name", version:String => "Version", details:Option<Strings> => "Details" });
object!(Platform { name:String => "Name" });
object!(Version {
    version:String => "Version", api_version:String => "ApiVersion", min_api_version:Option<String> => "MinAPIVersion",
    os:String => "Os", arch:String => "Arch", git_commit:Option<String> => "GitCommit", go_version:Option<String> => "GoVersion",
    kernel_version:Option<String> => "KernelVersion", experimental:Option<bool> => "Experimental",
    components:Option<Vec<Component>> => "Components", platform:Option<Platform> => "Platform"
});
object!(Info {
    id:String => "ID", name:String => "Name", server_version:String => "ServerVersion",
    containers:u64 => "Containers", containers_running:u64 => "ContainersRunning", containers_paused:u64 => "ContainersPaused",
    containers_stopped:u64 => "ContainersStopped", images:u64 => "Images", ncpu:u64 => "NCPU", memory_total:u64 => "MemTotal",
    driver:Option<String> => "Driver", operating_system:Option<String> => "OperatingSystem", os_type:Option<String> => "OSType",
    architecture:Option<String> => "Architecture", kernel_version:Option<String> => "KernelVersion",
    labels:Option<Vec<String>> => "Labels", warnings:Option<Vec<String>> => "Warnings"
});
object!(Port { private_port:u16 => "PrivatePort", public_port:Option<u16> => "PublicPort", protocol:String => "Type", ip:Option<String> => "IP" });
object!(Container {
    id:String => "Id", names:Vec<String> => "Names", image:String => "Image", image_id:String => "ImageID", command:String => "Command",
    created:i64 => "Created", ports:Option<Vec<Port>> => "Ports", labels:Option<Strings> => "Labels", state:String => "State", status:String => "Status",
    size_rw:Option<u64> => "SizeRw", size_root_fs:Option<u64> => "SizeRootFs"
});
object!(ContainerState {
    status:String => "Status", running:bool => "Running", paused:bool => "Paused", restarting:bool => "Restarting",
    oom_killed:bool => "OOMKilled", dead:bool => "Dead", pid:u64 => "Pid", exit_code:i64 => "ExitCode", error:String => "Error",
    started_at:String => "StartedAt", finished_at:String => "FinishedAt"
});
object!(Config {
    image:String => "Image", hostname:Option<String> => "Hostname", user:Option<String> => "User", env:Option<Vec<String>> => "Env",
    cmd:Option<Vec<String>> => "Cmd", entrypoint:Option<Vec<String>> => "Entrypoint", working_dir:Option<String> => "WorkingDir",
    labels:Option<Strings> => "Labels", tty:Option<bool> => "Tty", open_stdin:Option<bool> => "OpenStdin"
});
object!(Binding { host_ip:String => "HostIp", host_port:String => "HostPort" });
object!(HostConfig {
    network_mode:Option<String> => "NetworkMode", memory:Option<i64> => "Memory", cpu_shares:Option<i64> => "CpuShares",
    port_bindings:Option<BTreeMap<String,Option<Vec<Binding>>>> => "PortBindings"
});
object!(EndpointSettings {
    network_id:Option<String> => "NetworkID", endpoint_id:Option<String> => "EndpointID", ip_address:Option<String> => "IPAddress",
    ip_prefix_len:Option<u8> => "IPPrefixLen", gateway:Option<String> => "Gateway", mac_address:Option<String> => "MacAddress",
    global_ipv6_address:Option<String> => "GlobalIPv6Address", global_ipv6_prefix_len:Option<u8> => "GlobalIPv6PrefixLen"
});
object!(NetworkSettings {
    ports:Option<BTreeMap<String,Option<Vec<Binding>>>> => "Ports", networks:Option<BTreeMap<String,EndpointSettings>> => "Networks"
});
object!(Mount {
    kind:String => "Type", name:Option<String> => "Name", source:String => "Source", destination:String => "Destination",
    driver:Option<String> => "Driver", mode:Option<String> => "Mode", read_write:bool => "RW", propagation:Option<String> => "Propagation"
});
object!(Inspect {
    id:String => "Id", name:String => "Name", created:String => "Created", image:String => "Image", path:String => "Path", args:Vec<String> => "Args",
    state:ContainerState => "State", config:Config => "Config", host_config:HostConfig => "HostConfig", network_settings:NetworkSettings => "NetworkSettings",
    mounts:Option<Vec<Mount>> => "Mounts", size_rw:Option<u64> => "SizeRw", size_root_fs:Option<u64> => "SizeRootFs"
});
object!(Image {
    id:String => "Id", parent_id:Option<String> => "ParentId", repo_tags:Option<Vec<String>> => "RepoTags", repo_digests:Option<Vec<String>> => "RepoDigests",
    created:i64 => "Created", size:u64 => "Size", shared_size:Option<i64> => "SharedSize", containers:Option<i64> => "Containers", labels:Option<Strings> => "Labels"
});
object!(IpamConfig { subnet:Option<String> => "Subnet", ip_range:Option<String> => "IPRange", gateway:Option<String> => "Gateway", auxiliary_addresses:Option<Strings> => "AuxiliaryAddresses" });
object!(Ipam { driver:String => "Driver", options:Option<Strings> => "Options", config:Option<Vec<IpamConfig>> => "Config" });
object!(Network {
    id:String => "Id", name:String => "Name", created:Option<String> => "Created", scope:String => "Scope", driver:String => "Driver",
    enable_ipv6:Option<bool> => "EnableIPv6", internal:bool => "Internal", attachable:Option<bool> => "Attachable", ingress:Option<bool> => "Ingress",
    ipam:Ipam => "IPAM", options:Option<Strings> => "Options", labels:Option<Strings> => "Labels"
});
object!(Usage { ref_count:i64 => "RefCount", size:i64 => "Size" });
object!(Volume {
    name:String => "Name", driver:String => "Driver", mountpoint:String => "Mountpoint", created_at:Option<String> => "CreatedAt",
    scope:String => "Scope", labels:Option<Strings> => "Labels", options:Option<Strings> => "Options", usage_data:Option<Usage> => "UsageData"
});
object!(Volumes { volumes:Option<Vec<Volume>> => "Volumes", warnings:Option<Vec<String>> => "Warnings" });
object!(Error { message:String => "message" });
struct Seed<'a> {
    depth: usize,
    nodes: &'a mut usize,
}
impl<'de> serde::de::DeserializeSeed<'de> for Seed<'_> {
    type Value = Value;
    fn deserialize<D: serde::Deserializer<'de>>(
        self,
        d: D,
    ) -> std::result::Result<Value, D::Error> {
        use serde::de::Error;
        *self.nodes += 1;
        if self.depth > 32 || *self.nodes > 65536 {
            return Err(D::Error::custom("Docker JSON nesting/node limit"));
        }
        d.deserialize_any(self)
    }
}
impl<'de> serde::de::Visitor<'de> for Seed<'_> {
    type Value = Value;
    fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.write_str("bounded Docker JSON")
    }
    fn visit_bool<E: serde::de::Error>(self, v: bool) -> std::result::Result<Value, E> {
        Ok(Value::Bool(v))
    }
    fn visit_i64<E: serde::de::Error>(self, v: i64) -> std::result::Result<Value, E> {
        Ok(v.into())
    }
    fn visit_u64<E: serde::de::Error>(self, v: u64) -> std::result::Result<Value, E> {
        Ok(v.into())
    }
    fn visit_f64<E: serde::de::Error>(self, v: f64) -> std::result::Result<Value, E> {
        serde_json::Number::from_f64(v)
            .map(Value::Number)
            .ok_or_else(|| E::custom("invalid JSON number"))
    }
    fn visit_unit<E: serde::de::Error>(self) -> std::result::Result<Value, E> {
        Ok(Value::Null)
    }
    fn visit_str<E: serde::de::Error>(self, v: &str) -> std::result::Result<Value, E> {
        if v.len() > MAX_TEXT {
            return Err(E::custom("Docker text limit"));
        }
        Ok(Value::String(v.into()))
    }
    fn visit_string<E: serde::de::Error>(self, v: String) -> std::result::Result<Value, E> {
        if v.len() > MAX_TEXT {
            return Err(E::custom("Docker text limit"));
        }
        Ok(Value::String(v))
    }
    fn visit_seq<A: serde::de::SeqAccess<'de>>(
        self,
        mut a: A,
    ) -> std::result::Result<Value, A::Error> {
        use serde::de::Error;
        let mut values = Vec::new();
        while let Some(v) = a.next_element_seed(Seed {
            depth: self.depth + 1,
            nodes: self.nodes,
        })? {
            if values.len() >= MAX_ITEMS {
                return Err(A::Error::custom("Docker array limit"));
            }
            values.push(v);
        }
        Ok(Value::Array(values))
    }
    fn visit_map<A: serde::de::MapAccess<'de>>(
        self,
        mut a: A,
    ) -> std::result::Result<Value, A::Error> {
        use serde::de::Error;
        let mut values = serde_json::Map::new();
        while let Some(key) = a.next_key::<String>()? {
            if key.len() > 256 || values.len() >= 256 {
                return Err(A::Error::custom("Docker object field/name limit"));
            }
            if values.contains_key(&key) {
                return Err(A::Error::custom("duplicate Docker JSON field"));
            }
            let value = a.next_value_seed(Seed {
                depth: self.depth + 1,
                nodes: self.nodes,
            })?;
            values.insert(key, value);
        }
        Ok(Value::Object(values))
    }
}
pub fn json(body: &[u8]) -> Result<Value> {
    ensure!(body.len() <= MAX_BODY, "Docker body limit");
    use serde::de::DeserializeSeed;
    let mut decoder = serde_json::Deserializer::from_slice(body);
    let value = Seed {
        depth: 0,
        nodes: &mut 0,
    }
    .deserialize(&mut decoder)
    .context("invalid Docker JSON")?;
    decoder.end().context("trailing Docker JSON content")?;
    Ok(value)
}
fn typed<T: serde::de::DeserializeOwned + Serialize>(value: Value) -> Result<Value> {
    Ok(serde_json::to_value(
        serde_json::from_value::<T>(value).context("invalid Docker response schema")?,
    )?)
}
fn state(value: &str) -> Result<()> {
    ensure!(
        [
            "created",
            "running",
            "paused",
            "restarting",
            "removing",
            "exited",
            "dead"
        ]
        .contains(&value),
        "invalid container state"
    );
    Ok(())
}
fn id(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 256
            && value
                .bytes()
                .all(|c| c.is_ascii_alphanumeric() || b"_-.".contains(&c)),
        "invalid container ID"
    );
    Ok(())
}
pub fn parse(operation: &str, body: &[u8]) -> Result<Value> {
    let value = json(body)?;
    let result = match operation {
        "version" => typed::<Version>(value)?,
        "info" => typed::<Info>(value)?,
        "containers" => typed::<Vec<Container>>(value)?,
        "container" => typed::<Inspect>(value)?,
        "images" => typed::<Vec<Image>>(value)?,
        "networks" => typed::<Vec<Network>>(value)?,
        "volumes" => typed::<Volumes>(value)?,
        _ => anyhow::bail!("unknown Docker response operation"),
    };
    match operation {
        "version" => {
            super::api_version(result["api_version"].as_str().unwrap())?;
            if let Some(v) = result["min_api_version"].as_str() {
                ensure!(
                    super::api_version(v)?
                        <= super::api_version(result["api_version"].as_str().unwrap())?,
                    "invalid API version range"
                );
            }
        }
        "info" => {
            ensure!(
                result["containers_running"]
                    .as_u64()
                    .unwrap()
                    .checked_add(result["containers_paused"].as_u64().unwrap())
                    .and_then(|n| n.checked_add(result["containers_stopped"].as_u64().unwrap()))
                    == result["containers"].as_u64(),
                "inconsistent container counts"
            );
        }
        "containers" => {
            for c in result.as_array().unwrap() {
                id(c["id"].as_str().unwrap())?;
                state(c["state"].as_str().unwrap())?;
                for p in c["ports"].as_array().into_iter().flatten() {
                    ensure!(
                        p["private_port"].as_u64().unwrap() > 0
                            && p["public_port"].as_u64() != Some(0),
                        "invalid container port"
                    );
                    ensure!(
                        ["tcp", "udp", "sctp"].contains(&p["protocol"].as_str().unwrap()),
                        "invalid port protocol"
                    );
                }
            }
        }
        "container" => {
            id(result["id"].as_str().unwrap())?;
            state(result["state"]["status"].as_str().unwrap())?;
        }
        "images" => {
            for i in result.as_array().unwrap() {
                for field in ["shared_size", "containers"] {
                    if let Some(n) = i[field].as_i64() {
                        ensure!(n >= -1, "invalid image {field}");
                    }
                }
            }
        }
        "volumes" => {
            if let Some(volumes) = result["volumes"].as_array() {
                for v in volumes {
                    if let Some(usage) = v["usage_data"].as_object() {
                        for field in ["size", "ref_count"] {
                            ensure!(
                                usage[field].as_i64().unwrap() >= -1,
                                "invalid volume usage {field}"
                            );
                        }
                    }
                }
            }
        }
        _ => {}
    }
    Ok(result)
}
pub fn error(body: &[u8]) -> Result<String> {
    let e: Error = serde_json::from_value(json(body)?).context("invalid Docker error schema")?;
    Ok(e.message)
}
