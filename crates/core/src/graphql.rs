//! GraphQL: a request whose method is `GRAPHQL`, as JetBrains' HTTP client
//! writes it, sent as the JSON `POST` a GraphQL server expects.
//!
//! ```text
//! GRAPHQL https://countries.trevorblades.com/
//!
//! query Country($code: ID!) { country(code: $code) { name capital } }
//!
//! {"code": "UG"}
//! ```
//!
//! The body is the query, then, after a blank line, its variables as a JSON
//! object if it has any. Written that way it is kept that way, in the file,
//! the journal and history; only what goes out is JSON. Every GraphQL request
//! is also checked for `errors` in its answer, which servers send with a 200.

use serde_json::{Map, Value};
use tracer_protocol::{CheckResult, GqlField, GqlSchema, GqlType, HttpRequest, HttpResponse};

use crate::expect;

pub const METHOD: &str = "GRAPHQL";

/// The check every GraphQL request gets.
pub const NO_ERRORS: &str = "no GraphQL errors";

pub fn is_graphql(request: &HttpRequest) -> bool {
    request.method.eq_ignore_ascii_case(METHOD)
}

/// The query and, if the body ends with one, its variables object.
pub fn split(body: &str) -> (String, Option<Value>) {
    let body = body.trim();
    if let Some(at) = body.rfind("\n\n").or_else(|| body.rfind("\r\n\r\n")) {
        let (query, rest) = (body[..at].trim(), body[at..].trim());
        if let Ok(vars @ Value::Object(_)) = serde_json::from_str::<Value>(rest) {
            if !query.is_empty() {
                return (query.to_string(), Some(vars));
            }
        }
    }
    (body.to_string(), None)
}

/// Whether `query` changes something: a mutation, or a subscription, which
/// this client sends as one request and doesn't keep open.
pub fn is_write(query: &str) -> bool {
    let first = query
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#'))
        .unwrap_or("");
    first.starts_with("mutation") || first.starts_with("subscription")
}

/// `request`, its variables filled in, as it goes out: a JSON `POST`.
pub fn to_http(request: &HttpRequest) -> Result<HttpRequest, String> {
    let (query, variables) = split(request.body.as_deref().unwrap_or(""));
    if query.is_empty() {
        return Err("a GraphQL request needs a query in its body".into());
    }
    let mut payload = Map::new();
    payload.insert("query".into(), Value::String(query));
    if let Some(vars) = variables {
        payload.insert("variables".into(), vars);
    }
    let mut headers = request.headers.clone();
    for (name, value) in [("Content-Type", "application/json"), ("Accept", "application/json")] {
        if !headers.iter().any(|(k, _)| k.eq_ignore_ascii_case(name)) {
            headers.push((name.into(), value.into()));
        }
    }
    Ok(HttpRequest { method: "POST".into(), headers, body: Some(Value::Object(payload).to_string()), ..request.clone() })
}

/// `request`'s expectations checked against `response`, and for a GraphQL
/// request, whether the answer has errors.
pub fn checks(request: &HttpRequest, response: Result<&HttpResponse, &str>) -> Vec<CheckResult> {
    let mut out = expect::check(&request.expect, response);
    if is_graphql(request) {
        out.push(errors_check(response));
    }
    out
}

fn errors_check(response: Result<&HttpResponse, &str>) -> CheckResult {
    let (pass, actual) = match response {
        Err(why) => (false, format!("no answer: {why}")),
        Ok(r) => match serde_json::from_str::<Value>(&r.body) {
            Err(_) => (false, format!("status {}, and the body is not JSON", r.status)),
            Ok(json) => match json.get("errors").and_then(Value::as_array) {
                Some(errors) if !errors.is_empty() => {
                    let said: Vec<&str> = errors.iter().filter_map(|e| e.get("message")?.as_str()).take(3).collect();
                    let n = errors.len();
                    (false, format!("{n} error{}: {}", if n == 1 { "" } else { "s" }, said.join("; ")))
                }
                _ if json.get("data").is_some() => (true, "none".into()),
                _ => (false, "neither data nor errors in the answer".into()),
            },
        },
    };
    CheckResult { expect: NO_ERRORS.into(), pass, actual }
}

/// The schema in an answer to [`tracer_protocol::GRAPHQL_INTROSPECTION`], the server's own
/// `__` types left out.
pub fn schema(body: &str) -> Result<GqlSchema, String> {
    let json: Value = serde_json::from_str(body).map_err(|_| "the answer is not JSON".to_string())?;
    if let Some(errors) = json.get("errors").and_then(Value::as_array).filter(|e| !e.is_empty()) {
        let said: Vec<&str> = errors.iter().filter_map(|e| e.get("message")?.as_str()).take(3).collect();
        return Err(format!("the server refused to describe itself: {}", said.join("; ")));
    }
    let s = json.pointer("/data/__schema").ok_or("the answer has no data.__schema")?;
    let root = |key: &str| s.get(key).and_then(|t| t.get("name")).and_then(Value::as_str).unwrap_or("").to_string();
    let mut types = Vec::new();
    for t in s.get("types").and_then(Value::as_array).into_iter().flatten() {
        let name = t.get("name").and_then(Value::as_str).unwrap_or("");
        if name.is_empty() || name.starts_with("__") {
            continue;
        }
        let fields = t
            .get("fields")
            .and_then(Value::as_array)
            .or_else(|| t.get("inputFields").and_then(Value::as_array))
            .into_iter()
            .flatten()
            .map(|f| GqlField {
                name: text(f, "name"),
                args: f
                    .get("args")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .map(|a| format!("{}: {}", text(a, "name"), type_name(a.get("type"))))
                    .collect::<Vec<_>>()
                    .join(", "),
                ty: type_name(f.get("type")),
                description: text(f, "description"),
            })
            .collect();
        let names = |key: &str| -> Vec<String> {
            t.get(key).and_then(Value::as_array).into_iter().flatten().map(|v| text(v, "name")).collect()
        };
        let mut values = names("enumValues");
        values.extend(names("possibleTypes"));
        types.push(GqlType {
            name: name.to_string(),
            kind: text(t, "kind"),
            description: text(t, "description"),
            fields,
            values,
        });
    }
    Ok(GqlSchema { query: root("queryType"), mutation: root("mutationType"), subscription: root("subscriptionType"), types })
}

fn text(v: &Value, key: &str) -> String {
    v.get(key).and_then(Value::as_str).unwrap_or("").to_string()
}

/// `[User!]!` from introspection's nested `ofType`s.
fn type_name(t: Option<&Value>) -> String {
    let Some(t) = t else { return "?".into() };
    match t.get("kind").and_then(Value::as_str) {
        Some("NON_NULL") => format!("{}!", type_name(t.get("ofType"))),
        Some("LIST") => format!("[{}]", type_name(t.get("ofType"))),
        _ => t.get("name").and_then(Value::as_str).unwrap_or("?").to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tracer_protocol::Timing;

    fn gql(body: &str) -> HttpRequest {
        HttpRequest { method: "GRAPHQL".into(), url: "http://x/graphql".into(), body: Some(body.into()), ..Default::default() }
    }

    #[test]
    fn a_body_is_its_query_then_its_variables() {
        let (q, v) = split("query A($id: ID!) {\n  user(id: $id) { name }\n}\n\n{\"id\": \"7\"}\n");
        assert_eq!(q, "query A($id: ID!) {\n  user(id: $id) { name }\n}");
        assert_eq!(v, Some(json!({"id": "7"})));
        // A query that starts with `{` is no variables object.
        assert_eq!(split("{ users { id } }"), ("{ users { id } }".into(), None));
        assert_eq!(split("{ a }\n\n{ b }").1, None, "`{{ b }}` is not JSON");

        let sent = to_http(&gql("{ a }\n\n{\"x\": 1}")).unwrap();
        assert_eq!(sent.method, "POST");
        assert_eq!(serde_json::from_str::<Value>(sent.body.as_deref().unwrap()).unwrap(), json!({"query": "{ a }", "variables": {"x": 1}}));
        assert!(sent.headers.iter().any(|(k, v)| k == "Content-Type" && v == "application/json"));
        assert!(to_http(&gql("  ")).is_err());
    }

    #[test]
    fn a_mutation_writes_and_a_query_reads() {
        assert!(is_write("# make one\nmutation { add(x: 1) { id } }"));
        assert!(!is_write("query { a }"));
        assert!(!is_write("{ a }"));
    }

    #[test]
    fn errors_in_a_200_fail_the_check() {
        let answer = |body: &str| HttpResponse {
            status: 200,
            headers: vec![],
            body: body.into(),
            size: 0,
            timing: Timing { ttfb_ms: 1.0, total_ms: 1.0, dns_ms: None, connect_ms: None, tls_ms: None },
        };
        let r = gql("{ a }");
        let ok = checks(&r, Ok(&answer(r#"{"data":{"a":1}}"#)));
        assert_eq!((ok.len(), ok[0].pass), (1, true));
        let bad = checks(&r, Ok(&answer(r#"{"data":null,"errors":[{"message":"no field a"}]}"#)));
        assert_eq!((bad[0].pass, bad[0].actual.as_str()), (false, "1 error: no field a"));
        assert!(!checks(&r, Ok(&answer("<html>")))[0].pass);
        let rest = HttpRequest { method: "GET".into(), ..r };
        assert!(checks(&rest, Ok(&answer("<html>"))).is_empty(), "only GraphQL requests get it");
    }

    #[test]
    fn a_schema_is_read_from_introspection() {
        let body = json!({"data": {"__schema": {
            "queryType": {"name": "Query"}, "mutationType": null, "subscriptionType": null,
            "types": [
                {"kind": "OBJECT", "name": "Query", "description": null, "fields": [
                    {"name": "country", "description": "One", "args": [{"name": "code", "type": {"kind": "NON_NULL", "name": null, "ofType": {"kind": "SCALAR", "name": "ID"}}}],
                     "type": {"kind": "OBJECT", "name": "Country"}},
                    {"name": "countries", "description": null, "args": [],
                     "type": {"kind": "NON_NULL", "name": null, "ofType": {"kind": "LIST", "name": null, "ofType": {"kind": "NON_NULL", "name": null, "ofType": {"kind": "OBJECT", "name": "Country"}}}}}
                ], "inputFields": null, "enumValues": null, "possibleTypes": null},
                {"kind": "ENUM", "name": "Size", "description": null, "fields": null, "inputFields": null,
                 "enumValues": [{"name": "S"}, {"name": "L"}], "possibleTypes": null},
                {"kind": "OBJECT", "name": "__Type", "description": null, "fields": [], "inputFields": null, "enumValues": null, "possibleTypes": null}
            ]}}});
        let s = schema(&body.to_string()).unwrap();
        assert_eq!((s.query.as_str(), s.mutation.as_str(), s.types.len()), ("Query", "", 2));
        let q = &s.types[0];
        assert_eq!((q.fields[0].args.as_str(), q.fields[0].ty.as_str()), ("code: ID!", "Country"));
        assert_eq!(q.fields[1].ty, "[Country!]!");
        assert_eq!(s.types[1].values, vec!["S", "L"]);
        assert!(s.to_sdl().contains("type Query {\n  \"One\"\n  country(code: ID!): Country\n  countries: [Country!]!\n}"));
        assert!(schema(r#"{"errors":[{"message":"introspection is off"}]}"#).unwrap_err().contains("introspection is off"));
    }
}
