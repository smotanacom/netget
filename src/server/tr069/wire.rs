//! TR-069 (CWMP) on the wire, for both roles: SOAP envelopes read into a bounded element tree
//! and from there into JSON shapes the model can read, and the envelopes NetGet writes. Every
//! value NetGet writes is XML-escaped; nothing the model says is spliced in raw.
use anyhow::{bail, ensure, Context, Result};
use quick_xml::escape::escape;
use quick_xml::events::Event as XmlEvent;
use serde_json::{json, Map, Value};

/// A SOAP envelope at most.
pub const MAX_ENVELOPE: usize = 1024 * 1024;
/// Element nesting at most.
pub const MAX_DEPTH: usize = 32;
/// Elements in one envelope at most.
pub const MAX_ELEMENTS: usize = 50_000;
/// Parameters one message may carry.
pub const MAX_PARAMETERS: usize = 10_000;
pub const CWMP_NS: &str = "urn:dslforum-org:cwmp-1-0";

#[derive(Debug, Default, Clone)]
pub struct Element {
    pub name: String,
    pub attrs: Vec<(String, String)>,
    pub children: Vec<Element>,
    pub text: String,
}

fn local(name: &[u8]) -> String {
    let s = String::from_utf8_lossy(name);
    s.rsplit(':').next().unwrap_or_default().to_string()
}

impl Element {
    pub fn child(&self, name: &str) -> Option<&Element> {
        self.children.iter().find(|c| c.name == name)
    }
    pub fn text_of(&self, name: &str) -> Option<String> {
        self.child(name).map(|c| c.text.trim().to_string())
    }
    pub fn attr(&self, name: &str) -> Option<&str> {
        self.attrs
            .iter()
            .find(|(k, _)| k == name)
            .map(|(_, v)| v.as_str())
    }
}

/// The element tree of an XML document, bounded in size, depth and element count.
pub fn tree(xml: &[u8]) -> Result<Element> {
    ensure!(
        xml.len() <= MAX_ENVELOPE,
        "envelope larger than {MAX_ENVELOPE} bytes"
    );
    let mut reader = quick_xml::Reader::from_reader(xml);
    reader.config_mut().trim_text(false);
    let mut stack: Vec<Element> = vec![Element::default()];
    let mut count = 0usize;
    let mut buf = Vec::new();
    loop {
        let ev = reader
            .read_event_into(&mut buf)
            .context("the envelope is not well-formed XML")?;
        let empty = matches!(ev, XmlEvent::Empty(_));
        match ev {
            XmlEvent::Start(e) | XmlEvent::Empty(e) => {
                count += 1;
                ensure!(count <= MAX_ELEMENTS, "more than {MAX_ELEMENTS} elements");
                ensure!(stack.len() <= MAX_DEPTH, "nested deeper than {MAX_DEPTH}");
                let mut el = Element {
                    name: local(e.name().as_ref()),
                    ..Default::default()
                };
                for a in e.attributes().flatten() {
                    let v = a
                        .decode_and_unescape_value(reader.decoder())
                        .map(|v| v.into_owned())
                        .unwrap_or_default();
                    el.attrs.push((local(a.key.as_ref()), v));
                }
                if empty {
                    if let Some(parent) = stack.last_mut() {
                        parent.children.push(el);
                    }
                } else {
                    stack.push(el);
                }
            }
            XmlEvent::End(_) => {
                ensure!(stack.len() > 1, "an end tag with nothing open");
                let el = stack.pop().unwrap_or_default();
                if let Some(parent) = stack.last_mut() {
                    parent.children.push(el);
                }
            }
            XmlEvent::Text(t) => {
                if let Some(top) = stack.last_mut() {
                    top.text
                        .push_str(&t.unescape().context("bad character reference")?);
                }
            }
            XmlEvent::CData(t) => {
                if let Some(top) = stack.last_mut() {
                    top.text.push_str(&String::from_utf8_lossy(&t));
                }
            }
            XmlEvent::DocType(_) => bail!("a DOCTYPE is not allowed in a SOAP envelope"),
            XmlEvent::Eof => break,
            _ => {}
        }
        buf.clear();
    }
    ensure!(stack.len() == 1, "the document ends inside an element");
    stack
        .pop()
        .unwrap_or_default()
        .children
        .into_iter()
        .next()
        .context("an empty document")
}

/// One SOAP message: its cwmp:ID, the method element's name and its content as JSON.
#[derive(Debug)]
pub struct Message {
    pub id: Option<String>,
    pub method: String,
    pub content: Value,
}

pub fn parse(xml: &[u8]) -> Result<Message> {
    let env = tree(xml)?;
    ensure!(env.name == "Envelope", "not a SOAP envelope");
    let id = env.child("Header").and_then(|h| h.text_of("ID"));
    let body = env.child("Body").context("the envelope has no Body")?;
    let el = body.children.first().context("the Body is empty")?;
    let content = match el.name.as_str() {
        "Fault" => fault_json(el),
        m => method_json(m, el)?,
    };
    let method = if el.name == "Fault" {
        "Fault".to_string()
    } else {
        el.name.clone()
    };
    Ok(Message {
        id,
        method,
        content,
    })
}

fn param_values(list: Option<&Element>) -> Result<Vec<Value>> {
    let items: Vec<&Element> = list
        .map(|l| l.children.iter().collect())
        .unwrap_or_default();
    ensure!(
        items.len() <= MAX_PARAMETERS,
        "more than {MAX_PARAMETERS} parameters"
    );
    Ok(items
        .into_iter()
        .map(|p| {
            let value = p.child("Value");
            json!({"name": p.text_of("Name").unwrap_or_default(),
                   "value": value.map(|v| v.text.clone()).unwrap_or_default(),
                   "type": value.and_then(|v| v.attr("type")).unwrap_or("xsd:string")})
        })
        .collect())
}

fn method_json(method: &str, el: &Element) -> Result<Value> {
    Ok(match method {
        "Inform" => {
            let d = el.child("DeviceId");
            let field = |n: &str| d.and_then(|d| d.text_of(n)).unwrap_or_default();
            json!({
                "device_id": {"manufacturer": field("Manufacturer"), "oui": field("OUI"),
                              "product_class": field("ProductClass"), "serial_number": field("SerialNumber")},
                "events": el.child("Event").map(|e| e.children.iter().map(|s| json!({
                    "code": s.text_of("EventCode").unwrap_or_default(),
                    "command_key": s.text_of("CommandKey").unwrap_or_default()})).collect::<Vec<_>>()).unwrap_or_default(),
                "max_envelopes": el.text_of("MaxEnvelopes"),
                "current_time": el.text_of("CurrentTime"),
                "retry_count": el.text_of("RetryCount").and_then(|r| r.parse::<u64>().ok()).unwrap_or(0),
                "parameters": param_values(el.child("ParameterList"))?,
            })
        }
        "InformResponse" => json!({"max_envelopes": el.text_of("MaxEnvelopes")}),
        "GetParameterValuesResponse" => {
            json!({"parameters": param_values(el.child("ParameterList"))?})
        }
        "SetParameterValuesResponse" | "AddObjectResponse" | "DeleteObjectResponse" => {
            let mut o = json!({"status": el.text_of("Status").and_then(|s| s.parse::<u64>().ok()).unwrap_or(0)});
            if let Some(n) = el.text_of("InstanceNumber") {
                o["instance_number"] = json!(n.parse::<u64>().unwrap_or(0));
            }
            o
        }
        "GetParameterNamesResponse" => {
            let items: Vec<&Element> = el
                .child("ParameterList")
                .map(|l| l.children.iter().collect())
                .unwrap_or_default();
            ensure!(
                items.len() <= MAX_PARAMETERS,
                "more than {MAX_PARAMETERS} parameters"
            );
            json!({"parameters": items.into_iter().map(|p| json!({
                "name": p.text_of("Name").unwrap_or_default(),
                "writable": matches!(p.text_of("Writable").as_deref(), Some("1" | "true"))})).collect::<Vec<_>>()})
        }
        "GetRPCMethodsResponse" => {
            json!({"methods": el.child("MethodList").map(|l| l.children.iter().map(|m| m.text.trim().to_string()).collect::<Vec<_>>()).unwrap_or_default()})
        }
        "GetParameterValues" => {
            json!({"names": el.child("ParameterNames").map(|l| l.children.iter().map(|n| n.text.trim().to_string()).collect::<Vec<_>>()).unwrap_or_default()})
        }
        "SetParameterValues" => {
            json!({"parameters": param_values(el.child("ParameterList"))?, "parameter_key": el.text_of("ParameterKey").unwrap_or_default()})
        }
        "GetParameterNames" => json!({"path": el.text_of("ParameterPath").unwrap_or_default(),
                                      "next_level": matches!(el.text_of("NextLevel").as_deref(), Some("1" | "true"))}),
        "AddObject" | "DeleteObject" => {
            json!({"object": el.text_of("ObjectName").unwrap_or_default(), "parameter_key": el.text_of("ParameterKey").unwrap_or_default()})
        }
        "Reboot" => json!({"command_key": el.text_of("CommandKey").unwrap_or_default()}),
        // Anything else: its child elements as text, for the model to read.
        _ => {
            let mut m = Map::new();
            for c in &el.children {
                m.insert(c.name.clone(), json!(c.text.trim()));
            }
            Value::Object(m)
        }
    })
}

fn fault_json(el: &Element) -> Value {
    let f = el.child("detail").and_then(|d| d.child("Fault"));
    json!({
        "code": f.and_then(|f| f.text_of("FaultCode")).and_then(|c| c.parse::<u64>().ok()).unwrap_or(0),
        "message": f.and_then(|f| f.text_of("FaultString")).or_else(|| el.text_of("faultstring")).unwrap_or_default(),
    })
}

fn esc(s: &str) -> String {
    escape(s).into_owned()
}

/// A complete envelope around one method element.
pub fn envelope(id: &str, method: &str) -> String {
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n<soap-env:Envelope xmlns:soap-env=\"http://schemas.xmlsoap.org/soap/envelope/\" xmlns:soap-enc=\"http://schemas.xmlsoap.org/soap/encoding/\" xmlns:xsd=\"http://www.w3.org/2001/XMLSchema\" xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\" xmlns:cwmp=\"{CWMP_NS}\"><soap-env:Header><cwmp:ID soap-env:mustUnderstand=\"1\">{}</cwmp:ID></soap-env:Header><soap-env:Body>{method}</soap-env:Body></soap-env:Envelope>",
        esc(id)
    )
}

/// `(name, value, xsd type)` as a ParameterValueStruct list.
pub fn value_list(params: &[(String, String, String)]) -> String {
    let mut s = format!(
        "<ParameterList soap-enc:arrayType=\"cwmp:ParameterValueStruct[{}]\">",
        params.len()
    );
    for (n, v, t) in params {
        s.push_str(&format!(
            "<ParameterValueStruct><Name>{}</Name><Value xsi:type=\"{}\">{}</Value></ParameterValueStruct>",
            esc(n),
            esc(t),
            esc(v)
        ));
    }
    s.push_str("</ParameterList>");
    s
}

pub struct DeviceId {
    pub manufacturer: String,
    pub oui: String,
    pub product_class: String,
    pub serial_number: String,
}

pub fn inform(
    d: &DeviceId,
    events: &[(String, String)],
    params: &[(String, String, String)],
    retry: u32,
    now: &str,
) -> String {
    let mut ev = format!(
        "<Event soap-enc:arrayType=\"cwmp:EventStruct[{}]\">",
        events.len()
    );
    for (code, key) in events {
        ev.push_str(&format!(
            "<EventStruct><EventCode>{}</EventCode><CommandKey>{}</CommandKey></EventStruct>",
            esc(code),
            esc(key)
        ));
    }
    ev.push_str("</Event>");
    format!(
        "<cwmp:Inform><DeviceId><Manufacturer>{}</Manufacturer><OUI>{}</OUI><ProductClass>{}</ProductClass><SerialNumber>{}</SerialNumber></DeviceId>{ev}<MaxEnvelopes>1</MaxEnvelopes><CurrentTime>{}</CurrentTime><RetryCount>{retry}</RetryCount>{}</cwmp:Inform>",
        esc(&d.manufacturer), esc(&d.oui), esc(&d.product_class), esc(&d.serial_number), esc(now), value_list(params)
    )
}

pub fn inform_response() -> String {
    "<cwmp:InformResponse><MaxEnvelopes>1</MaxEnvelopes></cwmp:InformResponse>".into()
}

pub fn fault(code: u32, message: &str) -> String {
    format!(
        "<soap-env:Fault><faultcode>{}</faultcode><faultstring>CWMP fault</faultstring><detail><cwmp:Fault><FaultCode>{code}</FaultCode><FaultString>{}</FaultString></cwmp:Fault></detail></soap-env:Fault>",
        if (8000..9000).contains(&code) { "Server" } else { "Client" },
        esc(message)
    )
}

fn string_list(tag: &str, kind: &str, items: &[String]) -> String {
    let mut s = format!("<{tag} soap-enc:arrayType=\"{kind}[{}]\">", items.len());
    for i in items {
        s.push_str(&format!("<string>{}</string>", esc(i)));
    }
    s.push_str(&format!("</{tag}>"));
    s
}

pub fn get_parameter_values(names: &[String]) -> String {
    format!(
        "<cwmp:GetParameterValues>{}</cwmp:GetParameterValues>",
        string_list("ParameterNames", "xsd:string", names)
    )
}

pub fn set_parameter_values(params: &[(String, String, String)], key: &str) -> String {
    format!(
        "<cwmp:SetParameterValues>{}<ParameterKey>{}</ParameterKey></cwmp:SetParameterValues>",
        value_list(params),
        esc(key)
    )
}

pub fn get_parameter_names(path: &str, next_level: bool) -> String {
    format!("<cwmp:GetParameterNames><ParameterPath>{}</ParameterPath><NextLevel>{}</NextLevel></cwmp:GetParameterNames>", esc(path), u8::from(next_level))
}

pub fn add_object(object: &str, key: &str) -> String {
    format!("<cwmp:AddObject><ObjectName>{}</ObjectName><ParameterKey>{}</ParameterKey></cwmp:AddObject>", esc(object), esc(key))
}

pub fn delete_object(object: &str, key: &str) -> String {
    format!("<cwmp:DeleteObject><ObjectName>{}</ObjectName><ParameterKey>{}</ParameterKey></cwmp:DeleteObject>", esc(object), esc(key))
}

pub fn reboot(key: &str) -> String {
    format!(
        "<cwmp:Reboot><CommandKey>{}</CommandKey></cwmp:Reboot>",
        esc(key)
    )
}

pub fn factory_reset() -> String {
    "<cwmp:FactoryReset></cwmp:FactoryReset>".into()
}

pub fn get_parameter_values_response(params: &[(String, String, String)]) -> String {
    format!(
        "<cwmp:GetParameterValuesResponse>{}</cwmp:GetParameterValuesResponse>",
        value_list(params)
    )
}

pub fn get_parameter_names_response(params: &[(String, bool)]) -> String {
    let mut s = format!("<cwmp:GetParameterNamesResponse><ParameterList soap-enc:arrayType=\"cwmp:ParameterInfoStruct[{}]\">", params.len());
    for (n, w) in params {
        s.push_str(&format!(
            "<ParameterInfoStruct><Name>{}</Name><Writable>{}</Writable></ParameterInfoStruct>",
            esc(n),
            u8::from(*w)
        ));
    }
    s.push_str("</ParameterList></cwmp:GetParameterNamesResponse>");
    s
}

/// The empty-bodied response a CPE sends for methods that return only a status (or nothing).
pub fn status_response(method: &str, status: u32, instance: Option<u64>) -> String {
    match method {
        "SetParameterValues" => format!("<cwmp:SetParameterValuesResponse><Status>{status}</Status></cwmp:SetParameterValuesResponse>"),
        "AddObject" => format!(
            "<cwmp:AddObjectResponse><InstanceNumber>{}</InstanceNumber><Status>{status}</Status></cwmp:AddObjectResponse>",
            instance.unwrap_or(1)
        ),
        "DeleteObject" => format!("<cwmp:DeleteObjectResponse><Status>{status}</Status></cwmp:DeleteObjectResponse>"),
        other => format!("<cwmp:{other}Response></cwmp:{other}Response>"),
    }
}

pub fn get_rpc_methods_response(methods: &[&str]) -> String {
    let items: Vec<String> = methods.iter().map(|m| m.to_string()).collect();
    format!(
        "<cwmp:GetRPCMethodsResponse>{}</cwmp:GetRPCMethodsResponse>",
        string_list("MethodList", "xsd:string", &items)
    )
}

/// `{name: value}` (and optional `{name: type}`) from the model as value triples.
pub fn triples(values: &Value, types: Option<&Value>) -> Result<Vec<(String, String, String)>> {
    let o = values
        .as_object()
        .context("parameters must be an object of name: value")?;
    ensure!(
        o.len() <= MAX_PARAMETERS,
        "more than {MAX_PARAMETERS} parameters"
    );
    o.iter()
        .map(|(k, v)| {
            ensure!(
                !k.is_empty() && k.len() <= 256,
                "parameter names must be 1-256 bytes"
            );
            let value = match v {
                Value::String(s) => s.clone(),
                Value::Bool(b) => b.to_string(),
                Value::Number(n) => n.to_string(),
                _ => bail!("the value of {k} must be a string, number or boolean"),
            };
            let kind = types
                .and_then(|t| t[k].as_str())
                .map(str::to_string)
                .unwrap_or_else(|| match v {
                    Value::Bool(_) => "xsd:boolean".into(),
                    Value::Number(n) if n.is_u64() => "xsd:unsignedInt".into(),
                    Value::Number(_) => "xsd:int".into(),
                    _ => "xsd:string".into(),
                });
            ensure!(
                kind.starts_with("xsd:") && kind.len() <= 32,
                "a type is xsd:string, xsd:boolean, xsd:unsignedInt and the like"
            );
            Ok((k.clone(), value, kind))
        })
        .collect()
}
