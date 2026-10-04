//! GraphQL documents, schema, introspection and execution on apollo-compiler. Rust parses and
//! validates every request against the configured SDL, answers introspection itself and runs
//! the spec's execution (result coercion, null propagation, error paths) over the data the
//! handler supplies, so the response always matches the query that was asked.
use apollo_compiler::{
    ast,
    executable::{self, OperationType, Selection},
    parser::Parser,
    request::coerce_variable_values,
    resolvers::{Execution, FieldError, ObjectValue, ResolveInfo, ResolvedValue},
    response::{ExecutionResponse, GraphQLError, JsonMap, JsonValue},
    schema::ExtendedType,
    validation::Valid,
    ExecutableDocument, Schema,
};
use serde_json::{json, Map, Value};

pub const MAX_BODY_BYTES: usize = 1024 * 1024;
pub const MAX_QUERY_BYTES: usize = 64 * 1024;
pub const MAX_SCHEMA_BYTES: usize = 256 * 1024;
/// Parser nesting bound: selection sets, lists and input objects all count toward it.
pub const RECURSION_LIMIT: usize = 64;
pub const TOKEN_LIMIT: usize = 15_000;
const MAX_SHAPE_NODES: usize = 2_000;
const MAX_ERRORS: usize = 64;

pub fn budget_ok(v: &Value) -> bool {
    crate::utils::json_budget::within_budget(v, MAX_BODY_BYTES, 50_000, 48)
}

pub fn load_schema(sdl: &str) -> anyhow::Result<Valid<Schema>> {
    anyhow::ensure!(
        !sdl.trim().is_empty() && sdl.len() <= MAX_SCHEMA_BYTES,
        "schema must be 1 byte..256 KiB of SDL"
    );
    let schema = Parser::new()
        .recursion_limit(RECURSION_LIMIT)
        .parse_schema(sdl, "schema.graphql")
        .map_err(|e| anyhow::anyhow!("schema does not parse: {}", first_message(&e.errors)))?;
    let schema = schema
        .validate()
        .map_err(|e| anyhow::anyhow!("schema is invalid: {}", first_message(&e.errors)))?;
    anyhow::ensure!(
        schema.schema_definition.query.is_some(),
        "schema has no Query type"
    );
    Ok(schema)
}

fn first_message(list: &apollo_compiler::validation::DiagnosticList) -> String {
    list.iter()
        .next()
        .map(|d| d.error.to_string())
        .unwrap_or_else(|| "unknown error".into())
}

/// A request error: no `data` key, a list of errors with locations where known.
#[derive(Debug)]
pub struct RequestErrors(pub Vec<GraphQLError>);

impl RequestErrors {
    pub fn one(message: impl Into<String>) -> Self {
        Self(vec![GraphQLError {
            message: message.into(),
            locations: vec![],
            path: vec![],
            extensions: JsonMap::new(),
        }])
    }
    pub fn body(&self) -> Value {
        json!({"errors": self.0})
    }
}

/// A parsed, validated request ready to execute.
pub struct Prepared {
    pub document: Valid<ExecutableDocument>,
    pub operation_name: Option<String>,
    pub operation_type: OperationType,
    pub variables: Valid<JsonMap>,
}

/// Parse, validate, pick the operation and coerce variables — every request error in the spec's
/// order (parse → validate → operation → variables).
pub fn prepare(
    schema: &Valid<Schema>,
    query: &str,
    operation_name: Option<&str>,
    variables: &Map<String, Value>,
) -> Result<Prepared, RequestErrors> {
    if query.len() > MAX_QUERY_BYTES {
        return Err(RequestErrors::one("query exceeds 64 KiB"));
    }
    let diagnostics = |list: &apollo_compiler::validation::DiagnosticList| {
        RequestErrors(list.iter().take(MAX_ERRORS).map(|d| d.to_json()).collect())
    };
    let document = Parser::new()
        .recursion_limit(RECURSION_LIMIT)
        .token_limit(TOKEN_LIMIT)
        .parse_executable(schema, query, "request.graphql")
        .map_err(|e| diagnostics(&e.errors))?;
    let document = document
        .validate(schema)
        .map_err(|e| diagnostics(&e.errors))?;
    let operation = document
        .operations
        .get(operation_name)
        .map_err(|_| match operation_name {
            Some(n) => RequestErrors::one(format!("Unknown operation named \"{n}\".")),
            None => RequestErrors::one(
                "Must provide operation name if query contains multiple operations.",
            ),
        })?;
    let raw: JsonMap = serde_json::from_value(Value::Object(variables.clone()))
        .map_err(|_| RequestErrors::one("variables must be a JSON object"))?;
    let coerced = coerce_variable_values(schema, operation, &raw)
        .map_err(|e| RequestErrors(vec![e.to_graphql_error(&document.sources)]))?;
    let operation_type = operation.operation_type;
    let operation_name = operation.name.as_ref().map(|n| n.to_string());
    Ok(Prepared {
        document,
        operation_name,
        operation_type,
        variables: coerced,
    })
}

impl Prepared {
    pub fn operation(&self) -> &executable::Operation {
        self.document
            .operations
            .get(self.operation_name.as_deref())
            .expect("operation was selected in prepare")
    }

    /// Only `__schema`, `__type` and `__typename` at the root: Rust answers it alone.
    pub fn is_introspection(&self) -> bool {
        self.operation().is_introspection(&self.document)
    }

    pub fn variables_json(&self) -> Value {
        serde_json::to_value(&*self.variables).unwrap_or(json!({}))
    }

    /// Root fields as `{response_key, field, arguments}` with variables substituted.
    pub fn root_fields(&self) -> Vec<Value> {
        let vars = self.variables_json();
        let mut out = Vec::new();
        collect_fields(
            &self.document,
            &self.operation().selection_set,
            &mut |f: &executable::Field| {
                if f.name.starts_with("__") {
                    return;
                }
                let args: Map<String, Value> = f
                    .arguments
                    .iter()
                    .map(|a| (a.name.to_string(), ast_to_json(&a.value, &vars)))
                    .collect();
                out.push(json!({"response_key": f.response_key().as_str(), "field": f.name.as_str(), "arguments": args}));
            },
        );
        out
    }

    /// The data the handler must supply, as a skeleton: response keys mapped to their GraphQL
    /// type (`"String!"`) for leaves, to nested skeletons for objects, wrapped in `[...]` for lists.
    pub fn shape(&self, schema: &Valid<Schema>) -> Value {
        let mut budget = MAX_SHAPE_NODES;
        shape_of(
            schema,
            &self.document,
            &self.operation().selection_set,
            &mut budget,
        )
    }

    /// Run the operation over `data` (keyed by response key). Field errors land in the
    /// response with paths; a type mismatch in `data` is a field error, never a crash.
    pub fn execute(
        &self,
        schema: &Valid<Schema>,
        data: &Value,
        introspection: bool,
    ) -> anyhow::Result<ExecutionResponse> {
        let data: JsonValue = serde_json::from_value(data.clone())?;
        let empty = JsonMap::new();
        let root = DataObject {
            schema,
            type_name: self.operation().object_type().to_string(),
            map: data.as_object().unwrap_or(&empty),
        };
        Execution::new(schema, &self.document)
            .operation(self.operation())
            .coerced_variable_values(&self.variables)
            .enable_schema_introspection(introspection)
            .execute_sync(&root)
            .map_err(|e| anyhow::anyhow!("{}", e.message()))
    }
}

fn collect_fields<'a>(
    document: &'a ExecutableDocument,
    set: &'a executable::SelectionSet,
    f: &mut dyn FnMut(&'a executable::Field),
) {
    for s in &set.selections {
        match s {
            Selection::Field(field) => f(field),
            Selection::InlineFragment(i) => collect_fields(document, &i.selection_set, f),
            Selection::FragmentSpread(sp) => {
                if let Some(frag) = document.fragments.get(&sp.fragment_name) {
                    collect_fields(document, &frag.selection_set, f)
                }
            }
        }
    }
}

fn shape_of(
    schema: &Valid<Schema>,
    document: &ExecutableDocument,
    set: &executable::SelectionSet,
    budget: &mut usize,
) -> Value {
    let mut out = Map::new();
    if matches!(
        schema.types.get(&set.ty),
        Some(ExtendedType::Interface(_) | ExtendedType::Union(_))
    ) {
        let mut names: Vec<String> = schema
            .types
            .iter()
            .filter(|(n, t)| t.is_object() && schema.is_subtype(&set.ty, n))
            .map(|(n, _)| n.to_string())
            .collect();
        names.sort();
        out.insert(
            "__typename".into(),
            json!(format!("one of {}", names.join("|"))),
        );
    }
    collect_fields(document, set, &mut |f: &executable::Field| {
        if *budget == 0 {
            return;
        }
        *budget -= 1;
        let key = f.response_key().to_string();
        if f.name == "__typename" {
            out.entry(key).or_insert(json!("String!"));
            return;
        }
        let value = if f.selection_set.selections.is_empty() {
            json!(f.definition.ty.to_string())
        } else {
            let inner = shape_of(schema, document, &f.selection_set, budget);
            let mut ty = &f.definition.ty;
            let mut v = inner;
            let mut depth = Vec::new();
            while let Some(item) = list_item(ty) {
                depth.push(());
                ty = item;
            }
            for _ in depth {
                v = json!([v]);
            }
            v
        };
        match (out.get_mut(&key), value) {
            (Some(Value::Object(existing)), Value::Object(more)) => existing.extend(more),
            (_, value) => {
                out.insert(key, value);
            }
        }
    });
    Value::Object(out)
}

fn list_item(ty: &ast::Type) -> Option<&ast::Type> {
    match ty {
        ast::Type::List(inner) | ast::Type::NonNullList(inner) => Some(inner),
        _ => None,
    }
}

pub fn ast_to_json(v: &ast::Value, vars: &Value) -> Value {
    match v {
        ast::Value::Null => Value::Null,
        ast::Value::Enum(n) => json!(n.as_str()),
        ast::Value::Variable(n) => vars.get(n.as_str()).cloned().unwrap_or(Value::Null),
        ast::Value::String(s) => json!(s),
        ast::Value::Float(f) => f.try_to_f64().map(|x| json!(x)).unwrap_or(Value::Null),
        ast::Value::Int(i) => i
            .as_str()
            .parse::<i64>()
            .map(|x| json!(x))
            .unwrap_or(Value::Null),
        ast::Value::Boolean(b) => json!(b),
        ast::Value::List(items) => {
            Value::Array(items.iter().map(|x| ast_to_json(x, vars)).collect())
        }
        ast::Value::Object(fields) => Value::Object(
            fields
                .iter()
                .map(|(k, x)| (k.to_string(), ast_to_json(x, vars)))
                .collect(),
        ),
    }
}

/// An object in the handler's data. Fields are looked up by response key (so aliases with
/// different arguments get different values), then by field name.
struct DataObject<'a> {
    schema: &'a Valid<Schema>,
    type_name: String,
    map: &'a JsonMap,
}

impl ObjectValue for DataObject<'_> {
    fn type_name(&self) -> &str {
        &self.type_name
    }

    fn resolve_field<'b>(
        &'b self,
        info: &'b ResolveInfo<'b>,
    ) -> Result<ResolvedValue<'b>, FieldError> {
        let field = &info.field_selections()[0];
        let value = self
            .map
            .get(field.response_key().as_str())
            .or_else(|| self.map.get(field.name.as_str()));
        match value {
            None => Ok(ResolvedValue::null()),
            Some(v) => resolve(self.schema, &info.field_definition().ty, v),
        }
    }
}

fn error(message: impl Into<String>) -> FieldError {
    FieldError {
        message: message.into(),
    }
}

fn resolve<'a>(
    schema: &'a Valid<Schema>,
    ty: &'a ast::Type,
    v: &'a JsonValue,
) -> Result<ResolvedValue<'a>, FieldError> {
    if v.is_null() {
        return Ok(ResolvedValue::null());
    }
    if let Some(item) = list_item(ty) {
        let items = v
            .as_array()
            .ok_or_else(|| error(format!("handler data is not a list for {ty}")))?;
        return Ok(ResolvedValue::List(Box::new(
            items.iter().map(move |x| resolve(schema, item, x)),
        )));
    }
    let name = ty.inner_named_type();
    match schema.types.get(name) {
        Some(ExtendedType::Scalar(_) | ExtendedType::Enum(_)) => Ok(ResolvedValue::Leaf(v.clone())),
        Some(ExtendedType::Object(_)) => Ok(ResolvedValue::object(DataObject {
            schema,
            type_name: name.to_string(),
            map: v
                .as_object()
                .ok_or_else(|| error(format!("handler data is not an object for {name}")))?,
        })),
        Some(ExtendedType::Interface(_) | ExtendedType::Union(_)) => {
            let map = v
                .as_object()
                .ok_or_else(|| error(format!("handler data is not an object for {name}")))?;
            let candidates: Vec<&str> = schema
                .types
                .iter()
                .filter(|(n, t)| t.is_object() && schema.is_subtype(name, n))
                .map(|(n, _)| n.as_str())
                .collect();
            let concrete = match map.get("__typename").and_then(|t| t.as_str()) {
                Some(t) if candidates.contains(&t) => t.to_owned(),
                Some(t) => {
                    return Err(error(format!(
                        "__typename {t} is not a possible type of {name}"
                    )))
                }
                None if candidates.len() == 1 => candidates[0].to_owned(),
                None => {
                    return Err(error(format!(
                        "handler data for abstract type {name} needs __typename (one of {})",
                        candidates.join("|")
                    )))
                }
            };
            Ok(ResolvedValue::object(DataObject {
                schema,
                type_name: concrete,
                map,
            }))
        }
        _ => Err(error(format!("{name} is not an output type"))),
    }
}

/// Handler errors appended to the response: `message` required, `path` optional (strings and
/// non-negative integers), `extensions` optional.
pub fn handler_errors(list: Option<&Value>) -> anyhow::Result<Vec<GraphQLError>> {
    let Some(list) = list.filter(|l| !l.is_null()) else {
        return Ok(vec![]);
    };
    let items = list
        .as_array()
        .ok_or_else(|| anyhow::anyhow!("errors must be an array"))?;
    anyhow::ensure!(items.len() <= MAX_ERRORS, "at most {MAX_ERRORS} errors");
    items
        .iter()
        .map(|e| {
            let message = e["message"]
                .as_str()
                .filter(|m| !m.is_empty() && m.len() <= 4096)
                .ok_or_else(|| anyhow::anyhow!("each error needs a message of 1..4096 bytes"))?;
            let mut out = json!({"message": message});
            if let Some(path) = e.get("path").filter(|p| !p.is_null()) {
                let ok = path.as_array().is_some_and(|p| {
                    p.len() <= 64 && p.iter().all(|s| s.is_string() || s.is_u64())
                });
                anyhow::ensure!(ok, "error path must be an array of field names and indexes");
                out["path"] = path.clone();
            }
            if let Some(ext) = e.get("extensions").filter(|x| !x.is_null()) {
                anyhow::ensure!(ext.is_object(), "error extensions must be an object");
                out["extensions"] = ext.clone();
            }
            Ok(serde_json::from_value(out)?)
        })
        .collect()
}

/// Syntax-only parse for the client: operation type and name of the selected operation.
pub fn parse_operation(
    query: &str,
    operation_name: Option<&str>,
) -> anyhow::Result<(OperationType, Option<String>)> {
    anyhow::ensure!(
        !query.trim().is_empty() && query.len() <= MAX_QUERY_BYTES,
        "query must be 1 byte..64 KiB"
    );
    let doc = Parser::new()
        .recursion_limit(RECURSION_LIMIT)
        .token_limit(TOKEN_LIMIT)
        .parse_ast(query, "request.graphql")
        .map_err(|e| anyhow::anyhow!("query does not parse: {}", first_message(&e.errors)))?;
    let ops: Vec<&ast::OperationDefinition> = doc
        .definitions
        .iter()
        .filter_map(|d| match d {
            ast::Definition::OperationDefinition(o) => Some(&**o),
            _ => None,
        })
        .collect();
    let op = match operation_name {
        Some(n) => ops
            .iter()
            .find(|o| o.name.as_ref().is_some_and(|x| x == n))
            .ok_or_else(|| anyhow::anyhow!("no operation named {n}"))?,
        None => {
            anyhow::ensure!(
                ops.len() == 1,
                "name the operation to run (the document has {})",
                ops.len()
            );
            &ops[0]
        }
    };
    Ok((op.operation_type, op.name.as_ref().map(|n| n.to_string())))
}

pub fn operation_type_name(t: OperationType) -> &'static str {
    match t {
        OperationType::Query => "query",
        OperationType::Mutation => "mutation",
        OperationType::Subscription => "subscription",
    }
}

/// A GraphQL response body as the client sees it: `data` and/or `errors`, each well formed.
pub fn check_response(body: &Value) -> anyhow::Result<()> {
    let obj = body
        .as_object()
        .ok_or_else(|| anyhow::anyhow!("response is not a JSON object"))?;
    anyhow::ensure!(
        obj.contains_key("data") || obj.contains_key("errors"),
        "response has neither data nor errors"
    );
    if let Some(d) = obj.get("data") {
        anyhow::ensure!(
            d.is_null() || d.is_object(),
            "data must be an object or null"
        );
    }
    if let Some(errors) = obj.get("errors") {
        let list = errors
            .as_array()
            .filter(|l| !l.is_empty())
            .ok_or_else(|| anyhow::anyhow!("errors must be a non-empty array"))?;
        anyhow::ensure!(
            list.iter().all(|e| e["message"].is_string()),
            "every error needs a message"
        );
    } else {
        anyhow::ensure!(!obj["data"].is_null(), "data is null without errors");
    }
    if let Some(x) = obj.get("extensions") {
        anyhow::ensure!(x.is_object(), "extensions must be an object");
    }
    Ok(())
}

/// Introspection the client runs on connect: root types and their fields with argument and
/// return types, enough for a handler to know what it can ask.
pub const CLIENT_INTROSPECTION: &str = "query NetGetIntrospection { __schema { queryType { name } mutationType { name } subscriptionType { name } types { kind name fields { name type { ...T } args { name type { ...T } } } } } } fragment T on __Type { kind name ofType { kind name ofType { kind name ofType { kind name } } } }";

fn type_ref(t: &Value) -> String {
    match t["kind"].as_str() {
        Some("NON_NULL") => format!("{}!", type_ref(&t["ofType"])),
        Some("LIST") => format!("[{}]", type_ref(&t["ofType"])),
        _ => t["name"].as_str().unwrap_or("?").to_owned(),
    }
}

/// Summarise an introspection answer as root-field signatures (`book(id: ID!): Book`).
pub fn root_signatures(data: &Value) -> Value {
    let schema = &data["__schema"];
    let types = schema["types"].as_array().cloned().unwrap_or_default();
    let fields_of = |root: &str| -> Vec<Value> {
        let Some(name) = schema[root]["name"].as_str() else {
            return vec![];
        };
        types
            .iter()
            .find(|t| t["name"] == name)
            .and_then(|t| t["fields"].as_array())
            .map(|fs| {
                fs.iter()
                    .take(256)
                    .map(|f| {
                        let args: Vec<String> = f["args"]
                            .as_array()
                            .map(|a| {
                                a.iter()
                                    .map(|x| {
                                        format!(
                                            "{}: {}",
                                            x["name"].as_str().unwrap_or("?"),
                                            type_ref(&x["type"])
                                        )
                                    })
                                    .collect()
                            })
                            .unwrap_or_default();
                        let args = if args.is_empty() {
                            String::new()
                        } else {
                            format!("({})", args.join(", "))
                        };
                        json!(format!(
                            "{}{args}: {}",
                            f["name"].as_str().unwrap_or("?"),
                            type_ref(&f["type"])
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default()
    };
    json!({"query": fields_of("queryType"), "mutation": fields_of("mutationType"), "subscription": fields_of("subscriptionType")})
}
